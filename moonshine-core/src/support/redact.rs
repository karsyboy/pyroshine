//! Redaction of support-bundle text.
//!
//! A bundle is meant to be attached to a public issue, so everything in it
//! passes through one [`Redactor`]:
//!
//! - known identifying values (home directory, user and host name, the
//!   configured server name, the server ID, client IDs and names) become
//!   placeholders such as `~`, `<user>` or `<client-name-1>`;
//! - certificate fingerprints and UUIDs become numbered placeholders;
//! - IP addresses become `<lan-ip-N>` (private, link-local and CGNAT ranges)
//!   or `<ip-N>`, consistently across the bundle, so a reader can still tell
//!   which lines name the same peer. Loopback, unspecified and multicast
//!   addresses are kept: they identify nothing and explain bind errors.
//!
//! The configuration is redacted by setting: see [`Redactor::config`].

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use moonshine_management::dto::FieldKind;
use regex::{Captures, Regex};

/// Names too generic to identify anyone; replacing them would mangle ordinary
/// text ("Steam Deck", "root cause", the default server name "Pyroshine").
const GENERIC_NAMES: &[&str] = &[
	"admin",
	"arch",
	"deck",
	"fedora",
	"game",
	"gamer",
	"games",
	"guest",
	"localhost",
	"moonshine",
	"nixos",
	"pyroshine",
	"root",
	"steam",
	"test",
	"ubuntu",
	"user",
];

static FINGERPRINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{64}\b").unwrap());
static UUID: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b").unwrap()
});
static IPV6: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9A-Fa-f:.]*:[0-9A-Fa-f:.]*").unwrap());
static IPV4: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{1,3}(?:\.\d{1,3}){3}").unwrap());

struct Literal {
	value: String,
	placeholder: String,
	/// Replace only where the value is not part of a longer word.
	word: bool,
}

pub(crate) struct Redactor {
	literals: Vec<Literal>,
	pseudonyms: HashMap<String, String>,
	counters: HashMap<&'static str, usize>,
}

impl Redactor {
	pub(crate) fn new() -> Self {
		Self {
			literals: Vec::new(),
			pseudonyms: HashMap::new(),
			counters: HashMap::new(),
		}
	}

	/// A redactor for this host's home directory, user name and host name.
	pub(crate) fn for_host() -> Self {
		let mut redactor = Self::new();
		if let Some(home) = dirs::home_dir() {
			redactor.path(&home.to_string_lossy(), "~");
		}
		// SAFETY: getuid has no preconditions; getpwuid returns a pointer to
		// static storage or null, read before any other passwd call.
		let account = unsafe {
			let entry = libc::getpwuid(libc::getuid());
			(!entry.is_null()).then(|| {
				std::ffi::CStr::from_ptr((*entry).pw_name)
					.to_string_lossy()
					.into_owned()
			})
		};
		if let Some(user) = account.or_else(|| std::env::var("USER").ok()) {
			redactor.word(&user, "<user>");
		}
		if let Ok(host) = gethostname::gethostname().into_string() {
			redactor.word(&host, "<host>");
		}
		redactor
	}

	/// Replace `value` wherever it appears.
	pub(crate) fn path(&mut self, value: &str, placeholder: &str) {
		self.add(value, placeholder, false);
	}

	/// Replace `value` where it is a whole word.
	pub(crate) fn word(&mut self, value: &str, placeholder: &str) {
		self.add(value, placeholder, true);
	}

	fn add(&mut self, value: &str, placeholder: &str, word: bool) {
		let value = value.trim();
		if value.chars().count() < 3 || GENERIC_NAMES.contains(&value.to_ascii_lowercase().as_str()) {
			return;
		}
		if self.literals.iter().any(|literal| literal.value == value) {
			return;
		}
		self.literals.push(Literal {
			value: value.to_string(),
			placeholder: placeholder.to_string(),
			word,
		});
		// Longest first, so a home directory wins over the user name in it.
		self.literals
			.sort_by_key(|literal| std::cmp::Reverse(literal.value.len()));
	}

	fn pseudonym(&mut self, kind: &'static str, value: &str) -> String {
		if let Some(name) = self.pseudonyms.get(value) {
			return name.clone();
		}
		let counter = self.counters.entry(kind).or_default();
		*counter += 1;
		let name = format!("<{kind}-{counter}>");
		self.pseudonyms.insert(value.to_string(), name.clone());
		name
	}

	pub(crate) fn text(&mut self, text: &str) -> String {
		let mut text = text.to_string();
		for literal in &self.literals {
			text = replace_literal(&text, literal);
		}
		let text = self.replace(&FINGERPRINT, &text, |redactor, found| {
			Some(redactor.pseudonym("fingerprint", &found.to_ascii_lowercase()))
		});
		let text = self.replace(&UUID, &text, |redactor, found| {
			Some(redactor.pseudonym("uuid", &found.to_ascii_lowercase()))
		});
		let text = self.replace(&IPV6, &text, |redactor, found| {
			let address: Ipv6Addr = found.parse().ok()?;
			redactor.address(IpAddr::V6(address))
		});
		let text = self.replace(&IPV4, &text, |redactor, found| {
			let address: Ipv4Addr = found.parse().ok()?;
			redactor.address(IpAddr::V4(address))
		});
		restore_versions(&text, &self.pseudonyms)
	}

	/// Replace whole-token matches of `pattern` for which `with` returns a value.
	fn replace(
		&mut self,
		pattern: &Regex,
		text: &str,
		mut with: impl FnMut(&mut Self, &str) -> Option<String>,
	) -> String {
		pattern
			.replace_all(text, |captures: &Captures| {
				let found = captures.get(0).unwrap();
				let before = text[..found.start()].chars().next_back();
				let after = text[found.end()..].chars().next();
				// Part of a longer word, version string or Rust path (`core::web`).
				let attached = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
				let dotted =
					|c: Option<char>, next: Option<char>| c == Some('.') && next.is_some_and(|c| c.is_ascii_digit());
				let next_after = text[found.end()..].chars().nth(1);
				let previous = text[..found.start()].chars().rev().nth(1);
				if attached(before) || attached(after) || dotted(after, next_after) || dotted(before, previous) {
					return found.as_str().to_string();
				}
				with(self, found.as_str()).unwrap_or_else(|| found.as_str().to_string())
			})
			.into_owned()
	}

	fn address(&mut self, address: IpAddr) -> Option<String> {
		let address = match address {
			IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
			v4 => v4,
		};
		if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
			return None;
		}
		let local = match address {
			IpAddr::V4(v4) => {
				let [a, b, ..] = v4.octets();
				v4.is_private() || v4.is_link_local() || v4.is_broadcast() || (a == 100 && (64..128).contains(&b))
			},
			IpAddr::V6(v6) => {
				let first = v6.segments()[0];
				first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
			},
		};
		Some(self.pseudonym(if local { "lan-ip" } else { "ip" }, &address.to_string()))
	}

	/// Redact every string in a JSON document.
	pub(crate) fn json(&mut self, value: &mut serde_json::Value) {
		match value {
			serde_json::Value::String(text) => *text = self.text(text),
			serde_json::Value::Array(items) => items.iter_mut().for_each(|item| self.json(item)),
			serde_json::Value::Object(map) => map.values_mut().for_each(|item| self.json(item)),
			_ => {},
		}
	}

	/// A redacted copy of `config.toml`, setting by setting. `name` (the
	/// server name clients show) is replaced and remembered so that it is
	/// also replaced in other files; every other known setting keeps its
	/// value with paths and addresses redacted. Settings the schema does not
	/// know (misspelled or obsolete keys) are shown as `<redacted>`.
	pub(crate) fn config(&mut self, text: &str) -> String {
		let mut value: toml::Table = match toml::from_str(text) {
			Ok(value) => value,
			Err(error) => {
				let message = self.text(&error.message().replace('\n', " "));
				return format!("# The configuration could not be parsed: {message}\n");
			},
		};
		if let Some(toml::Value::String(name)) = value.get("name") {
			let name = name.clone();
			self.word(&name, "<server-name>");
		}
		let known = known_settings();
		self.config_table(&mut value, "", &known);
		toml::to_string_pretty(&value).unwrap_or_default()
	}

	fn config_table(&mut self, table: &mut toml::Table, prefix: &str, known: &BTreeSet<String>) {
		for (key, value) in table.iter_mut() {
			let path = if prefix.is_empty() {
				key.clone()
			} else {
				format!("{prefix}.{key}")
			};
			self.config_value(value, &path, known);
		}
	}

	fn config_value(&mut self, value: &mut toml::Value, path: &str, known: &BTreeSet<String>) {
		match value {
			toml::Value::Table(table) => self.config_table(table, path, known),
			toml::Value::Array(items) => {
				for item in items {
					self.config_value(item, path, known);
				}
			},
			toml::Value::String(text) => {
				*text = if !known.contains(path) {
					"<redacted>".into()
				} else if path == "name" {
					"<server-name>".into()
				} else {
					self.text(text)
				};
			},
			_ => {},
		}
	}
}

/// Every setting path of the configuration editor's schema, which covers
/// exactly the settings `Config` serializes (its tests enforce this). List
/// entries use the list's path as prefix (`application.title`).
fn known_settings() -> BTreeSet<String> {
	let schema = crate::management::schema::config_schema();
	let mut known = BTreeSet::new();
	for field in schema.fields {
		match field.kind {
			FieldKind::Applications { item } => {
				known.extend(item.into_iter().map(|item| format!("{}.{}", field.path, item.path)));
			},
			FieldKind::Scanners { variants } => {
				known.insert(format!("{}.type", field.path));
				for variant in variants {
					known.extend(
						variant
							.fields
							.into_iter()
							.map(|item| format!("{}.{}", field.path, item.path)),
					);
				}
			},
			_ => {
				known.insert(field.path);
			},
		}
	}
	known
}

/// Four-part version numbers look like IPv4 addresses (`conformanceVersion =
/// 1.4.5.3`). On a line that names a version, put them back.
fn restore_versions(text: &str, pseudonyms: &HashMap<String, String>) -> String {
	if !text.to_ascii_lowercase().contains("version") {
		return text.to_string();
	}
	text.split_inclusive('\n')
		.map(|line| {
			let lower = line.to_ascii_lowercase();
			let Some(at) = lower.find("version") else {
				return line.to_string();
			};
			let (head, tail) = line.split_at(at);
			let mut tail = tail.to_string();
			for (value, name) in pseudonyms {
				if name.starts_with("<ip-") || name.starts_with("<lan-ip-") {
					tail = tail.replace(name.as_str(), value);
				}
			}
			format!("{head}{tail}")
		})
		.collect()
}

fn replace_literal(text: &str, literal: &Literal) -> String {
	let mut result = String::with_capacity(text.len());
	let mut rest = text;
	while let Some(index) = rest.find(&literal.value) {
		let end = index + literal.value.len();
		let attached = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
		let whole =
			!literal.word || (!attached(rest[..index].chars().next_back()) && !attached(rest[end..].chars().next()));
		result.push_str(&rest[..index]);
		result.push_str(if whole { &literal.placeholder } else { &literal.value });
		rest = &rest[end..];
	}
	result.push_str(rest);
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	fn redactor() -> Redactor {
		let mut redactor = Redactor::new();
		redactor.path("/home/alice", "~");
		redactor.word("alice", "<user>");
		redactor.word("alice-desktop", "<host>");
		redactor
	}

	#[test]
	fn identifying_values_are_replaced_where_they_are_words() {
		let mut redactor = redactor();
		assert_eq!(
			redactor.text("config at /home/alice/.config, unit pyroshine@alice.service on alice-desktop"),
			"config at ~/.config, unit pyroshine@<user>.service on <host>"
		);
		assert_eq!(redactor.text("malice and alice_x stay"), "malice and alice_x stay");
	}

	#[test]
	fn generic_and_short_names_are_kept() {
		let mut redactor = Redactor::new();
		redactor.word("deck", "<user>");
		redactor.word("jo", "<user>");
		assert_eq!(redactor.text("Steam Deck deck jo"), "Steam Deck deck jo");
	}

	#[test]
	fn addresses_get_consistent_pseudonyms_by_range() {
		let mut redactor = Redactor::new();
		let text = redactor.text(
			"client 192.168.1.20 from 192.168.1.20:47998, peer [fe80::1c2:3%eno1]:5 \
			 then 203.0.113.9 and ::ffff:192.168.1.20 via 100.101.2.3",
		);
		assert_eq!(
			text,
			// IPv6 is matched first, so the mapped address is numbered before the plain one.
			"client <lan-ip-2> from <lan-ip-2>:47998, peer [<lan-ip-1>%eno1]:5 then <ip-1> and <lan-ip-2> via <lan-ip-3>"
		);
		let kept = "bind 0.0.0.0 and :: and 127.0.0.1, mDNS 224.0.0.251";
		assert_eq!(redactor.text(kept), kept);
	}

	#[test]
	fn versions_times_and_rust_paths_are_not_addresses() {
		let mut redactor = Redactor::new();
		let text =
			"2026-10-14T16:02:39.661317Z moonshine_core::webserver: at 12:13:07 driver 1.2.3.4.5 mac aa:bb:cc:dd:ee:ff";
		assert_eq!(redactor.text(text), text);
	}

	#[test]
	fn version_numbers_are_not_addresses() {
		let mut redactor = Redactor::new();
		assert_eq!(
			redactor.text("\tconformanceVersion = 1.4.5.3\npeer 10.0.0.2 version 1.4.5.3\n"),
			"\tconformanceVersion = 1.4.5.3\npeer <lan-ip-1> version 1.4.5.3\n"
		);
	}

	#[test]
	fn fingerprints_and_uuids_are_numbered() {
		let mut redactor = Redactor::new();
		let fingerprint = "ab".repeat(32);
		let text = format!(
			"cert {fingerprint} again {} server 0f8fad5b-d9cb-469f-a165-70867728950e",
			fingerprint.to_uppercase()
		);
		assert_eq!(
			redactor.text(&text),
			"cert <fingerprint-1> again <fingerprint-1> server <uuid-1>"
		);
	}

	#[test]
	fn configuration_is_redacted_by_setting() {
		let mut redactor = redactor();
		let config = r#"
name = "Alice's PC"
address = "192.168.1.5"
not_a_setting = "secret token"

[webserver]
port = 47989
certificate = "/home/alice/.config/moonshine/cert.pem"

[[application]]
title = "Steam"
command = ["/home/alice/bin/steam", "-bigpicture"]

[[application_scanner]]
type = "steam"
library = "/home/alice/.local/share/Steam"
command = ["steam"]
"#;
		let redacted = redactor.config(config);
		let value: toml::Table = toml::from_str(&redacted).unwrap();
		assert_eq!(value["name"].as_str(), Some("<server-name>"));
		assert_eq!(value["address"].as_str(), Some("<lan-ip-1>"));
		assert_eq!(value["not_a_setting"].as_str(), Some("<redacted>"));
		assert_eq!(value["webserver"]["port"].as_integer(), Some(47989));
		assert_eq!(
			value["webserver"]["certificate"].as_str(),
			Some("~/.config/moonshine/cert.pem")
		);
		assert_eq!(value["application"][0]["title"].as_str(), Some("Steam"));
		assert_eq!(value["application"][0]["command"][0].as_str(), Some("~/bin/steam"));
		assert_eq!(value["application_scanner"][0]["type"].as_str(), Some("steam"));
		assert_eq!(
			value["application_scanner"][0]["library"].as_str(),
			Some("~/.local/share/Steam")
		);
		// The server name is replaced in later text too.
		assert_eq!(redactor.text("mDNS name Alice's PC."), "mDNS name <server-name>.");

		assert!(
			redactor
				.config("name = [")
				.starts_with("# The configuration could not be parsed")
		);
	}

	/// Every string the default configuration (with every optional setting
	/// present) serializes is a known setting, so nothing a user can set is
	/// blanked as unknown.
	#[test]
	fn every_serialized_setting_is_known() {
		let complete = crate::management::schema::tests::complete_config();
		let text = toml::to_string(&complete).unwrap();
		let redacted: toml::Table = toml::from_str(&Redactor::new().config(&text)).unwrap();
		fn strings(value: &toml::Value, out: &mut Vec<String>) {
			match value {
				toml::Value::String(text) => out.push(text.clone()),
				toml::Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
				toml::Value::Table(table) => table.values().for_each(|item| strings(item, out)),
				_ => {},
			}
		}
		let mut values = Vec::new();
		strings(&toml::Value::Table(redacted), &mut values);
		assert!(!values.contains(&"<redacted>".to_string()), "{values:?}");
	}
}

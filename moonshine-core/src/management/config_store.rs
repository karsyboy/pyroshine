//! Reading, validating and persisting `config.toml` for the management API.
//!
//! Pyroshine never rewrites the configuration on its own (it only creates a
//! default file), so the file is user-authored and may carry comments and
//! formatting. A save therefore edits the user's TOML document in place:
//! only settings whose typed value changed are touched, unchanged
//! applications and scanners keep their original tables, and everything else
//! (comments, ordering, unknown keys) is preserved byte for byte.
//!
//! Every save is checked end to end before anything is written: the edited
//! document must parse back into exactly the configuration that was
//! submitted, and that configuration must pass the same validation as
//! startup. The file is then replaced atomically (synced staging file,
//! rename, directory sync) with its previous permissions.
//!
//! Configuration is read once at startup; nothing here reloads it. A save
//! reports whether the file now differs from the running configuration so the
//! UI can ask for a restart.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use moonshine_management::ManagementError;
use moonshine_management::dto::{ConfigDocument, ConfigIssue, FieldKind, SaveOutcome, ValidationReport};
use serde_json::Value;
use sha2::{Digest, Sha256};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike};

use super::schema::{APPLICATIONS, SCANNERS, config_schema};
use crate::config::Config;

/// Tables whose struct has no field defaults: when a save has to create one,
/// it is written complete or the file would no longer parse.
const COMPLETE_TABLES: &[&str] = &["webserver"];

pub(crate) struct ConfigStore {
	/// Path the daemon was started with.
	path: PathBuf,
	/// The configuration the daemon is running with, as read at startup.
	running: Value,
	/// Serializes saves so a revision check and its write cannot interleave.
	save: tokio::sync::Mutex<()>,
}

/// The file's current content.
struct Snapshot {
	text: String,
	revision: String,
	values: Value,
}

impl ConfigStore {
	pub(crate) fn open(path: PathBuf) -> Self {
		let running = read_snapshot(&path)
			.map(|snapshot| snapshot.values)
			.unwrap_or(Value::Null);
		Self {
			path,
			running,
			save: tokio::sync::Mutex::new(()),
		}
	}

	/// The configuration file as the daemon would read it now.
	pub(crate) fn document(&self) -> Result<ConfigDocument, ManagementError> {
		let snapshot = read_snapshot(&self.path).map_err(ManagementError::Failed)?;
		let (writable, read_only_reason) = writability(&self.path);
		Ok(ConfigDocument {
			path: self.path.display().to_string(),
			revision: snapshot.revision,
			writable,
			read_only_reason,
			restart_required: snapshot.values != self.running,
			values: snapshot.values,
			defaults: config_json(&Config::default()),
		})
	}

	/// Validate `values` against the file without writing it.
	pub(crate) fn validate(&self, values: &str) -> Result<ValidationReport, ManagementError> {
		let current = read_snapshot(&self.path).map_err(ManagementError::Failed)?;
		Ok(match prepare(&current, values) {
			Ok(prepared) => ValidationReport {
				valid: true,
				issues: Vec::new(),
				restart_required: prepared.values != self.running,
				changed_paths: prepared.changed_paths,
			},
			Err(issues) => ValidationReport {
				valid: false,
				issues,
				changed_paths: Vec::new(),
				restart_required: false,
			},
		})
	}

	/// Validate and persist `values`. `base_revision` must be the revision the
	/// caller edited; a file changed since then is never overwritten.
	pub(crate) async fn save(&self, values: &str, base_revision: &str) -> Result<SaveOutcome, ManagementError> {
		let _save = self.save.lock().await;
		let (writable, reason) = writability(&self.path);
		if !writable {
			return Err(ManagementError::ReadOnly(
				reason.unwrap_or_else(|| "The configuration file cannot be written.".into()),
			));
		}
		let current = read_snapshot(&self.path).map_err(ManagementError::Failed)?;
		if current.revision != base_revision {
			return Err(ManagementError::Conflict(
				"The configuration file changed since it was loaded. Reload it and apply your changes again.".into(),
			));
		}
		let prepared = prepare(&current, values).map_err(|issues| {
			ManagementError::Invalid(
				issues
					.iter()
					.map(|issue| match &issue.path {
						Some(path) => format!("{path}: {}", issue.message),
						None => issue.message.clone(),
					})
					.collect::<Vec<_>>()
					.join("\n"),
			)
		})?;
		if prepared.changed_paths.is_empty() {
			return Ok(SaveOutcome {
				revision: current.revision,
				changed_paths: Vec::new(),
				restart_required: current.values != self.running,
			});
		}
		let path = self.path.clone();
		let text = prepared.text.clone();
		tokio::task::spawn_blocking(move || write_preserving_mode(&path, text.as_bytes()))
			.await
			.map_err(|e| ManagementError::Failed(format!("Saving the configuration failed: {e}")))?
			.map_err(|e| ManagementError::Failed(format!("Saving the configuration failed: {e}")))?;
		tracing::info!(
			path = %self.path.display(),
			changed = ?prepared.changed_paths,
			"Configuration saved through the management interface; restart Pyroshine to apply it"
		);
		Ok(SaveOutcome {
			revision: revision(&prepared.text),
			changed_paths: prepared.changed_paths,
			restart_required: prepared.values != self.running,
		})
	}
}

/// A validated edit, ready to be written.
#[derive(Debug)]
struct Prepared {
	text: String,
	values: Value,
	changed_paths: Vec<String>,
}

fn issue(path: Option<String>, message: impl Into<String>) -> ConfigIssue {
	ConfigIssue {
		path,
		message: message.into(),
	}
}

/// Check submitted values against the current file and produce the edited
/// document.
fn prepare(current: &Snapshot, values: &str) -> Result<Prepared, Vec<ConfigIssue>> {
	let submitted: Value =
		serde_json::from_str(values).map_err(|e| vec![issue(None, format!("Malformed configuration: {e}"))])?;
	let config: Config =
		serde_json::from_value(submitted).map_err(|e| vec![issue(None, format!("Invalid configuration: {e}"))])?;
	let values = config_json(&config);
	let changed_paths = changed_paths(&current.values, &values);

	let mut issues = Vec::new();
	if let Err(reason) = config.validate() {
		issues.push(issue(None, reason));
	}
	issues.extend(check_changed(&current.values, &values, &changed_paths));
	if !issues.is_empty() {
		return Err(issues);
	}

	let text = render(&current.text, &current.values, &config, &values).map_err(|e| vec![issue(None, e)])?;
	// The document is only written if it reads back as exactly what was
	// submitted; anything else would be silent corruption.
	let reread = parse(&text).map(|config| config_json(&config)).map_err(|e| {
		vec![issue(
			None,
			format!("Internal error: the edited file does not parse: {e}"),
		)]
	})?;
	if reread != values {
		return Err(vec![issue(
			None,
			"Internal error: the edited file does not reproduce the submitted configuration; nothing was written.",
		)]);
	}
	Ok(Prepared {
		text,
		values,
		changed_paths,
	})
}

fn parse(text: &str) -> Result<Config, String> {
	toml::from_str(text).map_err(|e| e.to_string())
}

fn config_json(config: &Config) -> Value {
	serde_json::to_value(config).expect("configuration serializes to JSON")
}

fn revision(text: &str) -> String {
	hex::encode(&Sha256::digest(text.as_bytes())[..16])
}

fn read_snapshot(path: &Path) -> Result<Snapshot, String> {
	let text = match std::fs::read_to_string(path) {
		Ok(text) => text,
		// Startup creates the file; a missing file reads as the defaults.
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
		Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
	};
	let config = parse(&text).map_err(|e| {
		format!(
			"{} is not a valid configuration; fix it in a text editor first: {e}",
			path.display()
		)
	})?;
	Ok(Snapshot {
		revision: revision(&text),
		values: config_json(&config),
		text,
	})
}

/// Settings whose values differ. List entries are reported per index.
fn changed_paths(old: &Value, new: &Value) -> Vec<String> {
	fn walk(old: &Value, new: &Value, prefix: &str, out: &mut Vec<String>) {
		match (old, new) {
			(Value::Object(a), Value::Object(b)) if prefix != APPLICATIONS && prefix != SCANNERS => {
				let mut keys: Vec<_> = a.keys().chain(b.keys()).collect();
				keys.sort();
				keys.dedup();
				for key in keys {
					let path = if prefix.is_empty() {
						key.clone()
					} else {
						format!("{prefix}.{key}")
					};
					walk(
						a.get(key).unwrap_or(&Value::Null),
						b.get(key).unwrap_or(&Value::Null),
						&path,
						out,
					);
				}
			},
			(Value::Array(a), Value::Array(b)) if prefix == APPLICATIONS || prefix == SCANNERS => {
				for index in 0..a.len().max(b.len()) {
					if a.get(index) != b.get(index) {
						out.push(format!("{prefix}[{index}]"));
					}
				}
			},
			(a, b) if a != b => out.push(prefix.to_string()),
			_ => {},
		}
	}
	let mut out = Vec::new();
	walk(old, new, "", &mut out);
	out
}

fn lookup<'a>(value: &'a Value, path: &str) -> &'a Value {
	path.split('.')
		.fold(value, |value, segment| value.get(segment).unwrap_or(&Value::Null))
}

fn check_range(path: String, value: &Value, kind: &FieldKind, issues: &mut Vec<ConfigIssue>) {
	let (min, max, optional) = match kind {
		FieldKind::Integer { min, max, optional, .. } | FieldKind::Number { min, max, optional, .. } => {
			(*min, *max, *optional)
		},
		FieldKind::Port => (1.0, 65535.0, false),
		_ => return,
	};
	match value.as_f64() {
		Some(number) if number.is_finite() && (min..=max).contains(&number) => {},
		Some(_) => issues.push(issue(Some(path), format!("must be between {min} and {max}"))),
		None if value.is_null() && optional => {},
		None => issues.push(issue(Some(path), "must be a number")),
	}
}

fn check_command(path: String, command: &Value, issues: &mut Vec<ConfigIssue>) {
	let first = command
		.as_array()
		.and_then(|command| command.first())
		.and_then(Value::as_str);
	if first.is_none_or(|program| program.trim().is_empty()) {
		issues.push(issue(Some(path), "needs an executable"));
	}
}

/// Checks applied to settings the edit changed. Values already in the file are
/// left alone: they are what the daemon accepts today.
fn check_changed(old: &Value, new: &Value, changed: &[String]) -> Vec<ConfigIssue> {
	let schema = config_schema();
	let mut issues = Vec::new();
	for field in &schema.fields {
		if changed.contains(&field.path) {
			check_range(field.path.clone(), lookup(new, &field.path), &field.kind, &mut issues);
		}
	}
	if changed.iter().any(|path| path.starts_with("stream.video.fec_"))
		&& lookup(new, "stream.video.fec_mode") == "auto"
		&& lookup(new, "stream.video.fec_min_percentage").as_u64()
			> lookup(new, "stream.video.fec_max_percentage").as_u64()
	{
		issues.push(issue(
			Some("stream.video.fec_max_percentage".into()),
			"must be at least the automatic minimum",
		));
	}

	let field_kind = |path: &str| {
		schema
			.fields
			.iter()
			.find(|field| field.path == path)
			.map(|field| &field.kind)
	};
	let old_items = |key: &str| old[key].as_array().cloned().unwrap_or_default();

	// Applications: changed entries are complete and their titles (which
	// derive client-visible IDs) are unique.
	let applications = new[APPLICATIONS].as_array().cloned().unwrap_or_default();
	let previous = old_items(APPLICATIONS);
	if let Some(FieldKind::Applications { item }) = field_kind(APPLICATIONS) {
		for (index, application) in applications.iter().enumerate() {
			if previous.contains(application) {
				continue;
			}
			let at = |key: &str| format!("{APPLICATIONS}[{index}].{key}");
			if application["title"]
				.as_str()
				.is_none_or(|title| title.trim().is_empty())
			{
				issues.push(issue(Some(at("title")), "is required"));
			}
			check_command(at("command"), &application["command"], &mut issues);
			for field in item {
				check_range(at(&field.path), &application[&field.path], &field.kind, &mut issues);
			}
		}
	}
	let mut titles: Vec<_> = applications
		.iter()
		.filter_map(|application| application["title"].as_str())
		.collect();
	titles.sort_unstable();
	for pair in titles.windows(2) {
		if pair[0] == pair[1] && changed.iter().any(|path| path.starts_with(APPLICATIONS)) {
			issues.push(issue(
				Some(APPLICATIONS.into()),
				format!("the title \"{}\" is used more than once", pair[0]),
			));
		}
	}

	// Scanners: changed entries have what their type needs to discover and launch.
	let previous = old_items(SCANNERS);
	if let Some(FieldKind::Scanners { variants }) = field_kind(SCANNERS) {
		for (index, scanner) in new[SCANNERS].as_array().cloned().unwrap_or_default().iter().enumerate() {
			if previous.contains(scanner) {
				continue;
			}
			let at = |key: &str| format!("{SCANNERS}[{index}].{key}");
			let Some(variant) = variants.iter().find(|variant| scanner["type"] == variant.id.as_str()) else {
				continue;
			};
			for field in &variant.fields {
				let value = &scanner[&field.path];
				match &field.kind {
					FieldKind::Command { .. } if field.required => check_command(at(&field.path), value, &mut issues),
					FieldKind::PathList { .. } if field.required => {
						if value.as_array().is_none_or(|paths| {
							paths.is_empty() || paths.iter().any(|path| path.as_str().is_none_or(str::is_empty))
						}) {
							issues.push(issue(Some(at(&field.path)), "needs at least one directory"));
						}
					},
					FieldKind::Path { .. } if field.required => {
						if value.as_str().is_none_or(|path| path.trim().is_empty()) {
							issues.push(issue(Some(at(&field.path)), "is required"));
						}
					},
					kind => check_range(at(&field.path), value, kind, &mut issues),
				}
			}
		}
	}
	issues
}

/// Apply the typed difference between `old` and `new` to the user's document.
fn render(original: &str, old: &Value, config: &Config, new: &Value) -> Result<String, String> {
	let mut document: DocumentMut = original
		.parse()
		.map_err(|e| format!("The configuration file is not valid TOML: {e}"))?;
	let serialized: DocumentMut = toml::to_string(config)
		.map_err(|e| format!("Cannot serialize the configuration: {e}"))?
		.parse()
		.map_err(|e| format!("Cannot serialize the configuration: {e}"))?;
	let original_document = document.clone();

	merge_table(document.as_table_mut(), false, serialized.as_item(), old, new, "")?;
	for key in [APPLICATIONS, SCANNERS] {
		merge_list(
			&mut document,
			&original_document,
			&serialized,
			key,
			&old[key],
			&new[key],
		);
	}
	Ok(document.to_string())
}

/// `inline` tells whether `target` is (inside) an inline table, where nested
/// tables must be inline too.
fn merge_table(
	target: &mut dyn TableLike,
	inline: bool,
	serialized: &Item,
	old: &Value,
	new: &Value,
	prefix: &str,
) -> Result<(), String> {
	let (Some(old_map), Some(new_map)) = (old.as_object(), new.as_object()) else {
		return Ok(());
	};
	let mut keys: Vec<_> = old_map.keys().chain(new_map.keys()).cloned().collect();
	keys.sort();
	keys.dedup();
	for key in keys {
		if prefix.is_empty() && (key == APPLICATIONS || key == SCANNERS) {
			continue;
		}
		let old_value = old_map.get(&key).unwrap_or(&Value::Null);
		let new_value = new_map.get(&key).unwrap_or(&Value::Null);
		if old_value == new_value {
			continue;
		}
		let path = if prefix.is_empty() {
			key.clone()
		} else {
			format!("{prefix}.{key}")
		};
		let serialized_item = serialized.get(key.as_str()).unwrap_or(&Item::None);
		if old_value.is_object() && new_value.is_object() {
			if target.get(&key).is_none() {
				let complete = COMPLETE_TABLES.contains(&path.as_str());
				let table = if complete {
					serialized_item.as_table().cloned().ok_or("missing serialized table")?
				} else {
					let mut table = Table::new();
					table.set_implicit(true);
					table
				};
				insert_table(target, inline, &key, table);
				if complete {
					continue;
				}
			}
			let child_inline = inline || target.get(&key).is_some_and(Item::is_inline_table);
			let child = target
				.get_mut(&key)
				.and_then(Item::as_table_like_mut)
				.ok_or_else(|| format!("{path} is not a table in the configuration file"))?;
			merge_table(child, child_inline, serialized_item, old_value, new_value, &path)?;
			continue;
		}
		match serialized_item.as_value() {
			None => {
				target.remove(&key);
			},
			Some(value) => {
				let mut value = value.clone();
				value.decor_mut().clear();
				match target.get_mut(&key) {
					// Replace only the value: the key keeps its comments and
					// the value its surrounding whitespace and trailing comment.
					Some(existing) => {
						if let Some(old) = existing.as_value() {
							*value.decor_mut() = old.decor().clone();
						}
						*existing = Item::Value(value);
					},
					None => {
						target.insert(&key, Item::Value(value));
					},
				}
			},
		}
	}
	Ok(())
}

/// Insert a new table in the parent's style: inline inside inline tables,
/// dotted keys inside dotted tables, otherwise its own `[header]`.
fn insert_table(target: &mut dyn TableLike, inline: bool, key: &str, mut table: Table) {
	if inline {
		target.insert(
			key,
			Item::Value(toml_edit::Value::InlineTable(table.into_inline_table())),
		);
	} else {
		if target.is_dotted() {
			table.set_dotted(true);
		}
		target.insert(key, Item::Table(table));
	}
}

/// Replace a list of tables, reusing the original table (comments and
/// formatting) for every entry that is unchanged.
fn merge_list(
	document: &mut DocumentMut,
	original: &DocumentMut,
	serialized: &DocumentMut,
	key: &str,
	old: &Value,
	new: &Value,
) {
	if old == new {
		return;
	}
	let new_items = new.as_array().cloned().unwrap_or_default();
	if new_items.is_empty() {
		// An omitted list means the default entries; an empty list must be explicit.
		document.insert(key, toml_edit::value(toml_edit::Array::new()));
		return;
	}
	let old_items = old.as_array().cloned().unwrap_or_default();
	let original_tables: Vec<Table> = original
		.get(key)
		.and_then(Item::as_array_of_tables)
		.map(|tables| tables.iter().cloned().collect())
		.unwrap_or_default();
	let serialized_tables = serialized.get(key).and_then(Item::as_array_of_tables);
	let first_position = original_tables.iter().filter_map(Table::position).min();

	let mut used = vec![false; old_items.len()];
	let mut tables = ArrayOfTables::new();
	for (index, item) in new_items.iter().enumerate() {
		let reused = old_items
			.iter()
			.enumerate()
			.find(|(old_index, old_item)| !used[*old_index] && *old_item == item && *old_index < original_tables.len())
			.map(|(old_index, _)| {
				used[old_index] = true;
				original_tables[old_index].clone()
			});
		let mut table = reused.unwrap_or_else(|| {
			serialized_tables
				.and_then(|tables| tables.get(index))
				.cloned()
				.unwrap_or_default()
		});
		// Keep the list where it was in the file, in its new order.
		table.set_position(first_position.map(|position| position + index as isize));
		tables.push(table);
	}
	document.insert(key, Item::ArrayOfTables(tables));
}

/// Whether the daemon can replace the file, and why not.
fn writability(path: &Path) -> (bool, Option<String>) {
	let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
	if target.starts_with("/nix/store") {
		return (
			false,
			Some("The configuration is generated by NixOS. Change services.moonshine.settings instead.".into()),
		);
	}
	let directory = crate::durable::directory(&target);
	let writable = |path: &Path| {
		CString::new(path.as_os_str().as_bytes())
			.map(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
			.unwrap_or(false)
	};
	if target.exists() && !writable(&target) {
		return (
			false,
			Some(format!(
				"{} is not writable by the Pyroshine service user.",
				target.display()
			)),
		);
	}
	if !writable(directory) {
		return (
			false,
			Some(format!(
				"{} is not writable by the Pyroshine service user, so the file cannot be replaced safely.",
				directory.display()
			)),
		);
	}
	(true, None)
}

/// Atomically replace the file behind `path` (following symlinks, so a linked
/// configuration stays linked), keeping its permission bits.
fn write_preserving_mode(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
	let mode = std::fs::metadata(&target)
		.ok()
		.map(|metadata| metadata.permissions().mode() & 0o7777);
	crate::durable::replace_with_mode(&target, bytes, mode)
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	const COMMENTED: &str = r#"# Pyroshine on the living-room PC.
name = "Den" # shown in Moonlight

# Keep the defaults for everything else.
application_scanner = []

[stream]
timeout = 30 # seconds

[stream.video]
# Tunnel MTU 1420.
max_packet_size = 1376
fec_mode = "fixed"

[compositor]
hdr = true

[compositor.keyboard]
layout = "de"
custom_unknown_key = "kept"

# Main entry.
[[application]]
title = "Steam"
command = ["/usr/bin/steam", "steam://open/bigpicture"] # big picture

# Desktop session.
[[application]]
title = "Plasma"
command = ["/usr/bin/startplasma-wayland"]
output_scale = 1.5
pre_command = [["/usr/bin/true", "--flag with space"]]
"#;

	fn snapshot(text: &str) -> Snapshot {
		let config = parse(text).unwrap();
		Snapshot {
			text: text.into(),
			revision: revision(text),
			values: config_json(&config),
		}
	}

	fn edit(text: &str, change: impl FnOnce(&mut Value)) -> Result<Prepared, Vec<ConfigIssue>> {
		let current = snapshot(text);
		let mut values = current.values.clone();
		change(&mut values);
		prepare(&current, &values.to_string())
	}

	#[test]
	fn unchanged_values_leave_the_file_untouched() {
		let prepared = edit(COMMENTED, |_| {}).unwrap();
		assert!(prepared.changed_paths.is_empty());
		assert_eq!(prepared.text, COMMENTED);
	}

	#[test]
	fn edits_preserve_comments_formatting_and_unknown_keys() {
		let prepared = edit(COMMENTED, |values| {
			values["name"] = json!("Den PC");
			values["stream"]["video"]["fec_mode"] = json!("auto");
			values["compositor"]["keyboard"]["options"] = json!("caps:escape");
		})
		.unwrap();
		assert_eq!(
			prepared.changed_paths,
			vec!["compositor.keyboard.options", "name", "stream.video.fec_mode"]
		);
		let expected = COMMENTED
			.replace("name = \"Den\" # shown", "name = \"Den PC\" # shown")
			.replace("fec_mode = \"fixed\"", "fec_mode = \"auto\"")
			.replace(
				"custom_unknown_key = \"kept\"\n",
				"custom_unknown_key = \"kept\"\noptions = \"caps:escape\"\n",
			);
		assert_eq!(prepared.text, expected);
	}

	#[test]
	fn missing_tables_are_created_and_optional_settings_removed() {
		let text = "name = \"Host\"\n\n[compositor]\ngpu = \"/dev/dri/renderD128\"\n";
		let prepared = edit(text, |values| {
			values["compositor"]["gpu"] = Value::Null;
			values["stream"]["control"]["gamepad"]["home_button"]["hold_ms"] = json!(750);
			values["stream"]["control"]["gamepad"]["home_button"]["trigger"] = json!("back_start");
		})
		.unwrap();
		assert!(!prepared.text.contains("gpu"));
		assert!(prepared.text.contains("[stream.control.gamepad.home_button]"));
		// Intermediate tables without their own settings get no header.
		assert!(!prepared.text.contains("[stream]\n"));
		assert!(prepared.text.starts_with("name = \"Host\"\n"));
	}

	#[test]
	fn a_created_webserver_table_is_complete() {
		// `[webserver]` fields have no individual defaults.
		let prepared = edit("name = \"Host\"\n", |values| {
			values["webserver"]["enable_pairing"] = json!(false);
		})
		.unwrap();
		assert!(prepared.text.contains("[webserver]"));
		for key in [
			"port",
			"port_https",
			"certificate",
			"private_key",
			"enable_pairing = false",
		] {
			assert!(prepared.text.contains(key), "{key}");
		}
	}

	#[test]
	fn application_edits_keep_unchanged_entries_verbatim() {
		let prepared = edit(COMMENTED, |values| {
			let applications = values[APPLICATIONS].as_array_mut().unwrap();
			applications.swap(0, 1);
			applications.push(json!({
				"title": "Shell script",
				"command": ["/usr/bin/bash", "-c", "echo \"quoted\" && exit 0"],
				"launch_timeout_secs": 10,
			}));
		})
		.unwrap();
		let text = &prepared.text;
		// Reordered entries carry their comments along and keep their
		// formatting; the new entry follows them.
		let plasma = text.find("# Desktop session.").unwrap();
		let steam = text.find("# Main entry.").unwrap();
		let script = text.find("Shell script").unwrap();
		assert!(plasma < steam && steam < script, "{text}");
		assert!(text.contains("command = [\"/usr/bin/steam\", \"steam://open/bigpicture\"] # big picture"));
		assert!(text.contains("pre_command = [[\"/usr/bin/true\", \"--flag with space\"]]"));
		let reread = config_json(&parse(text).unwrap());
		assert_eq!(reread[APPLICATIONS][2]["command"][2], "echo \"quoted\" && exit 0");
		assert_eq!(reread[APPLICATIONS][0]["output_scale"], 1.5);
	}

	#[test]
	fn emptied_lists_are_written_explicitly() {
		// An omitted list would bring back the default Steam entry.
		let prepared = edit(COMMENTED, |values| values[APPLICATIONS] = json!([])).unwrap();
		assert!(prepared.text.contains("application = []"));
		assert!(!prepared.text.contains("[[application]]"));
		assert_eq!(config_json(&parse(&prepared.text).unwrap())[APPLICATIONS], json!([]));
	}

	#[test]
	fn scanners_round_trip_every_type() {
		let prepared = edit(COMMENTED, |values| {
			values[SCANNERS] = json!([
				{"type": "steam", "library": "$HOME/.local/share/Steam", "command": ["/usr/bin/steam", "steam://rungameid/{game_id}"]},
				{"type": "desktop", "directories": ["~/.local/share/applications"], "include_terminal": false, "resolve_icons": true, "stdout": "journal"},
				{"type": "lutris", "pga_db": "/home/u/.local/share/lutris/pga.db", "command": ["/usr/bin/lutris", "lutris:rungame/{slug}"]},
				{"type": "heroic", "config_dir": "/home/u/.config/heroic", "command": ["/usr/bin/heroic", "heroic://launch?appName={app_name}&runner={runner}"], "post_command": [["/usr/bin/notify-send", "done"]]},
			]);
		})
		.unwrap();
		assert!(!prepared.text.contains("application_scanner = []"));
		let reread = config_json(&parse(&prepared.text).unwrap());
		assert_eq!(reread[SCANNERS].as_array().unwrap().len(), 4);
		assert_eq!(
			reread[SCANNERS][3]["command"][1],
			"heroic://launch?appName={app_name}&runner={runner}"
		);
	}

	#[test]
	fn invalid_values_are_rejected_with_paths() {
		let issues = edit(COMMENTED, |values| {
			values["stream"]["video"]["fec_mode"] = json!("sometimes")
		})
		.unwrap_err();
		assert!(issues[0].message.contains("unknown variant"), "{issues:?}");

		let issues = edit(COMMENTED, |values| {
			values["stream"]["audio"]["port"] = values["stream"]["control"]["port"].clone();
		})
		.unwrap_err();
		assert!(issues[0].message.contains("UDP port"), "{issues:?}");

		let issues = edit(COMMENTED, |values| values["address"] = json!("localhost")).unwrap_err();
		assert!(issues[0].message.contains("not an IP address"));

		let issues = edit(COMMENTED, |values| {
			values["stream"]["control"]["gamepad"]["home_button"]["rumble_intensity"] = json!(1.5);
			values["webserver"]["port"] = json!(0);
		})
		.unwrap_err();
		let paths: Vec<_> = issues.iter().filter_map(|issue| issue.path.clone()).collect();
		assert!(paths.contains(&"stream.control.gamepad.home_button.rumble_intensity".to_string()));
		assert!(paths.contains(&"webserver.port".to_string()));

		let issues = edit(COMMENTED, |values| {
			values[APPLICATIONS]
				.as_array_mut()
				.unwrap()
				.push(json!({"title": "Steam", "command": []}));
		})
		.unwrap_err();
		let paths: Vec<_> = issues.iter().filter_map(|issue| issue.path.clone()).collect();
		assert!(paths.contains(&"application[2].command".to_string()), "{issues:?}");
		assert!(
			paths.contains(&"application".to_string()),
			"duplicate title: {issues:?}"
		);

		let issues = edit(COMMENTED, |values| {
			values[SCANNERS] = json!([{"type": "desktop", "directories": []}]);
		})
		.unwrap_err();
		assert_eq!(issues[0].path.as_deref(), Some("application_scanner[0].directories"));

		let issues = edit(COMMENTED, |values| {
			values["stream"]["video"]["fec_mode"] = json!("auto");
			values["stream"]["video"]["fec_min_percentage"] = json!(30);
		})
		.unwrap_err();
		assert_eq!(issues[0].path.as_deref(), Some("stream.video.fec_max_percentage"));
	}

	#[test]
	fn values_the_file_already_had_are_not_newly_rejected() {
		// The daemon accepts this file; editing an unrelated setting must work.
		let text = "[[application]]\ntitle = \"Empty\"\ncommand = []\n\n[[application]]\ntitle = \"Empty\"\ncommand = [\"x\"]\n";
		let prepared = edit(text, |values| values["name"] = json!("Other")).unwrap();
		assert_eq!(prepared.changed_paths, vec!["name"]);
	}

	#[tokio::test]
	async fn saves_are_atomic_conflict_checked_and_keep_permissions() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("config.toml");
		std::fs::write(&path, COMMENTED).unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
		let store = ConfigStore::open(path.clone());

		let document = store.document().unwrap();
		assert!(document.writable);
		assert!(!document.restart_required);
		let mut values = document.values.clone();
		values["name"] = json!("Saved");

		let stale = store.save(&values.to_string(), "not-the-revision").await.unwrap_err();
		assert_eq!(stale.kind(), "conflict");
		assert_eq!(std::fs::read_to_string(&path).unwrap(), COMMENTED);

		let invalid = {
			let mut values = values.clone();
			values["address"] = json!("nowhere");
			store.save(&values.to_string(), &document.revision).await.unwrap_err()
		};
		assert_eq!(invalid.kind(), "invalid");
		assert_eq!(std::fs::read_to_string(&path).unwrap(), COMMENTED);

		let saved = store.save(&values.to_string(), &document.revision).await.unwrap();
		assert!(saved.restart_required);
		assert_eq!(saved.changed_paths, vec!["name"]);
		let text = std::fs::read_to_string(&path).unwrap();
		assert!(text.contains("name = \"Saved\" # shown in Moonlight"));
		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
		assert_eq!(
			std::fs::read_dir(directory.path()).unwrap().count(),
			1,
			"no staging file left"
		);

		let reloaded = store.document().unwrap();
		assert_eq!(reloaded.revision, saved.revision);
		assert!(reloaded.restart_required);
		assert_eq!(reloaded.values["name"], "Saved");

		// Saving the running values again clears the restart requirement.
		let restored = store
			.save(&document.values.to_string(), &reloaded.revision)
			.await
			.unwrap();
		assert!(!restored.restart_required);
	}

	#[tokio::test]
	async fn read_only_and_symlinked_files() {
		let directory = tempfile::tempdir().unwrap();
		let real = directory.path().join("real.toml");
		let link = directory.path().join("config.toml");
		std::fs::write(&real, "name = \"Linked\"\n").unwrap();
		std::os::unix::fs::symlink(&real, &link).unwrap();
		let store = ConfigStore::open(link.clone());
		let document = store.document().unwrap();
		let mut values = document.values.clone();
		values["name"] = json!("Still linked");
		store.save(&values.to_string(), &document.revision).await.unwrap();
		assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
		assert!(std::fs::read_to_string(&real).unwrap().contains("Still linked"));

		std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o444)).unwrap();
		// Root bypasses permission bits; the check is meaningful for users only.
		if unsafe { libc::geteuid() } != 0 {
			let document = store.document().unwrap();
			assert!(!document.writable);
			assert!(document.read_only_reason.is_some());
			let error = store.save(&values.to_string(), &document.revision).await.unwrap_err();
			assert_eq!(error.kind(), "read_only");
		}
	}

	#[test]
	fn an_unparsable_file_is_reported_not_overwritten() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("config.toml");
		std::fs::write(&path, "name = \n").unwrap();
		let store = ConfigStore::open(path);
		let error = store.document().unwrap_err();
		assert!(error.message().contains("not a valid configuration"));
	}
}

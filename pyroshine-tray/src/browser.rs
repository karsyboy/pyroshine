//! Opening web interface pages in the user's default browser.
//!
//! Pages are a fixed set of routes (see [`page`]); the address comes from the
//! running server (`GetWebUrl`) and must be an `http(s)` address. Nothing
//! that grants access is ever put into the address: the browser signs in
//! with its own session like any other.
//!
//! The browser is started through the desktop portal's `OpenURI`, passing the
//! XDG activation token of the click so the browser may take focus on
//! Wayland. Without a portal, `xdg-open` runs with the address as its only
//! argument (no shell) and the token in `XDG_ACTIVATION_TOKEN`.

use std::collections::HashMap;

use zbus::zvariant::Value;

/// The address used when the server cannot be asked (its default).
pub const DEFAULT_URL: &str = "http://127.0.0.1:47990/";

/// Routes of the web interface the tray and notifications open.
const PAGES: &[&str] = &[
	"dashboard",
	"library",
	"clients",
	"history",
	"settings",
	"diagnostics",
	"updates",
	"maintenance",
];

/// Restrict a page request (from a click, a command line or another
/// process) to a known route and, for the clients page, a pairing request
/// token: `clients?request=0123abcd`.
pub fn page(requested: &str) -> String {
	let (route, query) = requested.split_once('?').unwrap_or((requested, ""));
	let route = match route.trim_matches('/') {
		"pairing" => "clients",
		route if PAGES.contains(&route) => route,
		_ => "dashboard",
	};
	let request = query
		.split('&')
		.filter_map(|pair| pair.split_once('='))
		.find(|(key, _)| *key == "request")
		.map(|(_, value)| value)
		.filter(|value| !value.is_empty() && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()));
	match request {
		Some(request) if route == "clients" => format!("clients?request={request}"),
		_ => route.to_string(),
	}
}

/// An activation token from another process, if it looks like one. Tokens
/// are opaque compositor strings; this only bounds what is passed on.
pub fn activation_token(token: &str) -> Option<String> {
	let valid = !token.is_empty() && token.len() <= 512 && token.bytes().all(|byte| byte.is_ascii_graphic());
	valid.then(|| token.to_string())
}

/// The full address of `page` under the interface's `base` address. Only
/// `http`/`https` bases are accepted; anything else falls back to the default.
pub fn address(base: Option<&str>, page: &str) -> String {
	let base = base
		.and_then(|base| url::Url::parse(base).ok())
		.filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
		.filter(|url| url.username().is_empty() && url.password().is_none())
		.map(|mut url| {
			url.set_path("/");
			url.set_query(None);
			url.set_fragment(None);
			url.to_string()
		})
		.unwrap_or_else(|| DEFAULT_URL.to_string());
	format!("{base}#/{}", self::page(page))
}

#[zbus::proxy(
	interface = "org.freedesktop.portal.OpenURI",
	default_service = "org.freedesktop.portal.Desktop",
	default_path = "/org/freedesktop/portal/desktop"
)]
trait OpenUri {
	#[zbus(name = "OpenURI")]
	fn open_uri(
		&self,
		parent_window: &str,
		uri: &str,
		options: HashMap<&str, Value<'_>>,
	) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

/// Open `address` in the default browser.
pub async fn open(
	connection: Option<&zbus::Connection>,
	address: &str,
	activation: Option<String>,
) -> Result<(), String> {
	if let Some(connection) = connection {
		match open_with_portal(connection, address, activation.as_deref()).await {
			Ok(()) => return Ok(()),
			Err(error) => tracing::debug!("The desktop portal could not open the browser ({error}); trying xdg-open"),
		}
	}
	let mut command = tokio::process::Command::new("xdg-open");
	command.arg(address).kill_on_drop(false);
	if let Some(token) = activation {
		command.env("XDG_ACTIVATION_TOKEN", token);
	}
	match command.status().await {
		Ok(status) if status.success() => Ok(()),
		Ok(status) => Err(format!(
			"xdg-open could not open the browser ({status}). Open {address} yourself, or set a default browser."
		)),
		Err(error) => Err(format!(
			"No browser launcher is available ({error}). Install xdg-utils or a desktop portal, or open {address} yourself."
		)),
	}
}

async fn open_with_portal(connection: &zbus::Connection, address: &str, activation: Option<&str>) -> zbus::Result<()> {
	let proxy = OpenUriProxy::new(connection).await?;
	let mut options = HashMap::new();
	if let Some(token) = activation {
		options.insert("activation_token", Value::from(token));
	}
	proxy.open_uri("", address, options).await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pages_are_limited_to_known_routes_and_request_tokens() {
		assert_eq!(page("clients?request=00ff"), "clients?request=00ff");
		assert_eq!(page("/settings"), "settings");
		assert_eq!(page("updates"), "updates");
		assert_eq!(page("pairing"), "clients");
		assert_eq!(page("clients?request=<script>"), "clients");
		assert_eq!(page("dashboard?request=00ff"), "dashboard");
		assert_eq!(page("https://example.com"), "dashboard");
		assert_eq!(page("../../etc/passwd"), "dashboard");
	}

	#[test]
	fn addresses_come_from_the_server_and_carry_no_credentials() {
		assert_eq!(
			address(Some("http://127.0.0.1:48990/"), "clients?request=ab12"),
			"http://127.0.0.1:48990/#/clients?request=ab12"
		);
		assert_eq!(
			address(Some("https://[::1]:47990/ignored?x=1#y"), "history"),
			"https://[::1]:47990/#/history"
		);
		for unusable in [
			None,
			Some(""),
			Some("file:///etc/passwd"),
			Some("javascript:alert(1)"),
			Some("http://user:pw@host/"),
		] {
			assert_eq!(
				address(unusable, "dashboard"),
				"http://127.0.0.1:47990/#/dashboard",
				"{unusable:?}"
			);
		}
	}

	#[test]
	fn activation_tokens_are_bounded_opaque_strings() {
		assert_eq!(
			activation_token("kwin-12345-abc_TIME0").as_deref(),
			Some("kwin-12345-abc_TIME0")
		);
		assert_eq!(activation_token(""), None);
		assert_eq!(activation_token("has space"), None);
		assert_eq!(activation_token(&"x".repeat(513)), None);
	}
}

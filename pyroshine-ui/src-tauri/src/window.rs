//! The main window. It exists only while open: closing destroys the webview,
//! so a tray-only UI holds no web content process or GPU resources.
//!
//! On Wayland a window may only take focus with an XDG activation token
//! issued by the compositor for the user's click. The notification server
//! (`ActivationToken`), the tray host (`ProvideXdgActivationToken`) and the
//! launcher (`XDG_ACTIVATION_TOKEN`) pass one; [`show`] hands it to GTK, which
//! activates the window with it. Without a token GTK requests one itself from
//! the app's own last input, which compositors refuse after a click elsewhere,
//! so the window would only be marked as demanding attention.

use gtk::glib::prelude::ObjectExt;
use gtk::prelude::GtkWindowExt;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "main";

/// Restrict deep links from other processes to known pages and a request
/// token, for example `clients?request=0123abcd`.
pub fn sanitize(page: &str) -> String {
	let (route, query) = page.split_once('?').unwrap_or((page, ""));
	let route = match route.trim_matches('/') {
		route @ ("dashboard" | "clients" | "settings" | "diagnostics") => route,
		_ => "dashboard",
	};
	let request = query
		.split('&')
		.filter_map(|pair| pair.split_once('='))
		.find(|(key, _)| *key == "request")
		.map(|(_, value)| value)
		.filter(|value| !value.is_empty() && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()));
	match request {
		Some(request) => format!("{route}?request={request}"),
		None => route.to_string(),
	}
}

/// An activation token from another process, if it looks like one. Tokens
/// are opaque compositor strings; this only bounds what is handed to GTK.
pub fn activation_token(token: &str) -> Option<String> {
	let valid = !token.is_empty() && token.len() <= 512 && token.bytes().all(|byte| byte.is_ascii_graphic());
	valid.then(|| token.to_string())
}

/// The token a launcher passed to this process. It is removed from the
/// environment so GTK does not also apply it to a later, unrelated focus.
/// Call before any other thread exists.
pub fn take_launch_activation_token() -> Option<String> {
	let token = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
		.iter()
		.find_map(|name| std::env::var(name).ok())
		.and_then(|token| activation_token(&token));
	// SAFETY: called before any other thread exists.
	unsafe {
		std::env::remove_var("XDG_ACTIVATION_TOKEN");
		std::env::remove_var("DESKTOP_STARTUP_ID");
	}
	token
}

/// Whether GTK talks to a Wayland compositor (tokens activate windows only
/// there; on X11 GTK raises with the usual focus request).
fn on_wayland() -> bool {
	gtk::gdk::Display::default().is_some_and(|display| display.type_().name() == "GdkWaylandDisplay")
}

/// Show (creating if needed) the window at `page`, activating it with
/// `activation` when the user's click came with a token.
pub fn show(app: &AppHandle, page: &str, activation: Option<String>) {
	let page = sanitize(page);
	let handle = app.clone();
	let _ = app.run_on_main_thread(move || {
		let activation = activation.filter(|_| on_wayland());
		if let Some(window) = handle.get_webview_window(LABEL) {
			let _ = window.show();
			let _ = window.unminimize();
			match (activation, window.gtk_window()) {
				// On a mapped window GTK activates with the token at once.
				(Some(token), Ok(gtk_window)) => gtk_window.set_startup_id(&token),
				_ => {
					let _ = window.set_focus();
				},
			}
			let _ = window.emit("ui://navigate", &page);
			return;
		}
		let url = WebviewUrl::App(format!("index.html#/{page}").into());
		let window = match WebviewWindowBuilder::new(&handle, LABEL, url)
			.title("Pyroshine")
			.inner_size(1200.0, 820.0)
			.min_inner_size(860.0, 600.0)
			// Mapped below, after the token is set: GTK applies it when mapping.
			.visible(activation.is_none())
			.build()
		{
			Ok(window) => window,
			Err(error) => {
				tracing::error!("Cannot open the Pyroshine window: {error}");
				return;
			},
		};
		if let Some(token) = activation {
			if let Ok(gtk_window) = window.gtk_window() {
				gtk_window.set_startup_id(&token);
			}
			let _ = window.show();
		}
	});
}

pub fn is_visible(app: &AppHandle) -> bool {
	app.get_webview_window(LABEL)
		.and_then(|window| window.is_visible().ok())
		.unwrap_or(false)
}

/// WebKitGTK's DMA-BUF renderer shows blank windows on some NVIDIA Wayland
/// setups. Prefer its shared-memory path there unless the user chose otherwise.
pub fn apply_webkit_workarounds() {
	if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
		&& std::path::Path::new("/proc/driver/nvidia/version").exists()
	{
		// SAFETY: called before any other thread exists.
		unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
	}
}

#[cfg(test)]
mod tests {
	use super::{activation_token, sanitize};

	#[test]
	fn deep_links_are_limited_to_known_pages_and_tokens() {
		assert_eq!(sanitize("clients?request=00ff"), "clients?request=00ff");
		assert_eq!(sanitize("/settings"), "settings");
		assert_eq!(sanitize("clients?request=<script>"), "clients");
		assert_eq!(sanitize("https://example.com"), "dashboard");
		assert_eq!(sanitize("../../etc/passwd"), "dashboard");
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

//! The main window. It exists only while open: closing destroys the webview,
//! so a tray-only UI holds no web content process or GPU resources.

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

/// Show (creating if needed) the window at `page`.
pub fn show(app: &AppHandle, page: &str) {
	let page = sanitize(page);
	let handle = app.clone();
	let _ = app.run_on_main_thread(move || {
		if let Some(window) = handle.get_webview_window(LABEL) {
			let _ = window.show();
			let _ = window.unminimize();
			let _ = window.set_focus();
			let _ = window.emit("ui://navigate", &page);
			return;
		}
		let url = WebviewUrl::App(format!("index.html#/{page}").into());
		if let Err(error) = WebviewWindowBuilder::new(&handle, LABEL, url)
			.title("Pyroshine")
			.inner_size(1200.0, 820.0)
			.min_inner_size(860.0, 600.0)
			.build()
		{
			tracing::error!("Cannot open the Pyroshine window: {error}");
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
	use super::sanitize;

	#[test]
	fn deep_links_are_limited_to_known_pages_and_tokens() {
		assert_eq!(sanitize("clients?request=00ff"), "clients?request=00ff");
		assert_eq!(sanitize("/settings"), "settings");
		assert_eq!(sanitize("clients?request=<script>"), "clients");
		assert_eq!(sanitize("https://example.com"), "dashboard");
		assert_eq!(sanitize("../../etc/passwd"), "dashboard");
	}
}

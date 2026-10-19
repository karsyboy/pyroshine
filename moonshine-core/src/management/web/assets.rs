//! The browser UI's files.
//!
//! The frontend (`web/`) is built at release time into static
//! files; Node.js is never needed at runtime. Packages install them under
//! `share/pyroshine/web`; the server reads that directory once at startup
//! into memory. A request path is only ever a key into that map, never a
//! file system path, so no request can reach another file.
//!
//! The build writes `pyroshine-web.json` with the version it was built for.
//! A mismatch with the server is logged and reported to the page, which
//! warns that it may not match the API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hyper::body::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Response, StatusCode};
use serde::Deserialize;

use super::http::{Body, body, error};

/// Bounds on what is read into memory.
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 512;

struct Asset {
	bytes: Bytes,
	content_type: &'static str,
	etag: String,
	/// Content-hashed build output (`assets/…`), cacheable forever.
	immutable: bool,
}

pub(crate) struct Assets {
	files: HashMap<String, Asset>,
	version: Option<String>,
	root: Option<PathBuf>,
}

#[derive(Deserialize)]
struct BuildInfo {
	version: String,
}

fn content_type(path: &str) -> Option<&'static str> {
	Some(match path.rsplit_once('.')?.1 {
		"html" => "text/html; charset=utf-8",
		"js" | "mjs" => "text/javascript; charset=utf-8",
		"css" => "text/css; charset=utf-8",
		"json" => "application/json",
		"webmanifest" => "application/manifest+json",
		"png" => "image/png",
		"svg" => "image/svg+xml",
		"ico" => "image/x-icon",
		"woff" => "font/woff",
		"woff2" => "font/woff2",
		"txt" => "text/plain; charset=utf-8",
		_ => return None,
	})
}

/// Where packages install the files, relative to the executable first so a
/// portable installation finds its own copy.
fn candidates(configured: Option<&Path>) -> Vec<PathBuf> {
	let mut candidates = Vec::new();
	if let Some(path) = configured {
		candidates.push(path.to_path_buf());
		return candidates;
	}
	if let Some(path) = std::env::var_os("MOONSHINE_WEB_ROOT") {
		candidates.push(PathBuf::from(path));
		return candidates;
	}
	if let Ok(executable) = std::env::current_exe()
		&& let Some(prefix) = executable.parent().and_then(Path::parent)
	{
		candidates.push(prefix.join("share/pyroshine/web"));
	}
	candidates.push("/usr/share/pyroshine/web".into());
	candidates.push("/usr/local/share/pyroshine/web".into());
	candidates
}

impl Assets {
	pub(crate) fn load(configured: Option<&Path>) -> Self {
		let root = candidates(configured)
			.into_iter()
			.find(|candidate| candidate.join("index.html").is_file());
		let Some(root) = root else {
			tracing::warn!(
				"The web management UI's files were not found (install them to share/pyroshine/web, or set \
				 [management] web_root); the API is available but the page only explains this"
			);
			return Self {
				files: HashMap::new(),
				version: None,
				root: None,
			};
		};
		let mut files = HashMap::new();
		let mut total = 0u64;
		for entry in walkdir::WalkDir::new(&root)
			.follow_links(false)
			.max_depth(4)
			.into_iter()
			.filter_map(Result::ok)
			.filter(|entry| entry.file_type().is_file())
		{
			let Ok(relative) = entry.path().strip_prefix(&root) else {
				continue;
			};
			let Some(key) = relative.to_str().map(|key| key.replace('\\', "/")) else {
				continue;
			};
			let Some(content_type) = content_type(&key) else {
				continue;
			};
			let size = entry.metadata().map_or(u64::MAX, |metadata| metadata.len());
			if size > MAX_FILE || total + size > MAX_TOTAL || files.len() >= MAX_FILES {
				tracing::warn!(file = %entry.path().display(), "Web UI file skipped: the UI exceeds its size bounds");
				continue;
			}
			let Ok(bytes) = std::fs::read(entry.path()) else {
				continue;
			};
			total += bytes.len() as u64;
			let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &bytes);
			files.insert(
				key.clone(),
				Asset {
					etag: format!("\"{}\"", hex::encode(&digest.as_ref()[..12])),
					immutable: key.starts_with("assets/"),
					bytes: Bytes::from(bytes),
					content_type,
				},
			);
		}
		let version = files
			.get("pyroshine-web.json")
			.and_then(|asset| serde_json::from_slice::<BuildInfo>(&asset.bytes).ok())
			.map(|info| info.version);
		let server = env!("CARGO_PKG_VERSION");
		match &version {
			Some(version) if version == server => {},
			Some(version) => tracing::warn!(
				ui = version,
				server,
				root = %root.display(),
				"The web management UI was built for another Pyroshine version; install matching packages"
			),
			None => tracing::warn!(root = %root.display(), "The web management UI does not state its version"),
		}
		tracing::debug!(root = %root.display(), files = files.len(), bytes = total, "Web management UI loaded");
		Self {
			files,
			version,
			root: Some(root),
		}
	}

	#[cfg(test)]
	pub(crate) fn from_files(files: &[(&str, &str)], version: Option<&str>) -> Self {
		Self {
			files: files
				.iter()
				.map(|(path, text)| {
					(
						path.to_string(),
						Asset {
							bytes: Bytes::from(text.to_string()),
							content_type: content_type(path).unwrap_or("application/octet-stream"),
							etag: format!("\"{path}\""),
							immutable: path.starts_with("assets/"),
						},
					)
				})
				.collect(),
			version: version.map(str::to_string),
			root: None,
		}
	}

	pub(crate) fn version(&self) -> Option<&str> {
		self.version.as_deref()
	}

	pub(crate) fn respond(&self, method: &Method, path: &str) -> Response<Body> {
		if !matches!(*method, Method::GET | Method::HEAD) {
			return error(StatusCode::METHOD_NOT_ALLOWED, "invalid", "Only GET is supported here.");
		}
		let key = match path.trim_start_matches('/') {
			"" => "index.html",
			key => key,
		};
		if self.files.is_empty() && key == "index.html" {
			return missing_page(self.root.as_deref());
		}
		let Some(asset) = self.files.get(key) else {
			return error(StatusCode::NOT_FOUND, "not_found", "Not found.");
		};
		let mut response = Response::new(if *method == Method::HEAD {
			body(Bytes::new())
		} else {
			body(asset.bytes.clone())
		});
		let headers = response.headers_mut();
		headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.content_type));
		headers.insert(header::ETAG, HeaderValue::from_str(&asset.etag).expect("hex etag"));
		headers.insert(
			header::CACHE_CONTROL,
			HeaderValue::from_static(if asset.immutable {
				"public, max-age=31536000, immutable"
			} else {
				// The page and its manifest name the current build's files.
				"no-cache"
			}),
		);
		response
	}
}

fn missing_page(root: Option<&Path>) -> Response<Body> {
	let _ = root;
	let html = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Pyroshine</title>\
		<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"></head>\
		<body style=\"font-family:sans-serif;max-width:40em;margin:3em auto;padding:0 1em\">\
		<h1>Pyroshine is running</h1><p>The web management interface's files are not installed, so this \
		page cannot show it. Install the <code>pyroshine</code> package (it includes them), or set \
		<code>web_root</code> in the <code>[management]</code> section of the configuration to the \
		directory that contains <code>index.html</code>, then restart Pyroshine.</p>\
		<p>Pairing remains available with <code>pyroshine pair</code> in a terminal.</p></body></html>";
	let mut response = Response::new(body(html));
	*response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
	response.headers_mut().insert(
		header::CONTENT_TYPE,
		HeaderValue::from_static("text/html; charset=utf-8"),
	);
	response
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_indexed_files_are_served() {
		let directory = tempfile::tempdir().unwrap();
		let root = directory.path().join("web");
		std::fs::create_dir_all(root.join("assets")).unwrap();
		std::fs::write(root.join("index.html"), "<html></html>").unwrap();
		std::fs::write(root.join("assets/app-1234.js"), "console.log(1)").unwrap();
		std::fs::write(root.join("pyroshine-web.json"), r#"{"version":"0.0.0-test"}"#).unwrap();
		std::fs::write(root.join("notes.md"), "not served").unwrap();
		std::fs::write(directory.path().join("secret.txt"), "outside").unwrap();
		std::os::unix::fs::symlink(directory.path().join("secret.txt"), root.join("link.txt")).unwrap();
		let assets = Assets::load(Some(&root));
		assert_eq!(assets.version(), Some("0.0.0-test"));

		let index = assets.respond(&Method::GET, "/");
		assert_eq!(index.status(), StatusCode::OK);
		assert_eq!(index.headers()[header::CACHE_CONTROL], "no-cache");
		let script = assets.respond(&Method::GET, "/assets/app-1234.js");
		assert!(
			script.headers()[header::CACHE_CONTROL]
				.to_str()
				.unwrap()
				.contains("immutable")
		);
		assert!(
			script.headers()[header::CONTENT_TYPE]
				.to_str()
				.unwrap()
				.starts_with("text/javascript")
		);
		for path in [
			"/../secret.txt",
			"/assets/../../secret.txt",
			"/link.txt",
			"/notes.md",
			"/%2e%2e/secret.txt",
			"//etc/passwd",
		] {
			assert_eq!(
				assets.respond(&Method::GET, path).status(),
				StatusCode::NOT_FOUND,
				"{path}"
			);
		}
		assert_eq!(
			assets.respond(&Method::POST, "/").status(),
			StatusCode::METHOD_NOT_ALLOWED
		);
	}

	#[test]
	fn a_missing_build_explains_itself() {
		let directory = tempfile::tempdir().unwrap();
		let assets = Assets::load(Some(directory.path()));
		let response = assets.respond(&Method::GET, "/");
		assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	}
}

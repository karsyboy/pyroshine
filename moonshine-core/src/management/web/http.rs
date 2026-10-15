//! Request admission, responses and their security headers.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use moonshine_management::ManagementError;
use serde::Serialize;

use super::Context;
use crate::management::auth::{Auth, AuthError, Session};

pub(crate) type Body = BoxBody<Bytes, Infallible>;

/// Cookie holding the session token. Over HTTPS the `__Host-` prefix makes
/// browsers refuse a cookie that is not `Secure`, host-only and `Path=/`.
pub(crate) const COOKIE: &str = "pyroshine_session";
pub(crate) const SECURE_COOKIE: &str = "__Host-pyroshine_session";
pub(crate) const CSRF_HEADER: &str = "x-csrf-token";
/// Largest JSON body: a complete configuration with many applications.
pub(crate) const MAX_BODY: usize = 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// The frontend runs only its own bundled code: no inline scripts, no
/// third-party origins, never framed. Material UI injects `<style>` tags.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
	 img-src 'self' data:; font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
	 form-action 'self'; frame-ancestors 'none'";

pub(crate) fn body(bytes: impl Into<Bytes>) -> Body {
	Full::new(bytes.into()).boxed()
}

/// Host names browsers may use. Checking `Host` defeats DNS rebinding: a
/// page from an attacker's domain that resolves to this host still sends its
/// own name, which is not listed.
pub(crate) struct Hosts {
	port: u16,
	/// The machine's own names, accepted when the listener is reachable from
	/// other devices.
	own_names: Vec<String>,
	/// `[management] allowed_hosts`: lowercase `name` or `name:port`.
	allowed: Vec<String>,
}

pub(crate) fn valid_host_entry(entry: &str) -> bool {
	let (name, port) = match entry.rsplit_once(':') {
		Some((name, port)) => (name, Some(port)),
		None => (entry, None),
	};
	!name.is_empty()
		&& name.len() <= 253
		&& name.split('.').all(|label| {
			!label.is_empty() && label.len() <= 63 && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
		}) && port.is_none_or(|port| port.parse::<u16>().is_ok_and(|port| port != 0))
}

impl Hosts {
	pub(crate) fn new(port: u16, bound: IpAddr, allowed: &[String]) -> Self {
		let own_names = if bound.to_canonical().is_loopback() {
			Vec::new()
		} else {
			let name = gethostname::gethostname().to_string_lossy().to_ascii_lowercase();
			if name.is_empty() {
				Vec::new()
			} else {
				vec![format!("{name}.local"), name]
			}
		};
		Self {
			port,
			own_names,
			allowed: allowed.iter().map(|entry| entry.to_ascii_lowercase()).collect(),
		}
	}

	/// Whether a request's `Host` value names this interface.
	pub(crate) fn accepts(&self, authority: &str) -> bool {
		let authority = authority.to_ascii_lowercase();
		if self.allowed.contains(&authority) {
			return true;
		}
		let (host, port) = match split_authority(&authority) {
			Some(parts) => parts,
			None => return false,
		};
		if port.is_some_and(|port| port != self.port) && !self.allowed.iter().any(|entry| entry == host) {
			return false;
		}
		// An IP literal cannot come from DNS rebinding.
		host.parse::<IpAddr>().is_ok()
			|| host == "localhost"
			|| self.own_names.iter().any(|name| name == host)
			|| self.allowed.iter().any(|entry| entry == host)
	}
}

/// `host[:port]`, with IPv6 literals in brackets. The host is returned
/// without brackets.
fn split_authority(authority: &str) -> Option<(&str, Option<u16>)> {
	if let Some(rest) = authority.strip_prefix('[') {
		let (host, rest) = rest.split_once(']')?;
		return match rest {
			"" => Some((host, None)),
			_ => Some((host, Some(rest.strip_prefix(':')?.parse().ok()?))),
		};
	}
	match authority.split_once(':') {
		Some((host, port)) => Some((host, Some(port.parse().ok()?))),
		None => Some((authority, None)),
	}
}

/// API error document: `{"error": {"kind": "...", "message": "..."}}`.
#[derive(Serialize)]
struct ErrorBody<'a> {
	error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
	kind: &'a str,
	message: &'a str,
}

pub(crate) fn error(status: StatusCode, kind: &str, message: &str) -> Response<Body> {
	let json = serde_json::to_vec(&ErrorBody {
		error: ErrorDetail { kind, message },
	})
	.expect("errors serialize");
	let mut response = Response::new(body(json));
	*response.status_mut() = status;
	response
		.headers_mut()
		.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
	response
}

pub(crate) fn management_error(failure: &ManagementError) -> Response<Body> {
	let status = match failure {
		ManagementError::AccessDenied(_) => StatusCode::FORBIDDEN,
		ManagementError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
		ManagementError::NotFound(_) => StatusCode::NOT_FOUND,
		ManagementError::Conflict(_) => StatusCode::CONFLICT,
		ManagementError::ReadOnly(_) => StatusCode::CONFLICT,
		ManagementError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
		ManagementError::ZBus(_) => StatusCode::SERVICE_UNAVAILABLE,
	};
	error(status, failure.kind(), &failure.message())
}

pub(crate) fn auth_error(failure: &AuthError) -> Response<Body> {
	let status = match failure {
		AuthError::Unauthenticated | AuthError::InvalidCredentials => StatusCode::UNAUTHORIZED,
		AuthError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
		AuthError::NotConfigured => StatusCode::CONFLICT,
		AuthError::Weak(_) => StatusCode::UNPROCESSABLE_ENTITY,
		AuthError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
	};
	let mut response = error(status, failure.kind(), &failure.message());
	if let AuthError::RateLimited(after) = failure {
		response
			.headers_mut()
			.insert(header::RETRY_AFTER, HeaderValue::from(after.as_secs().max(1)));
	}
	response
}

pub(crate) fn json<T: Serialize>(value: &T) -> Response<Body> {
	json_bytes(serde_json::to_vec(value).expect("management documents serialize"))
}

/// A JSON document that is already serialized.
pub(crate) fn json_bytes(bytes: impl Into<Bytes>) -> Response<Body> {
	let mut response = Response::new(body(bytes));
	response
		.headers_mut()
		.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
	response
}

pub(crate) fn no_content() -> Response<Body> {
	let mut response = Response::new(body(Bytes::new()));
	*response.status_mut() = StatusCode::NO_CONTENT;
	response
}

/// Read a JSON body of at most `limit` bytes.
// A rejection is the complete response, returned at most once per request.
#[allow(clippy::result_large_err)]
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
	request: Request<Incoming>,
	limit: usize,
) -> Result<T, Response<Body>> {
	let is_json = request
		.headers()
		.get(header::CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.split(';').next())
		.is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
	// Only JSON: an HTML form cannot send it without a CORS preflight, which
	// this server never approves.
	if !is_json {
		return Err(error(
			StatusCode::UNSUPPORTED_MEDIA_TYPE,
			"invalid",
			"Send the request as application/json.",
		));
	}
	let collected = tokio::time::timeout(BODY_TIMEOUT, Limited::new(request.into_body(), limit).collect()).await;
	let bytes = match collected {
		Ok(Ok(collected)) => collected.to_bytes(),
		Ok(Err(_)) => {
			return Err(error(
				StatusCode::PAYLOAD_TOO_LARGE,
				"invalid",
				"The request is too large.",
			));
		},
		Err(_) => {
			return Err(error(
				StatusCode::REQUEST_TIMEOUT,
				"invalid",
				"The request body did not arrive in time.",
			));
		},
	};
	serde_json::from_slice(&bytes).map_err(|parse| {
		error(
			StatusCode::BAD_REQUEST,
			"invalid",
			&format!("The request is not valid: {parse}"),
		)
	})
}

/// The session token from the `Cookie` headers, if any.
pub(crate) fn session_token(headers: &HeaderMap, https: bool) -> Option<String> {
	let name = if https { SECURE_COOKIE } else { COOKIE };
	headers
		.get_all(header::COOKIE)
		.iter()
		.filter_map(|value| value.to_str().ok())
		.flat_map(|value| value.split(';'))
		.filter_map(|pair| pair.trim().split_once('='))
		.find(|(key, _)| *key == name)
		.map(|(_, value)| value.trim().to_string())
}

pub(crate) fn session_cookie(token: &str, https: bool, max_age: Duration) -> HeaderValue {
	let value = if https {
		format!(
			"{SECURE_COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
			max_age.as_secs()
		)
	} else {
		format!(
			"{COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict",
			max_age.as_secs()
		)
	};
	HeaderValue::from_str(&value).expect("hex token")
}

pub(crate) fn clear_cookie(https: bool) -> HeaderValue {
	HeaderValue::from_static(if https {
		"__Host-pyroshine_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict"
	} else {
		"pyroshine_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict"
	})
}

/// Whether the method changes state (and needs the same-origin checks).
pub(crate) fn changes_state(method: &Method) -> bool {
	!matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Browser provenance checks. Changes must come from this origin: `Origin`,
/// when sent, must be this interface (`scheme://Host`), and `Sec-Fetch-Site`
/// must not be cross-site. Reads are checked against `Sec-Fetch-Site` too, so
/// another site cannot even probe the API with credentials. Clients that are
/// not browsers send neither header; they still need a session and, for
/// changes, its CSRF token.
pub(crate) fn same_origin(headers: &HeaderMap, https: bool, mutating: bool) -> Result<(), &'static str> {
	if let Some(site) = headers.get("sec-fetch-site") {
		let allowed: &[&[u8]] = if mutating {
			&[b"same-origin"]
		} else {
			&[b"same-origin", b"none"]
		};
		if !allowed.contains(&site.as_bytes()) {
			return Err("cross-site request");
		}
	}
	if let Some(origin) = headers.get(header::ORIGIN) {
		let origin = origin.to_str().map_err(|_| "invalid Origin")?;
		let host = headers
			.get(header::HOST)
			.and_then(|value| value.to_str().ok())
			.ok_or("missing Host")?;
		let scheme = if https { "https" } else { "http" };
		if !origin.eq_ignore_ascii_case(&format!("{scheme}://{host}")) {
			return Err("cross-origin request");
		}
	}
	Ok(())
}

/// The authenticated session of a request, or the response refusing it.
/// Changes additionally need the session's CSRF token.
#[allow(clippy::result_large_err)]
pub(crate) fn authenticate(
	context: &Context,
	headers: &HeaderMap,
	mutating: bool,
) -> Result<(Session, String), Response<Body>> {
	let Some(token) = session_token(headers, context.https) else {
		return Err(auth_error(&AuthError::Unauthenticated));
	};
	let session = context
		.management
		.auth
		.authenticate(&token)
		.map_err(|failure| auth_error(&failure))?;
	if mutating {
		let submitted = headers
			.get(CSRF_HEADER)
			.and_then(|value| value.to_str().ok())
			.unwrap_or_default();
		if !Auth::csrf_matches(&token, submitted) {
			tracing::warn!(target: "pyroshine::audit", session = session.id, "Rejected a web management change without a valid CSRF token");
			return Err(error(
				StatusCode::FORBIDDEN,
				"csrf",
				"The request did not come from the Pyroshine page. Reload the page and try again.",
			));
		}
	}
	Ok((session, token))
}

/// Headers every response carries.
fn secure(response: &mut Response<Body>, api: bool) {
	let headers = response.headers_mut();
	headers.insert(
		header::CONTENT_SECURITY_POLICY,
		HeaderValue::from_static(CONTENT_SECURITY_POLICY),
	);
	headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
	headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
	headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
	headers.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
	headers.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
	headers.insert(
		"permissions-policy",
		HeaderValue::from_static("camera=(), microphone=(), geolocation=(), interest-cohort=()"),
	);
	if api && !headers.contains_key(header::CACHE_CONTROL) {
		headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	}
}

/// Serve one request.
pub(crate) async fn handle(context: Arc<Context>, request: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
	let host_ok = request
		.headers()
		.get(header::HOST)
		.and_then(|value| value.to_str().ok())
		.is_some_and(|host| context.hosts.accepts(host));
	let api = request.uri().path().starts_with("/api/");
	let mut response = if !host_ok {
		tracing::debug!(%peer, host = ?request.headers().get(header::HOST), "Web management: unknown Host rejected");
		error(
			StatusCode::MISDIRECTED_REQUEST,
			"invalid_host",
			"Open Pyroshine through its own address (for example http://127.0.0.1:47990/), \
			 or add this name to [management] allowed_hosts.",
		)
	} else if api {
		super::api::handle(&context, request, peer).await
	} else {
		context.assets.respond(request.method(), request.uri().path())
	};
	secure(&mut response, api);
	response
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hosts_accept_loopback_literals_and_listed_names_on_the_port() {
		let hosts = Hosts::new(
			47990,
			"127.0.0.1".parse().unwrap(),
			&["streambox.lan".into(), "proxy.example:443".into()],
		);
		for accepted in [
			"127.0.0.1:47990",
			"localhost:47990",
			"LOCALHOST:47990",
			"[::1]:47990",
			"192.168.1.20:47990",
			"streambox.lan",
			"streambox.lan:8443",
			"proxy.example:443",
		] {
			assert!(hosts.accepts(accepted), "{accepted}");
		}
		for rejected in [
			"attacker.example:47990",
			"localhost:47989",
			"127.0.0.1:80",
			"localhost.attacker.example:47990",
			"proxy.example:444",
			"[::1",
			"127.0.0.1:notaport",
			"",
		] {
			assert!(!hosts.accepts(rejected), "{rejected}");
		}
	}

	#[test]
	fn host_entries_are_validated() {
		assert!(valid_host_entry("streambox.lan"));
		assert!(valid_host_entry("streambox:8443"));
		assert!(!valid_host_entry("http://streambox"));
		assert!(!valid_host_entry("bad host"));
		assert!(!valid_host_entry("host:0"));
		assert!(!valid_host_entry(""));
	}

	fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
		let mut headers = HeaderMap::new();
		for (name, value) in pairs {
			headers.append(*name, HeaderValue::from_static(value));
		}
		headers
	}

	#[test]
	fn changes_must_come_from_this_origin() {
		let same = headers(&[
			("host", "127.0.0.1:47990"),
			("origin", "http://127.0.0.1:47990"),
			("sec-fetch-site", "same-origin"),
		]);
		assert!(same_origin(&same, false, true).is_ok());
		// HTTPS changes the expected origin.
		assert!(same_origin(&same, true, true).is_err());
		let cross = headers(&[("host", "127.0.0.1:47990"), ("origin", "http://evil.example")]);
		assert_eq!(same_origin(&cross, false, true), Err("cross-origin request"));
		let site = headers(&[("host", "127.0.0.1:47990"), ("sec-fetch-site", "cross-site")]);
		assert_eq!(same_origin(&site, false, true), Err("cross-site request"));
		assert_eq!(same_origin(&site, false, false), Err("cross-site request"));
		// Typing the address (navigation) is not a change.
		let typed = headers(&[("host", "127.0.0.1:47990"), ("sec-fetch-site", "none")]);
		assert!(same_origin(&typed, false, false).is_ok());
		assert!(same_origin(&typed, false, true).is_err());
		// Non-browser clients send neither header.
		assert!(same_origin(&headers(&[("host", "127.0.0.1:47990")]), false, true).is_ok());
	}

	#[test]
	fn the_session_cookie_is_read_by_its_exact_name() {
		let plain = headers(&[("cookie", "other=1; pyroshine_session=abc; x=y")]);
		assert_eq!(session_token(&plain, false).as_deref(), Some("abc"));
		assert_eq!(session_token(&plain, true), None);
		let secure = headers(&[("cookie", "__Host-pyroshine_session=def")]);
		assert_eq!(session_token(&secure, true).as_deref(), Some("def"));
		let lookalike = headers(&[("cookie", "xpyroshine_session=abc")]);
		assert_eq!(session_token(&lookalike, false), None);
		let cookie = session_cookie("00ff", true, Duration::from_secs(60));
		let cookie = cookie.to_str().unwrap();
		assert!(cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Strict"));
	}
}

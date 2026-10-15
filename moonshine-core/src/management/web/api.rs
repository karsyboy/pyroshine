//! `/api/v1/`: the management API.
//!
//! Requests and responses are JSON; resources are the
//! `moonshine_management::dto` documents the D-Bus interface also returns.
//! Errors are `{"error": {"kind", "message"}}` with an HTTP status matching
//! the kind (see `http::management_error`). Everything except the sign-in
//! routes requires a session; changes also require its CSRF token
//! (`X-CSRF-Token`, from `GET /api/v1/auth`).
//!
//! | Route | |
//! | --- | --- |
//! | `GET auth`, `POST auth/setup`, `POST auth/login` | Sign-in state, first-run setup, sign-in |
//! | `POST auth/logout`, `POST auth/password` | Sign out; change the password (signs out other browsers) |
//! | `GET auth/sessions`, `DELETE auth/sessions/{id}` | Signed-in browsers; sign one out |
//! | `GET overview` | Server, session, pairing, clients and statistics at once |
//! | `GET server`, `GET session`, `POST session/end` | |
//! | `GET pairing`, `POST pairing/approve`, `POST pairing/reject` | Exact request token required |
//! | `GET clients`, `POST clients/rename`, `POST clients/revoke` | |
//! | `GET config`, `POST config/validate`, `PUT config` | `PUT` needs the revision it edits |
//! | `GET library`, `POST library/rescan`, `POST library/hidden` | |
//! | `GET events/recent`, `POST health/run`, `GET diagnostics/report`, `POST diagnostics/bundle` | |
//! | `GET stream` | Server-Sent Events (see `events`) |

use std::net::SocketAddr;

use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Context;
use super::http::{
	Body, MAX_BODY, auth_error, authenticate, changes_state, clear_cookie, error, json, json_bytes, management_error,
	no_content, read_json, same_origin, session_cookie,
};
use crate::management::auth::{AuthStatus, MAX_LIFETIME};

/// Body limit of everything but configuration saves.
const SMALL_BODY: usize = 16 * 1024;

#[derive(Deserialize)]
struct SetupRequest {
	code: String,
	password: String,
}

#[derive(Deserialize)]
struct LoginRequest {
	password: String,
}

#[derive(Deserialize)]
struct PasswordRequest {
	current: String,
	new: String,
}

#[derive(Deserialize)]
struct PairingRequest {
	client_id: String,
	request: String,
	#[serde(default)]
	pin: String,
	#[serde(default)]
	label: String,
}

#[derive(Deserialize)]
struct ClientRequest {
	fingerprint: String,
	#[serde(default)]
	label: String,
}

#[derive(Deserialize)]
struct ValuesRequest {
	values: Value,
}

#[derive(Deserialize)]
struct SaveRequest {
	values: Value,
	revision: String,
}

#[derive(Deserialize)]
struct HiddenRequest {
	key: String,
	hidden: bool,
}

fn agent(request: &Request<Incoming>) -> String {
	request
		.headers()
		.get(header::USER_AGENT)
		.and_then(|value| value.to_str().ok())
		.unwrap_or_default()
		.to_string()
}

fn status_json(status: AuthStatus, authenticated: Option<(&str, &crate::management::auth::Session)>) -> Value {
	json!({
		"configured": status.configured,
		"setup_pending": status.setup_pending,
		"locked": status.locked,
		"min_password_length": status.min_password_length,
		"authenticated": authenticated.is_some(),
		"csrf_token": authenticated.map(|(token, _)| crate::management::auth::Auth::csrf_token(token)),
		"session": authenticated.map(|(_, session)| json!({
			"id": session.id,
			"expires_at_ms": session.expires_at_ms,
		})),
		"api_version": moonshine_management::API_VERSION,
	})
}

/// Sign-in succeeded: the session cookie plus the new sign-in state.
fn signed_in(context: &Context, created: crate::management::auth::NewSession) -> Response<Body> {
	let mut response = json(&status_json(
		context.management.auth.status(),
		Some((&created.token, &created.session)),
	));
	response.headers_mut().insert(
		header::SET_COOKIE,
		session_cookie(&created.token, context.https, MAX_LIFETIME),
	);
	response
}

pub(crate) async fn handle(context: &Context, request: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
	let method = request.method().clone();
	let path = request.uri().path().to_string();
	let Some(route) = path.strip_prefix("/api/v1/") else {
		return error(StatusCode::NOT_FOUND, "not_found", "Unknown API version.");
	};
	let mutating = changes_state(&method);
	if let Err(reason) = same_origin(request.headers(), context.https, mutating) {
		tracing::warn!(target: "pyroshine::audit", %peer, reason, route, "Rejected a cross-origin web management request");
		return error(
			StatusCode::FORBIDDEN,
			"cross_origin",
			"This request did not come from the Pyroshine page.",
		);
	}
	let management = &context.management;
	let auth = &management.auth;

	// Routes available without a session.
	match (&method, route) {
		(&Method::GET, "auth") => {
			let token = super::http::session_token(request.headers(), context.https);
			let session = token
				.as_deref()
				.and_then(|token| auth.authenticate(token).ok().map(|session| (token, session)));
			let mut response = json(&status_json(
				auth.status(),
				session.as_ref().map(|(token, session)| (*token, session)),
			));
			if token.is_some() && session.is_none() {
				response
					.headers_mut()
					.insert(header::SET_COOKIE, clear_cookie(context.https));
			}
			return response;
		},
		(&Method::POST, "auth/setup") => {
			let agent = agent(&request);
			let setup: SetupRequest = match read_json(request, SMALL_BODY).await {
				Ok(setup) => setup,
				Err(response) => return response,
			};
			return match auth.setup(&setup.code, &setup.password, peer.ip(), &agent).await {
				Ok(created) => signed_in(context, created),
				Err(failure) => auth_error(&failure),
			};
		},
		(&Method::POST, "auth/login") => {
			let agent = agent(&request);
			let login: LoginRequest = match read_json(request, SMALL_BODY).await {
				Ok(login) => login,
				Err(response) => return response,
			};
			return match auth.login(&login.password, peer.ip(), &agent).await {
				Ok(created) => signed_in(context, created),
				Err(failure) => auth_error(&failure),
			};
		},
		_ => {},
	}

	let (session, _token) = match authenticate(context, request.headers(), mutating) {
		Ok(authenticated) => authenticated,
		Err(response) => return response,
	};
	let actor = format!("web:{}", session.id);
	macro_rules! body {
		($ty:ty) => {
			body!($ty, SMALL_BODY)
		};
		($ty:ty, $limit:expr) => {
			match read_json::<$ty>(request, $limit).await {
				Ok(value) => value,
				Err(response) => return response,
			}
		};
	}
	macro_rules! audit {
		($action:literal $(, $($field:tt)*)?) => {
			tracing::info!(target: "pyroshine::audit", %peer, session = session.id, action = $action $(, $($field)*)?, "Web management action")
		};
	}
	let respond = |result: Result<Response<Body>, moonshine_management::ManagementError>| {
		result.unwrap_or_else(|failure| management_error(&failure))
	};

	match (&method, route) {
		(&Method::POST, "auth/logout") => {
			auth.logout(&session);
			let mut response = no_content();
			response
				.headers_mut()
				.insert(header::SET_COOKIE, clear_cookie(context.https));
			response
		},
		(&Method::POST, "auth/password") => {
			let change = body!(PasswordRequest);
			match auth
				.change_password(&session, &change.current, &change.new, peer.ip())
				.await
			{
				Ok(()) => no_content(),
				Err(failure) => auth_error(&failure),
			}
		},
		(&Method::GET, "auth/sessions") => json(&auth.sessions(&session)),
		(&Method::DELETE, route) if route.starts_with("auth/sessions/") => {
			let id = &route["auth/sessions/".len()..];
			respond(auth.revoke(id, &session).map(|()| no_content()))
		},

		(&Method::GET, "overview") => json(&json!({
			"connected": true,
			"server": management.op_server(),
			"session": management.op_session(),
			"pairing": management.op_pairing(),
			"clients": management.op_clients().ok(),
			"stats": management.op_stats(),
			"web": {
				"url": management.web_url(),
				"assets_version": context.assets.version(),
			},
		})),
		(&Method::GET, "server") => json(&management.op_server()),
		(&Method::GET, "session") => json(&management.op_session()),
		(&Method::POST, "session/end") => {
			audit!("end_session");
			respond(management.op_end_session(&actor).await.map(|()| no_content()))
		},

		(&Method::GET, "pairing") => json(&management.op_pairing()),
		(&Method::POST, "pairing/approve") => {
			let approval = body!(PairingRequest);
			audit!("approve_pairing");
			respond(
				management
					.op_approve_pairing(
						&approval.client_id,
						&approval.request,
						&approval.pin,
						&approval.label,
						&actor,
					)
					.map(|()| no_content()),
			)
		},
		(&Method::POST, "pairing/reject") => {
			let rejection = body!(PairingRequest);
			audit!("reject_pairing");
			respond(
				management
					.op_reject_pairing(&rejection.client_id, &rejection.request, &actor)
					.map(|()| no_content()),
			)
		},

		(&Method::GET, "clients") => respond(management.op_clients().map(|clients| json(&clients))),
		(&Method::POST, "clients/rename") => {
			let rename = body!(ClientRequest);
			audit!("rename_client");
			respond(
				management
					.op_rename_client(&rename.fingerprint, &rename.label)
					.map(|()| no_content()),
			)
		},
		(&Method::POST, "clients/revoke") => {
			let revoke = body!(ClientRequest);
			audit!("revoke_client");
			respond(
				management
					.op_revoke_client(&revoke.fingerprint, &actor)
					.await
					.map(|()| no_content()),
			)
		},

		(&Method::GET, "config") => respond(management.op_config().map(|document| {
			json(&json!({
				"document": document,
				"schema": management.op_config_schema(),
			}))
		})),
		(&Method::POST, "config/validate") => {
			let validate = body!(ValuesRequest, MAX_BODY);
			respond(
				management
					.op_validate_config(&validate.values.to_string())
					.map(|report| json(&report)),
			)
		},
		(&Method::PUT, "config") => {
			let save = body!(SaveRequest, MAX_BODY);
			audit!("save_config");
			respond(
				management
					.op_save_config(&save.values.to_string(), &save.revision, &actor)
					.await
					.map(|outcome| json(&outcome)),
			)
		},

		(&Method::GET, "library") => json(&management.op_library()),
		(&Method::POST, "library/rescan") => {
			audit!("rescan_library");
			respond(management.op_rescan_library().map(|()| no_content()))
		},
		(&Method::POST, "library/hidden") => {
			let hidden = body!(HiddenRequest);
			audit!("set_application_hidden", hidden = hidden.hidden);
			respond(
				management
					.op_set_application_hidden(&hidden.key, hidden.hidden)
					.await
					.map(|()| no_content()),
			)
		},

		(&Method::GET, "events/recent") => json(&management.op_events()),
		(&Method::POST, "health/run") => {
			audit!("run_health_check");
			json(&management.op_run_health_check().await)
		},
		(&Method::GET, "diagnostics/report") => json_bytes(management.op_diagnostic_report().await),
		// POST: collecting is expensive and the archive holds logs, so it
		// needs the CSRF token like a change.
		(&Method::POST, "diagnostics/bundle") => {
			audit!("support_bundle");
			match management.op_support_bundle().await {
				Ok(archive) => {
					let seconds = std::time::SystemTime::now()
						.duration_since(std::time::UNIX_EPOCH)
						.map_or(0, |elapsed| elapsed.as_secs());
					let name = moonshine_management::support_bundle_name(seconds);
					let mut response = Response::new(super::http::body(archive));
					let headers = response.headers_mut();
					headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
					headers.insert(
						header::CONTENT_DISPOSITION,
						HeaderValue::from_str(&format!("attachment; filename=\"{name}.tar.gz\""))
							.expect("bundle names are ASCII"),
					);
					response
				},
				Err(failure) => management_error(&failure),
			}
		},

		(&Method::GET, "stream") => super::events::stream(context, session).await,

		(_, route) if is_known(route) => error(
			StatusCode::METHOD_NOT_ALLOWED,
			"invalid",
			"This method is not supported here.",
		),
		_ => error(StatusCode::NOT_FOUND, "not_found", "Unknown management API route."),
	}
}

fn is_known(route: &str) -> bool {
	const ROUTES: &[&str] = &[
		"auth",
		"auth/setup",
		"auth/login",
		"auth/logout",
		"auth/password",
		"auth/sessions",
		"overview",
		"server",
		"session",
		"session/end",
		"pairing",
		"pairing/approve",
		"pairing/reject",
		"clients",
		"clients/rename",
		"clients/revoke",
		"config",
		"config/validate",
		"library",
		"library/rescan",
		"library/hidden",
		"events/recent",
		"health/run",
		"diagnostics/report",
		"diagnostics/bundle",
		"stream",
	];
	ROUTES.contains(&route) || route.starts_with("auth/sessions/")
}

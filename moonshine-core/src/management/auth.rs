//! Administrator authentication for the web management interface.
//!
//! One administrator password protects every management operation in the
//! browser. Nothing else grants access: not a loopback address (other local
//! users and the streamed applications share it), not a paired Moonlight
//! certificate, not the desktop tray (it only opens pages).
//!
//! - **First run.** No password exists until the operator sets one with a
//!   one-time *setup code*. Only the user running Pyroshine can obtain a code
//!   (`pyroshine web setup` or the tray, both over the same-user D-Bus
//!   interface), so setting the password proves control of that account
//!   rather than of a browser on the host. `--reset` issues a code that
//!   replaces a forgotten password and signs out every session.
//! - **Verifier.** The password is stored as PBKDF2-HMAC-SHA256 with a random
//!   salt (`management.json` next to `state.toml`, mode 0600). Verification
//!   runs on blocking threads, at most two at a time, so password guessing
//!   cannot starve the server.
//! - **Sessions.** Signing in creates a new random 256-bit session token (a
//!   pre-existing cookie is never adopted, so a planted token cannot be
//!   fixed), sent as an `HttpOnly`, `SameSite=Strict` cookie. Only its
//!   SHA-256 is stored. Sessions expire after [`IDLE_TIMEOUT`] without use and
//!   [`MAX_LIFETIME`] after sign-in; signing out, changing the password and
//!   revoking a session from the UI end them at once. The CSRF token is
//!   derived from the session token, so it needs no storage and dies with it.
//! - **Guessing.** Failed attempts (password or setup code) lock the peer
//!   address out with growing delays, and a global budget bounds distributed
//!   guessing. Failures are logged without the submitted value.
//!
//! The file is read once at startup. If it cannot be parsed, management fails
//! closed (no sign-in) until a reset code replaces it.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use aws_lc_rs::{digest, pbkdf2, rand};
use moonshine_management::ManagementError;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};

use super::telemetry::unix_millis;

/// Sessions end after this long without a request.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);
/// Sessions end this long after sign-in, however active.
pub(crate) const MAX_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);
/// Signed-in browsers kept; the least recently used is signed out first.
const MAX_SESSIONS: usize = 32;
/// A setup code is valid this long, and for this many wrong guesses.
const SETUP_CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);
const SETUP_CODE_ATTEMPTS: u32 = 5;
/// PBKDF2-HMAC-SHA256 work factor (OWASP's 2023 recommendation). Each
/// verifier records its own count, so raising it keeps old passwords valid.
#[cfg(not(test))]
const ITERATIONS: u32 = 600_000;
/// Unit tests hash many passwords in unoptimized builds.
#[cfg(test)]
const ITERATIONS: u32 = 1_000;
const ALGORITHM: &str = "pbkdf2-sha256";
/// Password length bounds, in characters.
pub(crate) const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 1024;
/// Failures a peer may make in [`FAILURE_WINDOW`] before it is locked out.
const FREE_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
const FIRST_LOCKOUT: Duration = Duration::from_secs(30);
const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Failures from all peers together within [`GLOBAL_WINDOW`] before every
/// attempt waits.
const GLOBAL_FAILURES: usize = 50;
const GLOBAL_WINDOW: Duration = Duration::from_secs(5 * 60);
/// `last_used` is written back at most this often per session.
const TOUCH_INTERVAL: Duration = Duration::from_secs(3600);
/// Crockford's base32 alphabet: no I, L, O or U to misread.
const CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Where the management credentials live: beside the pairing state, so an
/// isolated test instance (its own `state.toml`) has its own administrator.
pub(crate) fn auth_path(state_path: &Path) -> PathBuf {
	crate::durable::directory(state_path).join("management.json")
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuthError {
	/// No valid session.
	Unauthenticated,
	/// Wrong password or setup code.
	InvalidCredentials,
	/// Too many failures; try again after the delay.
	RateLimited(Duration),
	/// No administrator password is set yet.
	NotConfigured,
	/// The new password does not meet the policy.
	Weak(String),
	/// The credentials could not be saved or read.
	Storage(String),
}

impl AuthError {
	pub(crate) fn kind(&self) -> &'static str {
		match self {
			Self::Unauthenticated => "unauthenticated",
			Self::InvalidCredentials => "invalid_credentials",
			Self::RateLimited(_) => "rate_limited",
			Self::NotConfigured => "not_configured",
			Self::Weak(_) => "invalid",
			Self::Storage(_) => "failed",
		}
	}

	pub(crate) fn message(&self) -> String {
		match self {
			Self::Unauthenticated => "Sign in to manage Pyroshine.".into(),
			Self::InvalidCredentials => "The password or setup code is not correct.".into(),
			Self::RateLimited(after) => format!(
				"Too many failed attempts. Try again in {} seconds.",
				after.as_secs().max(1)
			),
			Self::NotConfigured => {
				"No administrator password is set. Run `pyroshine web setup` on the host to get a setup code.".into()
			},
			Self::Weak(reason) | Self::Storage(reason) => reason.clone(),
		}
	}
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Stored {
	version: u32,
	administrator: Option<Verifier>,
	#[serde(default)]
	sessions: Vec<StoredSession>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Verifier {
	algorithm: String,
	iterations: u32,
	salt: String,
	hash: String,
	changed_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSession {
	/// SHA-256 of the session token, hex.
	id: String,
	created_ms: u64,
	last_used_ms: u64,
	/// Where the browser signed in from, for the session list.
	address: String,
	/// A short, sanitized `User-Agent` for the session list.
	agent: String,
}

/// A browser session the operator can see and revoke (never its token).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionSummary {
	/// Public identifier: a prefix of the token's hash.
	pub id: String,
	pub created_at_ms: u64,
	pub last_used_at_ms: u64,
	pub expires_at_ms: u64,
	pub address: String,
	pub agent: String,
	pub current: bool,
}

/// An authenticated request's session.
#[derive(Clone, Debug)]
pub(crate) struct Session {
	/// Public identifier (see [`SessionSummary::id`]), used in the audit log.
	pub id: String,
	/// Full hash, the stored key.
	key: String,
	pub expires_at_ms: u64,
}

/// A session created by sign-in or setup: the token goes into the cookie.
pub(crate) struct NewSession {
	pub token: String,
	pub session: Session,
}

#[derive(Debug)]
struct SetupCode {
	digest: Vec<u8>,
	expires: Instant,
	attempts: u32,
	reset: bool,
}

#[derive(Debug, Default)]
struct Failures {
	count: u32,
	first: Option<Instant>,
	locked_until: Option<Instant>,
}

struct Inner {
	stored: Stored,
	/// Why the file could not be read; sign-in is refused until a reset.
	load_error: Option<String>,
	setup: Option<SetupCode>,
	failures: HashMap<IpAddr, Failures>,
	global_failures: VecDeque<Instant>,
	/// When each session's `last_used` was last written back.
	touched: HashMap<String, Instant>,
}

pub(crate) struct Auth {
	path: PathBuf,
	inner: Mutex<Inner>,
	/// Bounds concurrent PBKDF2 work.
	verifying: Semaphore,
	/// Bumped whenever sessions end, so open event streams recheck theirs.
	revocations: watch::Sender<u64>,
}

/// What the sign-in page needs to know.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthStatus {
	/// An administrator password exists.
	pub configured: bool,
	/// A setup code was issued and is still valid.
	pub setup_pending: bool,
	/// The credentials file could not be read; only a reset code helps.
	pub locked: Option<String>,
	pub min_password_length: usize,
}

fn now_ms() -> u64 {
	unix_millis(SystemTime::now())
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
	digest::digest(&digest::SHA256, bytes).as_ref().to_vec()
}

fn random_bytes<const N: usize>() -> [u8; N] {
	let mut bytes = [0u8; N];
	rand::fill(&mut bytes).expect("the system random generator works");
	bytes
}

/// Compare without an early exit, so timing does not reveal a prefix.
fn same(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Hash `password` for storage. Runs PBKDF2: call off the async workers.
fn derive(password: &str) -> Verifier {
	let salt = random_bytes::<16>();
	let mut hash = [0u8; 32];
	pbkdf2::derive(
		pbkdf2::PBKDF2_HMAC_SHA256,
		NonZeroU32::new(ITERATIONS).expect("non-zero"),
		&salt,
		password.as_bytes(),
		&mut hash,
	);
	Verifier {
		algorithm: ALGORITHM.into(),
		iterations: ITERATIONS,
		salt: hex::encode(salt),
		hash: hex::encode(hash),
		changed_at_ms: now_ms(),
	}
}

/// Check `password` against `verifier`. Runs PBKDF2: call off the async workers.
fn verify(verifier: &Verifier, password: &str) -> bool {
	let (Ok(salt), Ok(hash), Some(iterations)) = (
		hex::decode(&verifier.salt),
		hex::decode(&verifier.hash),
		NonZeroU32::new(verifier.iterations),
	) else {
		return false;
	};
	verifier.algorithm == ALGORITHM
		&& pbkdf2::verify(
			pbkdf2::PBKDF2_HMAC_SHA256,
			iterations,
			&salt,
			password.as_bytes(),
			&hash,
		)
		.is_ok()
}

fn check_password(password: &str) -> Result<(), AuthError> {
	let length = password.chars().count();
	if length < MIN_PASSWORD {
		return Err(AuthError::Weak(format!(
			"Choose a password of at least {MIN_PASSWORD} characters."
		)));
	}
	if length > MAX_PASSWORD {
		return Err(AuthError::Weak(format!(
			"Choose a password of at most {MAX_PASSWORD} characters."
		)));
	}
	Ok(())
}

/// A setup code as the operator types it: `XXXX-XXXX-XXXX-XXXX`, 80 bits.
fn new_setup_code() -> String {
	let bytes = random_bytes::<16>();
	let symbols: String = bytes
		.iter()
		.map(|byte| CODE_ALPHABET[usize::from(byte & 31)] as char)
		.collect();
	symbols
		.as_bytes()
		.chunks(4)
		.map(|chunk| std::str::from_utf8(chunk).expect("ASCII"))
		.collect::<Vec<_>>()
		.join("-")
}

/// Normalize what the operator typed: case, separators and the letters
/// Crockford's alphabet folds (`I`/`L` are 1, `O` is 0).
fn normalize_code(code: &str) -> String {
	code.chars()
		.filter(|c| !matches!(c, '-' | ' ' | '\t'))
		.map(|c| match c.to_ascii_uppercase() {
			'I' | 'L' => '1',
			'O' => '0',
			other => other,
		})
		.collect()
}

/// A short, printable `User-Agent` excerpt for the session list.
pub(crate) fn sanitize_agent(agent: &str) -> String {
	agent
		.chars()
		.filter(|c| c.is_ascii_graphic() || *c == ' ')
		.take(120)
		.collect()
}

impl Session {
	fn from_key(key: String, created_ms: u64, last_used_ms: u64) -> Self {
		Self {
			id: key[..16].to_string(),
			expires_at_ms: expiry(created_ms, last_used_ms),
			key,
		}
	}
}

fn expiry(created_ms: u64, last_used_ms: u64) -> u64 {
	(created_ms + MAX_LIFETIME.as_millis() as u64).min(last_used_ms + IDLE_TIMEOUT.as_millis() as u64)
}

impl Auth {
	/// Read the credentials at `path`. A missing file means first run.
	pub(crate) fn open(path: PathBuf) -> Self {
		let (stored, load_error) = match std::fs::read(&path) {
			Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
				Ok(stored) => (stored, None),
				Err(error) => {
					tracing::error!(
						path = %path.display(),
						"The web management credentials cannot be read ({error}); sign-in is disabled until \
						 `pyroshine web setup --reset` replaces them"
					);
					(
						Stored::default(),
						Some(format!("{} is damaged: {error}", path.display())),
					)
				},
			},
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Stored::default(), None),
			Err(error) => {
				tracing::error!(path = %path.display(), "The web management credentials cannot be read: {error}");
				(
					Stored::default(),
					Some(format!("{} cannot be read: {error}", path.display())),
				)
			},
		};
		let now = now_ms();
		let mut stored = stored;
		stored
			.sessions
			.retain(|session| expiry(session.created_ms, session.last_used_ms) > now);
		Self {
			path,
			inner: Mutex::new(Inner {
				stored,
				load_error,
				setup: None,
				failures: HashMap::new(),
				global_failures: VecDeque::new(),
				touched: HashMap::new(),
			}),
			verifying: Semaphore::new(2),
			revocations: watch::channel(0).0,
		}
	}

	pub(crate) fn status(&self) -> AuthStatus {
		let mut inner = self.inner.lock().unwrap();
		if inner
			.setup
			.as_ref()
			.is_some_and(|setup| setup.expires <= Instant::now())
		{
			inner.setup = None;
		}
		AuthStatus {
			configured: inner.stored.administrator.is_some() && inner.load_error.is_none(),
			setup_pending: inner.setup.is_some(),
			locked: inner.load_error.clone(),
			min_password_length: MIN_PASSWORD,
		}
	}

	/// Changes when sessions end; event streams recheck their session then.
	pub(crate) fn revocations(&self) -> watch::Receiver<u64> {
		self.revocations.subscribe()
	}

	/// The CSRF token of the session `token` (hex SHA-256 with a context).
	pub(crate) fn csrf_token(token: &str) -> String {
		let mut input = b"pyroshine-management-csrf\0".to_vec();
		input.extend_from_slice(token.as_bytes());
		hex::encode(sha256(&input))
	}

	pub(crate) fn csrf_matches(token: &str, submitted: &str) -> bool {
		same(Self::csrf_token(token).as_bytes(), submitted.as_bytes())
	}

	/// The live session of a cookie `token`, extending its idle deadline.
	pub(crate) fn authenticate(&self, token: &str) -> Result<Session, AuthError> {
		if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			return Err(AuthError::Unauthenticated);
		}
		let key = hex::encode(sha256(token.as_bytes()));
		let now = now_ms();
		let mut inner = self.inner.lock().unwrap();
		if inner.load_error.is_some() {
			return Err(AuthError::Unauthenticated);
		}
		let index = inner
			.stored
			.sessions
			.iter()
			.position(|session| same(session.id.as_bytes(), key.as_bytes()))
			.ok_or(AuthError::Unauthenticated)?;
		let stored = &mut inner.stored.sessions[index];
		if expiry(stored.created_ms, stored.last_used_ms) <= now {
			inner.stored.sessions.remove(index);
			drop(inner);
			self.persist_quietly();
			return Err(AuthError::Unauthenticated);
		}
		stored.last_used_ms = now;
		let session = Session::from_key(key.clone(), stored.created_ms, now);
		let write_back = inner
			.touched
			.get(&key)
			.is_none_or(|last| last.elapsed() >= TOUCH_INTERVAL);
		if write_back {
			inner.touched.insert(key, Instant::now());
			drop(inner);
			self.persist_quietly();
		}
		Ok(session)
	}

	/// Whether `session` is still live (for long-lived event streams).
	pub(crate) fn is_live(&self, session: &Session) -> bool {
		let now = now_ms();
		let inner = self.inner.lock().unwrap();
		inner.load_error.is_none()
			&& inner
				.stored
				.sessions
				.iter()
				.any(|stored| stored.id == session.key && expiry(stored.created_ms, stored.last_used_ms) > now)
	}

	/// Refuse while `peer` (or everyone) is locked out.
	fn admit(&self, peer: IpAddr) -> Result<(), AuthError> {
		let mut inner = self.inner.lock().unwrap();
		let now = Instant::now();
		while inner
			.global_failures
			.front()
			.is_some_and(|at| now.duration_since(*at) > GLOBAL_WINDOW)
		{
			inner.global_failures.pop_front();
		}
		if inner.global_failures.len() >= GLOBAL_FAILURES {
			let oldest = *inner.global_failures.front().expect("non-empty");
			return Err(AuthError::RateLimited(
				GLOBAL_WINDOW.saturating_sub(now.duration_since(oldest)),
			));
		}
		if let Some(until) = inner.failures.get(&peer).and_then(|failures| failures.locked_until)
			&& until > now
		{
			return Err(AuthError::RateLimited(until - now));
		}
		Ok(())
	}

	fn record_failure(&self, peer: IpAddr, what: &str) {
		let mut inner = self.inner.lock().unwrap();
		let now = Instant::now();
		inner.global_failures.push_back(now);
		let failures = inner.failures.entry(peer).or_default();
		if failures
			.first
			.is_none_or(|first| now.duration_since(first) > FAILURE_WINDOW)
		{
			*failures = Failures {
				count: 0,
				first: Some(now),
				locked_until: None,
			};
		}
		failures.count += 1;
		if failures.count >= FREE_FAILURES {
			let doublings = (failures.count - FREE_FAILURES).min(16);
			let lockout = FIRST_LOCKOUT.saturating_mul(1 << doublings).min(MAX_LOCKOUT);
			failures.locked_until = Some(now + lockout);
			tracing::warn!(target: "pyroshine::audit", %peer, what, failures = failures.count, lockout_s = lockout.as_secs(), "Web management sign-in locked out after repeated failures");
		} else {
			tracing::warn!(target: "pyroshine::audit", %peer, what, failures = failures.count, "Web management sign-in failed");
		}
		// Bound the table: forget the oldest peers first.
		if inner.failures.len() > 4096 {
			inner.failures.retain(|_, failures| {
				failures
					.first
					.is_some_and(|first| now.duration_since(first) <= FAILURE_WINDOW)
			});
		}
	}

	fn clear_failures(&self, peer: IpAddr) {
		self.inner.lock().unwrap().failures.remove(&peer);
	}

	/// Run PBKDF2 for `password` against the stored verifier.
	async fn verify_password(&self, password: &str) -> Result<bool, AuthError> {
		let verifier = {
			let inner = self.inner.lock().unwrap();
			if inner.load_error.is_some() {
				return Err(AuthError::NotConfigured);
			}
			inner.stored.administrator.clone().ok_or(AuthError::NotConfigured)?
		};
		let _permit = self.verifying.acquire().await.expect("never closed");
		let password = password.to_string();
		tokio::task::spawn_blocking(move || verify(&verifier, &password))
			.await
			.map_err(|error| AuthError::Storage(format!("Password verification failed: {error}")))
	}

	/// Sign in with the administrator password.
	pub(crate) async fn login(&self, password: &str, peer: IpAddr, agent: &str) -> Result<NewSession, AuthError> {
		self.admit(peer)?;
		if password.is_empty() || password.chars().count() > MAX_PASSWORD {
			self.record_failure(peer, "password");
			return Err(AuthError::InvalidCredentials);
		}
		if !self.verify_password(password).await? {
			self.record_failure(peer, "password");
			return Err(AuthError::InvalidCredentials);
		}
		self.clear_failures(peer);
		let created = self.create_session(peer, agent)?;
		tracing::info!(target: "pyroshine::audit", %peer, session = created.session.id, "Signed in to web management");
		Ok(created)
	}

	/// Set the administrator password with a setup code, and sign in.
	pub(crate) async fn setup(
		&self,
		code: &str,
		password: &str,
		peer: IpAddr,
		agent: &str,
	) -> Result<NewSession, AuthError> {
		self.admit(peer)?;
		let reset = {
			let mut inner = self.inner.lock().unwrap();
			let digest = sha256(normalize_code(code).as_bytes());
			let Some(setup) = inner.setup.as_mut().filter(|setup| setup.expires > Instant::now()) else {
				inner.setup = None;
				drop(inner);
				self.record_failure(peer, "setup code");
				return Err(AuthError::InvalidCredentials);
			};
			if !same(&setup.digest, &digest) {
				setup.attempts += 1;
				if setup.attempts >= SETUP_CODE_ATTEMPTS {
					inner.setup = None;
					tracing::warn!(target: "pyroshine::audit", "Setup code withdrawn after repeated wrong entries");
				}
				drop(inner);
				self.record_failure(peer, "setup code");
				return Err(AuthError::InvalidCredentials);
			}
			setup.reset
		};
		check_password(password)?;
		let password = password.to_string();
		let verifier = {
			let _permit = self.verifying.acquire().await.expect("never closed");
			tokio::task::spawn_blocking(move || derive(&password))
				.await
				.map_err(|error| AuthError::Storage(format!("Password hashing failed: {error}")))?
		};
		{
			let mut inner = self.inner.lock().unwrap();
			// A code issued before another browser finished setup cannot
			// silently replace that password.
			if inner.stored.administrator.is_some() && inner.load_error.is_none() && !reset {
				inner.setup = None;
				return Err(AuthError::InvalidCredentials);
			}
			inner.setup = None;
			inner.load_error = None;
			inner.stored = Stored {
				version: 1,
				administrator: Some(verifier),
				sessions: Vec::new(),
			};
			inner.touched.clear();
		}
		self.clear_failures(peer);
		let created = self.create_session(peer, agent)?;
		self.revocations.send_modify(|generation| *generation += 1);
		tracing::info!(target: "pyroshine::audit", %peer, reset, session = created.session.id, "Web management administrator password set");
		Ok(created)
	}

	/// Issue a setup code (see the module documentation).
	pub(crate) fn create_setup_code(&self, reset: bool) -> Result<String, ManagementError> {
		let mut inner = self.inner.lock().unwrap();
		if inner.stored.administrator.is_some() && inner.load_error.is_none() && !reset {
			return Err(ManagementError::Conflict(
				"An administrator password is already set. Sign in with it, or reset it with \
				 `pyroshine web setup --reset`."
					.into(),
			));
		}
		let code = new_setup_code();
		inner.setup = Some(SetupCode {
			digest: sha256(normalize_code(&code).as_bytes()),
			expires: Instant::now() + SETUP_CODE_LIFETIME,
			attempts: 0,
			reset,
		});
		tracing::info!(target: "pyroshine::audit", reset, "Web management setup code issued");
		Ok(code)
	}

	pub(crate) async fn change_password(
		&self,
		session: &Session,
		current: &str,
		new: &str,
		peer: IpAddr,
	) -> Result<(), AuthError> {
		self.admit(peer)?;
		if !self.verify_password(current).await? {
			self.record_failure(peer, "password");
			return Err(AuthError::InvalidCredentials);
		}
		check_password(new)?;
		let new = new.to_string();
		let verifier = {
			let _permit = self.verifying.acquire().await.expect("never closed");
			tokio::task::spawn_blocking(move || derive(&new))
				.await
				.map_err(|error| AuthError::Storage(format!("Password hashing failed: {error}")))?
		};
		{
			let mut inner = self.inner.lock().unwrap();
			inner.stored.administrator = Some(verifier);
			// Every other browser signs in again with the new password.
			inner.stored.sessions.retain(|stored| stored.id == session.key);
		}
		self.persist()?;
		self.revocations.send_modify(|generation| *generation += 1);
		tracing::info!(target: "pyroshine::audit", %peer, session = session.id, "Web management password changed; other sessions signed out");
		Ok(())
	}

	fn create_session(&self, peer: IpAddr, agent: &str) -> Result<NewSession, AuthError> {
		let token = hex::encode(random_bytes::<32>());
		let key = hex::encode(sha256(token.as_bytes()));
		let now = now_ms();
		{
			let mut inner = self.inner.lock().unwrap();
			let sessions = &mut inner.stored.sessions;
			sessions.retain(|session| expiry(session.created_ms, session.last_used_ms) > now);
			if sessions.len() >= MAX_SESSIONS {
				sessions.sort_by_key(|session| session.last_used_ms);
				let excess = sessions.len() + 1 - MAX_SESSIONS;
				sessions.drain(..excess);
			}
			sessions.push(StoredSession {
				id: key.clone(),
				created_ms: now,
				last_used_ms: now,
				address: peer.to_canonical().to_string(),
				agent: sanitize_agent(agent),
			});
			inner.touched.insert(key.clone(), Instant::now());
		}
		self.persist()?;
		Ok(NewSession {
			token,
			session: Session::from_key(key, now, now),
		})
	}

	pub(crate) fn logout(&self, session: &Session) {
		self.inner
			.lock()
			.unwrap()
			.stored
			.sessions
			.retain(|stored| stored.id != session.key);
		self.persist_quietly();
		self.revocations.send_modify(|generation| *generation += 1);
		tracing::info!(target: "pyroshine::audit", session = session.id, "Signed out of web management");
	}

	pub(crate) fn sessions(&self, current: &Session) -> Vec<SessionSummary> {
		let now = now_ms();
		let inner = self.inner.lock().unwrap();
		let mut sessions: Vec<_> = inner
			.stored
			.sessions
			.iter()
			.filter(|session| expiry(session.created_ms, session.last_used_ms) > now)
			.map(|session| SessionSummary {
				id: session.id[..16].to_string(),
				created_at_ms: session.created_ms,
				last_used_at_ms: session.last_used_ms,
				expires_at_ms: expiry(session.created_ms, session.last_used_ms),
				address: session.address.clone(),
				agent: session.agent.clone(),
				current: session.id == current.key,
			})
			.collect();
		sessions.sort_by_key(|session| std::cmp::Reverse(session.last_used_at_ms));
		sessions
	}

	/// Sign out the session whose public identifier is `id`.
	pub(crate) fn revoke(&self, id: &str, by: &Session) -> Result<(), ManagementError> {
		if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			return Err(ManagementError::Invalid("A session identifier is required.".into()));
		}
		let removed = {
			let mut inner = self.inner.lock().unwrap();
			let before = inner.stored.sessions.len();
			inner.stored.sessions.retain(|stored| !stored.id.starts_with(id));
			before != inner.stored.sessions.len()
		};
		if !removed {
			return Err(ManagementError::NotFound("This session already ended.".into()));
		}
		self.persist()
			.map_err(|error| ManagementError::Failed(error.message()))?;
		self.revocations.send_modify(|generation| *generation += 1);
		tracing::info!(target: "pyroshine::audit", revoked = id, by = by.id, "Web management session revoked");
		Ok(())
	}

	fn persist(&self) -> Result<(), AuthError> {
		let bytes = {
			let inner = self.inner.lock().unwrap();
			if inner.load_error.is_some() {
				// Never overwrite a file we could not read, except by setup.
				return Err(AuthError::Storage(
					"The web management credentials are damaged; reset them with `pyroshine web setup --reset`.".into(),
				));
			}
			serde_json::to_vec_pretty(&inner.stored).expect("credentials serialize")
		};
		crate::durable::replace(&self.path, &bytes).map_err(|error| {
			tracing::error!(path = %self.path.display(), "Cannot save the web management credentials: {error}");
			AuthError::Storage(format!(
				"Cannot save the web management credentials ({error}); see the Pyroshine log."
			))
		})
	}

	fn persist_quietly(&self) {
		let _ = self.persist();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const PEER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

	fn auth() -> (tempfile::TempDir, Auth) {
		let directory = tempfile::tempdir().unwrap();
		let auth = Auth::open(directory.path().join("management.json"));
		(directory, auth)
	}

	async fn configured() -> (tempfile::TempDir, Auth, NewSession) {
		let (directory, auth) = auth();
		let code = auth.create_setup_code(false).unwrap();
		let created = auth.setup(&code, "correct horse", PEER, "Firefox").await.unwrap();
		(directory, auth, created)
	}

	#[tokio::test]
	async fn first_run_requires_a_setup_code_and_creates_a_session() {
		let (_directory, auth) = auth();
		let status = auth.status();
		assert!(!status.configured && !status.setup_pending);
		assert_eq!(
			auth.login("anything", PEER, "").await.err(),
			Some(AuthError::NotConfigured)
		);
		// No code issued: setup is refused.
		assert_eq!(
			auth.setup("AAAA-BBBB-CCCC-DDDD", "correct horse", PEER, "").await.err(),
			Some(AuthError::InvalidCredentials)
		);
		let code = auth.create_setup_code(false).unwrap();
		assert_eq!(code.len(), 19);
		assert!(auth.status().setup_pending);
		assert!(matches!(
			auth.setup(&code, "short", PEER, "").await,
			Err(AuthError::Weak(_))
		));
		// Separators and case do not matter.
		let typed = code.replace('-', " ").to_ascii_lowercase();
		let created = auth.setup(&typed, "correct horse", PEER, "Firefox").await.unwrap();
		assert!(auth.status().configured && !auth.status().setup_pending);
		assert_eq!(auth.authenticate(&created.token).unwrap().id, created.session.id);
		// The code is single use.
		assert_eq!(
			auth.setup(&code, "another password", PEER, "").await.err(),
			Some(AuthError::InvalidCredentials)
		);
		// A second code needs an explicit reset.
		assert!(matches!(
			auth.create_setup_code(false),
			Err(ManagementError::Conflict(_))
		));
	}

	#[tokio::test]
	async fn sessions_are_stored_hashed_and_survive_a_restart() {
		let (directory, _auth, created) = configured().await;
		let text = std::fs::read_to_string(directory.path().join("management.json")).unwrap();
		assert!(!text.contains(&created.token), "the token itself is never stored");
		assert!(!text.contains("correct horse"));
		use std::os::unix::fs::PermissionsExt;
		let mode = std::fs::metadata(directory.path().join("management.json"))
			.unwrap()
			.permissions()
			.mode();
		assert_eq!(mode & 0o777, 0o600);
		let reopened = Auth::open(directory.path().join("management.json"));
		assert!(reopened.authenticate(&created.token).is_ok());
		assert!(reopened.authenticate(&"0".repeat(64)).is_err());
		assert!(reopened.authenticate("not hex").is_err());
	}

	#[tokio::test]
	async fn login_creates_a_fresh_session_and_logout_ends_it() {
		let (_directory, auth, first) = configured().await;
		let second = auth.login("correct horse", PEER, "Chromium").await.unwrap();
		assert_ne!(first.token, second.token, "sign-in never reuses a token");
		let revocations = auth.revocations();
		auth.logout(&second.session);
		assert!(revocations.has_changed().unwrap());
		assert_eq!(auth.authenticate(&second.token).err(), Some(AuthError::Unauthenticated));
		assert!(auth.authenticate(&first.token).is_ok());
		assert!(!auth.is_live(&second.session));
	}

	#[tokio::test]
	async fn csrf_tokens_are_bound_to_the_session() {
		let (_directory, auth, created) = configured().await;
		let other = auth.login("correct horse", PEER, "").await.unwrap();
		let csrf = Auth::csrf_token(&created.token);
		assert!(Auth::csrf_matches(&created.token, &csrf));
		assert!(!Auth::csrf_matches(&other.token, &csrf));
		assert!(!Auth::csrf_matches(&created.token, ""));
	}

	#[tokio::test]
	async fn repeated_failures_lock_the_peer_out() {
		let (_directory, auth, _created) = configured().await;
		for _ in 0..FREE_FAILURES {
			assert_eq!(
				auth.login("wrong password", PEER, "").await.err(),
				Some(AuthError::InvalidCredentials)
			);
		}
		// Even the right password waits now.
		assert!(matches!(
			auth.login("correct horse", PEER, "").await,
			Err(AuthError::RateLimited(_))
		));
		// Another address is unaffected.
		let other: IpAddr = "192.0.2.7".parse().unwrap();
		assert!(auth.login("correct horse", other, "").await.is_ok());
	}

	#[tokio::test]
	async fn wrong_setup_codes_withdraw_the_code() {
		let (_directory, auth) = auth();
		let code = auth.create_setup_code(false).unwrap();
		for attempt in 0..SETUP_CODE_ATTEMPTS {
			let peer = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, attempt as u8));
			assert_eq!(
				auth.setup("0000-0000-0000-0000", "correct horse", peer, "").await.err(),
				Some(AuthError::InvalidCredentials)
			);
		}
		assert!(!auth.status().setup_pending);
		let peer = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 1));
		assert_eq!(
			auth.setup(&code, "correct horse", peer, "").await.err(),
			Some(AuthError::InvalidCredentials)
		);
	}

	#[tokio::test]
	async fn password_changes_and_resets_sign_out_other_sessions() {
		let (_directory, auth, first) = configured().await;
		let second = auth.login("correct horse", PEER, "").await.unwrap();
		assert_eq!(
			auth.change_password(&first.session, "wrong", "new password!", PEER)
				.await
				.err(),
			Some(AuthError::InvalidCredentials)
		);
		auth.change_password(&first.session, "correct horse", "new password!", PEER)
			.await
			.unwrap();
		assert!(auth.authenticate(&first.token).is_ok());
		assert!(auth.authenticate(&second.token).is_err());
		assert!(auth.login("correct horse", PEER, "").await.is_err());
		let third = auth.login("new password!", PEER, "").await.unwrap();

		let code = auth.create_setup_code(true).unwrap();
		let reset = auth.setup(&code, "reset password", PEER, "").await.unwrap();
		assert!(auth.authenticate(&first.token).is_err());
		assert!(auth.authenticate(&third.token).is_err());
		assert!(auth.authenticate(&reset.token).is_ok());
	}

	#[tokio::test]
	async fn sessions_can_be_listed_and_revoked_by_public_id() {
		let (_directory, auth, first) = configured().await;
		let second = auth
			.login("correct horse", PEER, "Mozilla/5.0\u{7}\nInjected")
			.await
			.unwrap();
		let sessions = auth.sessions(&first.session);
		assert_eq!(sessions.len(), 2);
		assert!(
			sessions
				.iter()
				.any(|session| session.current && session.id == first.session.id)
		);
		let listed = sessions.iter().find(|session| !session.current).unwrap();
		assert_eq!(listed.agent, "Mozilla/5.0Injected");
		auth.revoke(&second.session.id, &first.session).unwrap();
		assert!(auth.authenticate(&second.token).is_err());
		assert!(matches!(
			auth.revoke(&second.session.id, &first.session),
			Err(ManagementError::NotFound(_))
		));
		assert!(matches!(
			auth.revoke("../", &first.session),
			Err(ManagementError::Invalid(_))
		));
	}

	#[tokio::test]
	async fn expired_sessions_are_refused() {
		let (directory, auth, created) = configured().await;
		{
			let mut inner = auth.inner.lock().unwrap();
			let session = &mut inner.stored.sessions[0];
			session.last_used_ms -= IDLE_TIMEOUT.as_millis() as u64 + 1;
		}
		assert_eq!(
			auth.authenticate(&created.token).err(),
			Some(AuthError::Unauthenticated)
		);
		// Lifetime is absolute, however active the session was.
		let session = auth.login("correct horse", PEER, "").await.unwrap();
		{
			let mut inner = auth.inner.lock().unwrap();
			let stored = inner.stored.sessions.last_mut().unwrap();
			stored.created_ms -= MAX_LIFETIME.as_millis() as u64 + 1;
		}
		assert!(auth.authenticate(&session.token).is_err());
		drop(directory);
	}

	#[tokio::test]
	async fn a_damaged_file_fails_closed_until_reset() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("management.json");
		std::fs::write(&path, "{ not json").unwrap();
		let auth = Auth::open(path.clone());
		let status = auth.status();
		assert!(!status.configured && status.locked.is_some());
		assert_eq!(
			auth.login("anything", PEER, "").await.err(),
			Some(AuthError::NotConfigured)
		);
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
		// An ordinary setup code replaces nothing; a reset code does.
		let code = auth.create_setup_code(true).unwrap();
		auth.setup(&code, "correct horse", PEER, "").await.unwrap();
		assert!(auth.status().locked.is_none());
		assert!(Auth::open(path).status().configured);
	}

	#[test]
	fn setup_codes_fold_misread_letters() {
		assert_eq!(normalize_code("abcd-iloO 1234"), "ABCD11001234");
		let code = new_setup_code();
		assert!(code.chars().all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8))));
	}

	#[test]
	fn verifiers_round_trip_and_reject_tampering() {
		let verifier = derive("correct horse");
		assert!(verify(&verifier, "correct horse"));
		assert!(!verify(&verifier, "correct horsE"));
		let mut weaker = verifier.clone();
		weaker.algorithm = "plain".into();
		assert!(!verify(&weaker, "correct horse"));
	}
}

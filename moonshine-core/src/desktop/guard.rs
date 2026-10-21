//! Independently supervised owner of the desktop transaction and recovery journal.
use super::audio::{Audio, AudioSnapshot};
use super::native::{Native, Platform, restore_configuration, verify};
use super::wire::{self, Reply, Request};
use super::{DesktopStatus, Result, Snapshot, Target};
use crate::session::capture::portal::desktop;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
	uid: u32,
	boot: String,
	socket: PathBuf,
	inode: u64,
	session: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
	version: u32,
	identity: Identity,
	lease: String,
	baseline: Snapshot,
	targets: Vec<Target>,
	audio: Option<AudioSnapshot>,
	committed: bool,
	active_target: Option<String>,
}
struct Guard {
	native: Box<dyn Platform>,
	identity: Identity,
	path: PathBuf,
	desktop: String,
	supported: bool,
	journal: Option<Journal>,
	owner: Option<OwnedFd>,
	owner_connection: Option<UnixStream>,
	owner_pid: i32,
	audio: Option<Audio>,
	phase: String,
	error: Option<String>,
	grace_deadline_ms: Option<u64>,
	grace_deadline: Option<Instant>,
	grace_revision: u64,
	last_check: Instant,
	last_topology: Option<Snapshot>,
}
fn journal_path() -> Result<PathBuf> {
	Ok(dirs::state_dir()
		.ok_or("No user state directory")?
		.join("moonshine/desktop-recovery.json"))
}
fn save(path: &std::path::Path, journal: &Journal) -> Result<()> {
	{
		let bytes = serde_json::to_vec(journal).map_err(|e| e.to_string())?;
		if bytes.len() as u64 > wire::MAX_MESSAGE {
			return Err("Desktop recovery journal is too large".into());
		}
		crate::durable::replace(path, &bytes)
	}
	.map_err(|e| e.to_string())
}
fn load(path: &std::path::Path) -> Result<Option<Journal>> {
	let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
		Ok(file) => file,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(e.to_string()),
	};
	let mut bytes = Vec::new();
	file.take(wire::MAX_MESSAGE + 1)
		.read_to_end(&mut bytes)
		.map_err(|e| e.to_string())?;
	if bytes.len() as u64 > wire::MAX_MESSAGE {
		return Err("Desktop journal exceeds its size limit".into());
	}
	let journal: Journal =
		serde_json::from_slice(&bytes).map_err(|e| format!("Invalid desktop recovery journal: {e}"))?;
	if journal.version != 1 {
		return Err("Unsupported desktop recovery journal version".into());
	}
	Ok(Some(journal))
}
fn peer(stream: &UnixStream) -> Result<libc::ucred> {
	let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
	let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
	// SAFETY: pointers and length describe the initialized ucred output buffer.
	if unsafe {
		libc::getsockopt(
			stream.as_raw_fd(),
			libc::SOL_SOCKET,
			libc::SO_PEERCRED,
			(&mut cred as *mut libc::ucred).cast(),
			&mut length,
		)
	} != 0
	{
		return Err(std::io::Error::last_os_error().to_string());
	}
	// SAFETY: geteuid has no preconditions.
	if cred.uid != unsafe { libc::geteuid() } {
		return Err("Desktop guard accepts only its own user".into());
	}
	Ok(cred)
}
fn pidfd(pid: i32) -> Result<OwnedFd> {
	// SAFETY: pidfd_open has no pointer arguments; zero flags requests an owned fd.
	let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
	if fd < 0 {
		return Err(format!(
			"Cannot watch the desktop owner: {}",
			std::io::Error::last_os_error()
		));
	}
	// SAFETY: the successful syscall returned a fresh owned descriptor.
	Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
fn connection_closed(stream: &UnixStream) -> bool {
	let mut poll = libc::pollfd {
		fd: stream.as_raw_fd(),
		events: libc::POLLRDHUP,
		revents: 0,
	};
	// SAFETY: one initialized pollfd. Only EOF/hangup, never media, is observed.
	unsafe { libc::poll(&mut poll, 1, 0) > 0 && poll.revents & (libc::POLLHUP | libc::POLLRDHUP | libc::POLLERR) != 0 }
}
fn dead(fd: &OwnedFd) -> bool {
	let mut poll = libc::pollfd {
		fd: fd.as_raw_fd(),
		events: libc::POLLIN,
		revents: 0,
	};
	// SAFETY: one initialized pollfd, nonblocking observation only.
	unsafe { libc::poll(&mut poll, 1, 0) > 0 }
}
impl Guard {
	fn persist(&self) -> Result<()> {
		if let Some(journal) = &self.journal {
			save(&self.path, journal)?;
		}
		Ok(())
	}
	fn status(&self) -> DesktopStatus {
		DesktopStatus {
			supported: self.supported,
			desktop: self.desktop.clone(),
			phase: self.phase.clone(),
			reason: self.error.clone(),
			mode: self.journal.as_ref().and_then(|j| {
				j.targets
					.iter()
					.find(|t| Some(&t.id) == j.active_target.as_ref())
					.or_else(|| j.targets.last())
					.map(|t| t.mode.into())
			}),
			grace_deadline_ms: self.grace_deadline_ms,
			audio_remote_only: matches!(self.phase.as_str(), "active" | "grace" | "preparing" | "committing")
				&& self.journal.as_ref().is_some_and(|j| j.audio.is_some() && j.committed),
		}
	}
	fn check(&self, lease: &str) -> Result<()> {
		if self.journal.as_ref().is_some_and(|j| j.lease == lease) && self.phase != "recovery_required" {
			Ok(())
		} else {
			Err("Desktop lease is stale or requires recovery".into())
		}
	}
	fn restore(&mut self) -> Result<()> {
		let Some(journal) = self.journal.clone() else {
			return Ok(());
		};
		self.phase = "restoring".into();
		self.grace_deadline_ms = None;
		self.grace_deadline = None;
		if journal.identity != self.identity {
			return Err("Recovery belongs to a different graphical login; refusing to replay it".into());
		}
		let started = Instant::now();
		let current = self.native.snapshot()?;
		let owned: Vec<_> = journal
			.targets
			.iter()
			.filter_map(|t| {
				if !t.output.is_empty() {
					Some(t.output.clone())
				} else {
					self.native
						.resolve_target(t)
						.ok()
						.map(|t| t.output)
						.filter(|key| !key.is_empty())
						.or_else(|| {
							current
								.outputs
								.iter()
								.find(|o| o.key == t.id || o.name == format!("Virtual-Pyroshine-{}", t.id))
								.map(|o| o.key.clone())
						})
				}
			})
			.collect();
		let (mut desired, partial) = restore_configuration(&current, &journal.baseline, &owned)?;
		self.native.apply(&desired)?;
		verify(&self.native.snapshot()?, &desired)?;
		if let Some(snapshot) = &journal.audio {
			if self.audio.is_none() {
				self.audio = Some(Audio::connect()?);
			}
			self.audio
				.as_mut()
				.ok_or("Audio recovery is unavailable")?
				.restore(snapshot)?;
		}
		if started.elapsed() >= Duration::from_secs(15) {
			return Err("Desktop restoration exceeded 15 seconds".into());
		}
		for target in &journal.targets {
			self.native.retire(&target.id)?;
		}
		desired.outputs.retain(|o| !owned.contains(&o.key));
		let restored = self.native.snapshot()?;
		verify(&restored, &desired)?;
		if restored.outputs.iter().any(|o| owned.contains(&o.key)) {
			return Err("An owned virtual output remains after closing its capture".into());
		}
		if partial {
			return Err(
				"Available displays were recovered, but saved displays are missing; reconnect them and retry Restore"
					.into(),
			);
		}
		std::fs::remove_file(&self.path).map_err(|e| e.to_string())?;
		crate::durable::sync_parent(&self.path).map_err(|e| e.to_string())?;
		self.journal = None;
		self.owner = None;
		self.owner_connection = None;
		self.audio = None;
		self.phase = "idle".into();
		self.error = None;
		Ok(())
	}
	fn recover(&mut self) {
		if let Err(error) = self.restore() {
			self.phase = "recovery_required".into();
			self.error = Some(error.clone());
			tracing::error!(%error,"Desktop recovery incomplete; journal and ownership retained");
		}
	}
	fn handle(&mut self, request: Request, pid: i32) -> Result<Option<Target>> {
		match request {
			Request::Status => Ok(None),
			Request::Watch { lease } => {
				self.check(&lease)?;
				if self.owner_pid != pid {
					return Err("Only the original desktop owner can watch its lease".into());
				}
				Ok(None)
			},
			Request::Restore { lease } => {
				if self.journal.is_none() {
					return Ok(None);
				}
				if let Some(lease) = lease
					&& !self.journal.as_ref().is_some_and(|j| j.lease == lease)
				{
					return Err("Stale desktop restore request".into());
				}
				self.restore()?;
				Ok(None)
			},
			Request::Prepare { lease, mode, audio } => {
				let mode = mode.validate()?;
				if let Some(lease) = lease {
					self.check(&lease)?;
				} else {
					if self.journal.is_some() {
						return Err("Another Desktop Mode session owns the desktop or requires recovery".into());
					}
					let baseline = self.native.snapshot()?;
					if !baseline.outputs.iter().any(|o| o.enabled) {
						return Err("No active desktop display configuration can be saved".into());
					}
					if baseline
						.outputs
						.iter()
						.any(|o| o.enabled && (o.name.starts_with("Virtual-") || o.key.contains("MetaVendor")))
					{
						return Err(
							"Another virtual desktop output is active; stop its owner before Desktop Mode".into(),
						);
					}
					let owner = pidfd(pid)?;
					let lease = uuid::Uuid::new_v4().to_string();
					let audio_snapshot = if audio {
						let mut control = Audio::connect()?;
						let snapshot = control.snapshot(&lease)?;
						self.audio = Some(control);
						Some(snapshot)
					} else {
						None
					};
					self.journal = Some(Journal {
						version: 1,
						identity: self.identity.clone(),
						lease,
						baseline,
						targets: Vec::new(),
						audio: audio_snapshot,
						committed: false,
						active_target: None,
					});
					self.grace_revision = 0;
					self.owner = Some(owner);
					self.owner_pid = pid;
					self.phase = "preparing".into();
					self.error = None;
					self.persist()?;
					if let (Some(control), Some(snapshot)) =
						(&mut self.audio, self.journal.as_ref().and_then(|j| j.audio.as_ref()))
					{
						control.create(snapshot)?;
					}
				}
				self.phase = "preparing".into();
				let id = uuid::Uuid::new_v4().to_string();
				self.journal
					.as_mut()
					.ok_or("Desktop lease disappeared")?
					.targets
					.push(Target {
						id: id.clone(),
						node_id: 0,
						serial: None,
						output: String::new(),
						mode,
					});
				self.persist()?;
				let target = self.native.prepare(&id, mode)?;
				*self
					.journal
					.as_mut()
					.ok_or("Desktop lease disappeared")?
					.targets
					.last_mut()
					.ok_or("Desktop target disappeared")? = target.clone();
				self.persist()?;
				Ok(Some(target))
			},
			Request::Commit { lease, target } => {
				self.check(&lease)?;
				let target = self
					.journal
					.as_ref()
					.and_then(|j| j.targets.iter().find(|t| t.id == target))
					.cloned()
					.ok_or("Unknown desktop capture target")?;
				let target = self.native.resolve_target(&target)?;
				let journal = self.journal.as_mut().ok_or("Desktop lease disappeared")?;
				*journal
					.targets
					.iter_mut()
					.find(|t| t.id == target.id)
					.ok_or("Unknown desktop target")? = target.clone();
				journal.committed = true;
				journal.active_target = Some(target.id.clone());
				self.phase = "committing".into();
				if let (Some(control), Some(snapshot)) = (&mut self.audio, &mut journal.audio) {
					control.update_snapshot(snapshot)?;
				}
				self.persist()?;
				if let (Some(control), Some(snapshot)) =
					(&mut self.audio, self.journal.as_ref().and_then(|j| j.audio.as_ref()))
				{
					control.route(snapshot)?;
				}
				self.native.commit(&target)?;
				self.phase = if self.grace_deadline_ms.is_some() {
					"grace"
				} else {
					"active"
				}
				.into();
				Ok(Some(target))
			},
			Request::Retire { lease, target } => {
				self.check(&lease)?;
				let journal = self.journal.as_ref().ok_or("Desktop lease disappeared")?;
				if !journal.targets.iter().any(|t| t.id == target)
					|| journal.active_target.as_ref().is_some_and(|id| *id == target)
				{
					return Err("Cannot retire the active or an unowned desktop output".into());
				}
				self.native.retire(&target)?;
				self.journal
					.as_mut()
					.ok_or("Desktop lease disappeared")?
					.targets
					.retain(|t| t.id != target);
				self.persist()?;
				Ok(None)
			},
			Request::Grace {
				lease,
				deadline_ms,
				revision,
			} => {
				self.check(&lease)?;
				if revision > self.grace_revision {
					self.grace_revision = revision;
					self.grace_deadline_ms = deadline_ms;
					let now_ms = std::time::SystemTime::now()
						.duration_since(std::time::UNIX_EPOCH)
						.unwrap_or_default()
						.as_millis()
						.min(u64::MAX as u128) as u64;
					// Convert the display timestamp once. Later wall-clock changes
					// must not extend or shorten the actual reconnect grace.
					self.grace_deadline = deadline_ms
						.map(|d| Instant::now() + Duration::from_millis(d.saturating_sub(now_ms).min(86_400_000)));
					if self.phase != "preparing" {
						self.phase = if deadline_ms.is_some() { "grace" } else { "active" }.into();
					}
				}
				Ok(None)
			},
		}
	}
	fn tick(&mut self) {
		let result = (|| -> Result<()> {
			self.native.pump()?;
			if self.grace_deadline.is_some_and(|d| Instant::now() >= d) {
				self.restore()?;
			}
			if self.last_check.elapsed() >= Duration::from_millis(500) && self.journal.is_some() {
				self.last_check = Instant::now();
				let current = self.native.snapshot()?;
				let changed = self.last_topology.as_ref().is_none_or(|last| last != &current);
				self.last_topology = Some(current);
				if self.phase == "recovery_required"
					&& (changed
						|| self
							.audio
							.as_mut()
							.map(|a| a.take_changed())
							.transpose()?
							.unwrap_or(false))
				{
					self.restore()?;
				} else if changed
					&& matches!(self.phase.as_str(), "active" | "grace")
					&& let Some(target) = self
						.journal
						.as_ref()
						.and_then(|j| j.targets.iter().find(|t| Some(&t.id) == j.active_target.as_ref()))
						.cloned()
				{
					self.native.commit(&target)?;
				}
			}

			if (self.owner.as_ref().is_some_and(dead) || self.owner_connection.as_ref().is_some_and(connection_closed))
				&& self.phase != "recovery_required"
			{
				self.restore()?;
			}
			if self.journal.as_ref().is_some_and(|j| j.committed)
				&& matches!(self.phase.as_str(), "active" | "grace")
				&& self.audio.as_mut().map(|a| a.changed()).transpose()?.unwrap_or(false)
			{
				if let (Some(control), Some(snapshot)) =
					(&mut self.audio, self.journal.as_mut().and_then(|j| j.audio.as_mut()))
				{
					control.update_snapshot(snapshot)?;
				}
				self.persist()?;
				if let (Some(control), Some(snapshot)) =
					(&mut self.audio, self.journal.as_ref().and_then(|j| j.audio.as_ref()))
				{
					control.route(snapshot)?;
				}
			}
			Ok(())
		})();
		if let Err(error) = result {
			if self.journal.is_none() {
				self.supported = false;
				self.phase = "unsupported".into();
				self.error = Some(error);
			} else if self.phase != "recovery_required" {
				self.error = Some(error);
				self.recover();
			}
		}
	}
}

pub fn status() -> Result<DesktopStatus> {
	wire::ensure()?;
	wire::request(&Request::Status).map(|r| r.status)
}
pub fn restore() -> Result<DesktopStatus> {
	wire::ensure()?;
	wire::request(&Request::Restore { lease: None }).map(|r| r.status)
}

/// Run only as `pyroshine desktop guard`, in the user's independently supervised unit.
pub fn run_guard() -> Result<()> {
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|e| e.to_string())?;
	let desktop = runtime.block_on(async {
		let bus = zbus::Connection::session().await.map_err(|e| e.to_string())?;
		desktop::discover(&bus).await.map_err(|e| e.to_string())
	})?;
	let metadata = std::fs::metadata(&desktop.wayland_socket).map_err(|e| e.to_string())?;
	let identity = Identity {
		uid: metadata.uid(),
		boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e| e.to_string())?,
		socket: desktop.wayland_socket.clone(),
		inode: metadata.ino(),
		session: desktop.session_id.clone(),
	};
	let socket = wire::socket_path()?;
	let directory = socket.parent().ok_or("Invalid guard socket path")?;
	crate::durable::create_directories(directory).map_err(|e| e.to_string())?;
	std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
	let lock: File = OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.mode(0o600)
		.custom_flags(libc::O_NOFOLLOW)
		.open(directory.join("desktop-guard.lock"))
		.map_err(|e| e.to_string())?;
	// SAFETY: flock operates on a live descriptor; lock is held for this process lifetime.
	if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
		return Err("Desktop guard is already running".into());
	}
	if let Ok(metadata) = std::fs::symlink_metadata(&socket) {
		if !metadata.file_type().is_socket() || metadata.uid() != identity.uid {
			return Err("Refusing to replace an unowned desktop socket".into());
		}
		std::fs::remove_file(&socket).map_err(|e| e.to_string())?;
	}
	let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
	std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
	listener.set_nonblocking(true).map_err(|e| e.to_string())?;
	let (native, unsupported) = match Native::connect(&desktop) {
		Ok(native) => (native, None),
		Err(reason) => (Native::Unavailable(reason.clone()), Some(reason)),
	};
	let path = journal_path()?;
	let journal = load(&path)?;
	let mut guard = Guard {
		native: Box::new(native),
		identity,
		path,
		desktop: desktop.kind.as_str().into(),
		supported: unsupported.is_none(),
		journal,
		owner: None,
		owner_connection: None,
		owner_pid: 0,
		audio: None,
		phase: if unsupported.is_some() { "unsupported" } else { "idle" }.into(),
		error: unsupported,
		grace_deadline_ms: None,
		grace_deadline: None,
		grace_revision: 0,
		last_check: Instant::now(),
		last_topology: None,
	};
	if guard.journal.is_some() {
		guard.recover();
	}
	// Signal handlers only set an atomic; restoration remains on the owner thread.
	static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
	extern "C" fn stop(_: libc::c_int) {
		STOP.store(true, std::sync::atomic::Ordering::Relaxed);
	}
	// SAFETY: the handler has the required ABI and performs only a lock-free store.
	unsafe {
		libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
		libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
	}
	tracing::info!(socket=%socket.display(),"Desktop guard ready");
	loop {
		if STOP.load(std::sync::atomic::Ordering::Relaxed) {
			guard.recover();
			let _ = std::fs::remove_file(&socket);
			return if guard.journal.is_none() {
				Ok(())
			} else {
				Err(guard
					.error
					.unwrap_or_else(|| "Desktop recovery incomplete at shutdown".into()))
			};
		}
		match listener.accept() {
			Ok((mut stream, _)) => {
				stream
					.set_read_timeout(Some(Duration::from_millis(200)))
					.map_err(|e| e.to_string())?;
				stream
					.set_write_timeout(Some(Duration::from_secs(1)))
					.map_err(|e| e.to_string())?;
				let mut mutation = false;
				let result = peer(&stream).and_then(|peer| {
					let request: Request = wire::read_message(&mut stream)?;
					mutation = matches!(
						request,
						Request::Prepare { .. } | Request::Commit { .. } | Request::Restore { .. }
					);
					let watch = matches!(request, Request::Watch { .. });
					let result = guard.handle(request, peer.pid)?;
					if watch {
						guard.owner_connection = Some(stream.try_clone().map_err(|e| e.to_string())?);
					}
					Ok(result)
				});
				let (target, error) = match result {
					Ok(target) => (target, None),
					Err(error) => {
						if mutation
							&& guard.journal.is_some()
							&& !matches!(guard.phase.as_str(), "active" | "grace" | "recovery_required")
						{
							guard.recover();
						}
						(None, Some(error))
					},
				};
				let reply = Reply {
					status: guard.status(),
					lease: guard.journal.as_ref().map(|j| j.lease.clone()),
					target,
					error,
				};
				let _ = wire::write_message(&mut stream, &reply);
			},
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
			Err(e) => return Err(e.to_string()),
		}
		guard.tick();
		std::thread::sleep(Duration::from_millis(20));
	}
}

pub(crate) fn cleanup_complete() -> Result<()> {
	if !journal_path()?.exists() {
		return Ok(());
	}
	let status = wire::request(&Request::Status)?.status;
	if status.phase == "idle" && !journal_path()?.exists() {
		Ok(())
	} else {
		Err(status.reason.unwrap_or_else(|| "Desktop recovery is incomplete".into()))
	}
}

pub(crate) fn recovery_pending() -> bool {
	journal_path().is_ok_and(|p| p.exists())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::desktop::{Mode, Output};
	use std::cell::RefCell;
	use std::rc::Rc;
	struct FakeState {
		current: Snapshot,
		journal: PathBuf,
		fail_prepare: bool,
		fail_commit: bool,
		fail_restore: bool,
		retired: Vec<String>,
	}
	struct Fake(Rc<RefCell<FakeState>>);
	impl Platform for Fake {
		fn snapshot(&mut self) -> Result<Snapshot> {
			Ok(self.0.borrow().current.clone())
		}
		fn prepare(&mut self, id: &str, mode: Mode) -> Result<Target> {
			let mut state = self.0.borrow_mut();
			let journal = load(&state.journal)?.expect("journal exists before native mutation");
			assert!(journal.targets.iter().any(|t| t.id == id));
			let mut output = output(id, true);
			output.name = format!("Virtual-{id}");
			output.mode = mode;
			state.current.outputs.push(output);
			if state.fail_prepare {
				return Err("injected prepare failure after output creation".into());
			}
			Ok(Target {
				id: id.into(),
				node_id: 7,
				serial: Some(42),
				output: id.into(),
				mode,
			})
		}
		fn resolve_target(&mut self, target: &Target) -> Result<Target> {
			Ok(target.clone())
		}
		fn apply(&mut self, snapshot: &Snapshot) -> Result<()> {
			let mut state = self.0.borrow_mut();
			assert!(state.journal.exists(), "restoration owns a durable journal");
			if state.fail_restore {
				return Err("injected display restore failure".into());
			}
			for desired in &snapshot.outputs {
				if !desired.enabled
					&& state
						.current
						.outputs
						.iter()
						.any(|o| o.key == desired.key && !o.can_disable)
				{
					return Err("attempted to disable an output without Disable capability".into());
				}
			}
			state.current = snapshot.clone();
			Ok(())
		}
		fn commit(&mut self, target: &Target) -> Result<()> {
			let mut state = self.0.borrow_mut();
			let journal = load(&state.journal)?.unwrap();
			assert_eq!(journal.active_target.as_deref(), Some(target.id.as_str()));
			for output in &mut state.current.outputs {
				output.enabled = output.key == target.output;
			}
			if state.fail_commit {
				return Err("injected failure after physical displays disabled".into());
			}
			Ok(())
		}
		fn retire(&mut self, id: &str) -> Result<()> {
			let mut state = self.0.borrow_mut();
			assert!(
				state
					.current
					.outputs
					.iter()
					.any(|o| o.enabled && !o.name.starts_with("Virtual-"))
					|| state.current.outputs.iter().any(|o| o.enabled && o.key != id),
				"never remove the last usable image"
			);
			state.current.outputs.retain(|o| o.key != id);
			state.retired.push(id.into());
			Ok(())
		}
		fn pump(&mut self) -> Result<()> {
			Ok(())
		}
	}
	fn output(key: &str, enabled: bool) -> Output {
		Output {
			key: key.into(),
			name: key.into(),
			enabled,
			can_disable: true,
			mode: Mode {
				width: 2560,
				height: 1440,
				refresh_millihertz: 144000,
			},
			x: 0,
			y: 0,
			scale: 1.25,
			transform: 0,
			priority: 1,
			group: 1,
			hdr: 0,
			wcg: 0,
			replication: String::new(),
		}
	}
	fn fixture() -> (tempfile::TempDir, Guard, Rc<RefCell<FakeState>>) {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("recovery.json");
		let current = Snapshot {
			outputs: vec![output("DP-1", true)],
			layout_mode: None,
		};
		let state = Rc::new(RefCell::new(FakeState {
			current,
			journal: path.clone(),
			fail_prepare: false,
			fail_commit: false,
			fail_restore: false,
			retired: Vec::new(),
		}));
		let guard = Guard {
			native: Box::new(Fake(state.clone())),
			identity: Identity {
				uid: 1000,
				boot: "boot".into(),
				socket: "wayland-0".into(),
				inode: 3,
				session: Some("login".into()),
			},
			path,
			desktop: "test".into(),
			supported: true,
			journal: None,
			owner: None,
			owner_connection: None,
			owner_pid: 0,
			audio: None,
			phase: "idle".into(),
			error: None,
			grace_deadline_ms: None,
			grace_deadline: None,
			grace_revision: 0,
			last_check: Instant::now(),
			last_topology: None,
		};
		(directory, guard, state)
	}
	fn prepare(guard: &mut Guard) -> Target {
		guard
			.handle(
				Request::Prepare {
					lease: None,
					mode: Mode {
						width: 1920,
						height: 1080,
						refresh_millihertz: 60000,
					},
					audio: false,
				},
				std::process::id() as i32,
			)
			.unwrap()
			.unwrap()
	}
	fn commit(guard: &mut Guard, target: &Target) {
		let lease = guard.journal.as_ref().unwrap().lease.clone();
		guard
			.handle(
				Request::Commit {
					lease,
					target: target.id.clone(),
				},
				std::process::id() as i32,
			)
			.unwrap();
	}
	#[test]
	fn non_disableable_virtual_output_is_closed_after_physical_restore() {
		let (_directory, mut guard, state) = fixture();
		let baseline = state.borrow().current.clone();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		state
			.borrow_mut()
			.current
			.outputs
			.iter_mut()
			.find(|o| o.key == target.output)
			.unwrap()
			.can_disable = false;
		guard.restore().unwrap();
		assert_eq!(state.borrow().current, baseline);
		assert_eq!(guard.phase, "idle");
		assert_eq!(state.borrow().retired, [target.id]);
	}

	#[test]
	fn old_journals_default_physical_outputs_to_disableable() {
		let mut value = serde_json::to_value(output("DP-1", true)).unwrap();
		value.as_object_mut().unwrap().remove("can_disable");
		let saved: Output = serde_json::from_value(value).unwrap();
		assert!(saved.can_disable);
	}

	#[test]
	fn verification_restores_fractional_scale_rotation_and_mirror_membership() {
		let mut a = output("DP-1", true);
		a.transform = 1;
		a.scale = 1.5;
		let mut b = a.clone();
		b.key = "HDMI-1".into();
		let saved = Snapshot {
			outputs: vec![a, b],
			layout_mode: Some(1),
		};
		let mut observed = saved.clone();
		for output in &mut observed.outputs {
			output.group = 7;
		}
		assert!(verify(&observed, &saved).is_ok(), "logical IDs can be renumbered");
		observed.outputs[1].group = 8;
		assert!(verify(&observed, &saved).is_err());
		observed = saved.clone();
		observed.outputs[0].scale = 1.0;
		assert!(verify(&observed, &saved).is_err());
		observed = saved.clone();
		observed.layout_mode = Some(2);
		assert!(verify(&observed, &saved).is_err());
	}

	#[test]
	fn capture_owner_connection_loss_restores_without_daemon_exit() {
		let (_directory, mut guard, state) = fixture();
		let baseline = state.borrow().current.clone();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		let (owner, watcher) = UnixStream::pair().unwrap();
		guard.owner_connection = Some(watcher);
		guard.tick();
		assert!(guard.journal.is_some());
		drop(owner);
		guard.tick();
		assert_eq!(guard.phase, "idle");
		assert_eq!(state.borrow().current, baseline);
	}

	#[test]
	fn takeover_snapshots_before_mutation_and_restores_before_retiring() {
		let (_directory, mut guard, state) = fixture();
		let baseline = state.borrow().current.clone();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		assert!(
			!state
				.borrow()
				.current
				.outputs
				.iter()
				.find(|o| o.key == "DP-1")
				.unwrap()
				.enabled
		);
		guard.restore().unwrap();
		assert_eq!(state.borrow().current, baseline);
		assert_eq!(state.borrow().retired, vec![target.id]);
		assert!(!guard.path.exists());
		assert_eq!(guard.phase, "idle");
		guard.restore().unwrap();
	}
	#[test]
	fn failure_after_output_creation_rolls_back_owned_resources() {
		let (_directory, mut guard, state) = fixture();
		state.borrow_mut().fail_prepare = true;
		assert!(
			guard
				.handle(
					Request::Prepare {
						lease: None,
						mode: Mode {
							width: 1920,
							height: 1080,
							refresh_millihertz: 60000
						},
						audio: false
					},
					std::process::id() as i32
				)
				.is_err()
		);
		guard.recover();
		assert_eq!(guard.phase, "idle");
		assert_eq!(state.borrow().retired.len(), 1);
	}
	#[test]
	fn failed_commit_and_failed_restore_retain_ownership_until_retry() {
		let (_directory, mut guard, state) = fixture();
		let target = prepare(&mut guard);
		let lease = guard.journal.as_ref().unwrap().lease.clone();
		state.borrow_mut().fail_commit = true;
		assert!(
			guard
				.handle(
					Request::Commit {
						lease,
						target: target.id
					},
					std::process::id() as i32
				)
				.is_err()
		);
		state.borrow_mut().fail_restore = true;
		guard.recover();
		assert_eq!(guard.phase, "recovery_required");
		assert!(guard.path.exists());
		assert!(state.borrow().retired.is_empty());
		state.borrow_mut().fail_restore = false;
		guard.restore().unwrap();
		assert_eq!(guard.phase, "idle");
	}
	#[test]
	fn replacement_keeps_original_snapshot_and_stale_requests_cannot_change_it() {
		let (_directory, mut guard, state) = fixture();
		let baseline = state.borrow().current.clone();
		let first = prepare(&mut guard);
		commit(&mut guard, &first);
		let lease = guard.journal.as_ref().unwrap().lease.clone();
		let next = guard
			.handle(
				Request::Prepare {
					lease: Some(lease.clone()),
					mode: Mode {
						width: 2560,
						height: 1440,
						refresh_millihertz: 120000,
					},
					audio: false,
				},
				std::process::id() as i32,
			)
			.unwrap()
			.unwrap();
		commit(&mut guard, &next);
		guard
			.handle(
				Request::Retire {
					lease: lease.clone(),
					target: first.id,
				},
				std::process::id() as i32,
			)
			.unwrap();
		assert_eq!(guard.journal.as_ref().unwrap().baseline, baseline);
		assert!(
			guard
				.handle(
					Request::Restore {
						lease: Some("old-lease".into())
					},
					std::process::id() as i32
				)
				.is_err()
		);
		assert_eq!(guard.phase, "active");
		guard.restore().unwrap();
		assert_eq!(state.borrow().current, baseline);
	}
	#[test]
	fn stale_grace_update_cannot_restore_an_authenticated_reconnect() {
		let (_directory, mut guard, _) = fixture();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		let lease = guard.journal.as_ref().unwrap().lease.clone();
		guard
			.handle(
				Request::Grace {
					lease: lease.clone(),
					deadline_ms: None,
					revision: 2,
				},
				std::process::id() as i32,
			)
			.unwrap();
		guard
			.handle(
				Request::Grace {
					lease,
					deadline_ms: Some(0),
					revision: 1,
				},
				std::process::id() as i32,
			)
			.unwrap();
		guard.tick();
		assert_eq!(guard.phase, "active");
		assert!(guard.journal.is_some());
	}
	#[test]
	fn guard_enforces_grace_even_if_daemon_stops_scheduling() {
		let (_directory, mut guard, _) = fixture();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		let lease = guard.journal.as_ref().unwrap().lease.clone();
		guard
			.handle(
				Request::Grace {
					lease,
					deadline_ms: Some(0),
					revision: 1,
				},
				std::process::id() as i32,
			)
			.unwrap();
		guard.tick();
		assert_eq!(guard.phase, "idle");
	}
	#[test]
	fn missing_monitor_retains_virtual_output_and_recovers_after_hotplug() {
		let (_directory, mut guard, state) = fixture();
		let physical = state.borrow().current.outputs[0].clone();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		state.borrow_mut().current.outputs.retain(|o| o.key != physical.key);
		guard.recover();
		assert_eq!(guard.phase, "recovery_required");
		assert!(state.borrow().retired.is_empty());
		state.borrow_mut().current.outputs.push(physical);
		guard.restore().unwrap();
		assert_eq!(guard.phase, "idle");
	}
	#[test]
	fn guard_restart_recovers_durable_transaction_without_adopting_owner() {
		let (_directory, mut first, state) = fixture();
		let target = prepare(&mut first);
		commit(&mut first, &target);
		let saved = load(&first.path).unwrap().unwrap();
		first.journal = None;
		let mut restarted = Guard {
			native: Box::new(Fake(state.clone())),
			identity: first.identity.clone(),
			path: first.path.clone(),
			desktop: "test".into(),
			supported: true,
			journal: Some(saved),
			owner: None,
			owner_connection: None,
			owner_pid: 0,
			audio: None,
			phase: "idle".into(),
			error: None,
			grace_deadline_ms: None,
			grace_deadline: None,
			grace_revision: 0,
			last_check: Instant::now(),
			last_topology: None,
		};
		restarted.recover();
		assert_eq!(restarted.phase, "idle");
		assert!(!restarted.path.exists());
	}
	#[test]
	fn cannot_replay_journal_into_another_graphical_login() {
		let (_directory, mut guard, state) = fixture();
		let target = prepare(&mut guard);
		commit(&mut guard, &target);
		guard.identity.session = Some("another-login".into());
		guard.recover();
		assert_eq!(guard.phase, "recovery_required");
		assert!(state.borrow().retired.is_empty());
	}
	#[test]
	fn owner_process_death_restores_without_server_cooperation() {
		let (_directory, mut guard, _) = fixture();
		let mut child = std::process::Command::new("sleep").arg("10").spawn().unwrap();
		let target = guard
			.handle(
				Request::Prepare {
					lease: None,
					mode: Mode {
						width: 1920,
						height: 1080,
						refresh_millihertz: 60000,
					},
					audio: false,
				},
				child.id() as i32,
			)
			.unwrap()
			.unwrap();
		commit(&mut guard, &target);
		child.kill().unwrap();
		child.wait().unwrap();
		guard.tick();
		assert_eq!(guard.phase, "idle");
	}
	#[test]
	fn unknown_or_corrupt_journal_never_starts_a_new_transaction() {
		let (directory, _, _) = fixture();
		let path = directory.path().join("recovery.json");
		std::fs::write(&path, b"{broken").unwrap();
		assert!(load(&path).is_err());
		std::fs::write(&path, vec![b' '; wire::MAX_MESSAGE as usize + 1]).unwrap();
		assert!(load(&path).is_err());
	}
}

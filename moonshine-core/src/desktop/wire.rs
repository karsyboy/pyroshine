//! Private, bounded, same-UID lifecycle RPC. No video or audio payloads use it.
use super::{DesktopStatus, Mode, Result, Target};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) const MAX_MESSAGE: u64 = 256 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(super) enum Request {
	Status,
	Watch {
		lease: String,
	},
	Prepare {
		lease: Option<String>,
		mode: Mode,
		audio: bool,
		#[serde(default)]
		client: Option<String>,
	},
	SelectClient {
		lease: String,
		client: String,
	},
	Commit {
		lease: String,
		target: String,
	},
	Retire {
		lease: String,
		target: String,
	},
	Grace {
		lease: String,
		deadline_ms: Option<u64>,
		revision: u64,
	},
	Restore {
		lease: Option<String>,
	},
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Reply {
	pub status: DesktopStatus,
	pub lease: Option<String>,
	pub target: Option<Target>,
	pub error: Option<String>,
}
pub(super) fn socket_path() -> Result<PathBuf> {
	Ok(dirs::runtime_dir()
		.ok_or("XDG_RUNTIME_DIR is not set")?
		.join("moonshine/desktop-guard.sock"))
}
pub(super) fn read_message<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
	let mut line = String::new();
	let length = BufReader::new(stream.take(MAX_MESSAGE + 1))
		.read_line(&mut line)
		.map_err(|e| e.to_string())?;
	if length == 0 || length as u64 > MAX_MESSAGE || !line.ends_with('\n') {
		return Err("Invalid or oversized desktop guard message".into());
	}
	serde_json::from_str(&line).map_err(|e| e.to_string())
}
pub(super) fn write_message<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
	let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
	if bytes.len() as u64 >= MAX_MESSAGE {
		return Err("Desktop guard message is too large".into());
	}
	bytes.push(b'\n');
	stream.write_all(&bytes).map_err(|e| e.to_string())
}
pub(super) fn request(request: &Request) -> Result<Reply> {
	exchange(request).map(|(reply, _)| reply)
}
/// Send `request` and return the reply with the process ID of the guard that
/// answered, when the socket reports one for this user.
fn exchange(request: &Request) -> Result<(Reply, Option<i32>)> {
	let mut stream = UnixStream::connect(socket_path()?).map_err(|e| format!("Desktop guard unavailable: {e}"))?;
	let pid = super::guard::peer(&stream).ok().map(|credentials| credentials.pid);
	stream
		.set_read_timeout(Some(Duration::from_secs(20)))
		.map_err(|e| e.to_string())?;
	stream
		.set_write_timeout(Some(Duration::from_secs(2)))
		.map_err(|e| e.to_string())?;
	write_message(&mut stream, request)?;
	let reply: Reply = read_message(&mut stream)?;
	if let Some(error) = &reply.error {
		return Err(error.clone());
	}
	Ok((reply, pid))
}
/// Whether process `pid` runs an executable that was replaced on disk after it
/// started, as a package upgrade does: the kernel then reports it deleted.
fn executable_replaced(pid: i32) -> bool {
	use std::os::unix::ffi::OsStrExt;
	std::fs::read_link(format!("/proc/{pid}/exe"))
		.is_ok_and(|path| path.as_os_str().as_bytes().ends_with(b" (deleted)"))
}
fn wait_command(mut command: std::process::Command) -> Result<()> {
	command
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null());
	let mut child = command.spawn().map_err(|e| e.to_string())?;
	let deadline = Instant::now() + Duration::from_secs(8);
	loop {
		if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
			return if status.success() {
				Ok(())
			} else {
				Err("User-session guard service could not start".into())
			};
		}
		if Instant::now() >= deadline {
			let _ = child.kill();
			let _ = child.wait();
			return Err("Desktop guard service start timed out".into());
		}
		std::thread::sleep(Duration::from_millis(20));
	}
}
pub(crate) fn ensure() -> Result<()> {
	match exchange(&Request::Status) {
		// The guard owns the native desktop code but runs in the user session,
		// so upgrading and restarting the server leaves it on the old binary.
		// Replace it only while it holds no lease: then nothing needs recovery.
		Ok((reply, Some(pid))) if reply.lease.is_none() && executable_replaced(pid) => {
			tracing::info!(pid, "Restarting the idle desktop guard: its executable was upgraded");
			let mut stop = std::process::Command::new("systemctl");
			stop.args(["--user", "stop", "pyroshine-desktop-guard.service"]);
			if let Err(error) = wait_command(stop) {
				tracing::warn!(%error, "Keeping the outdated desktop guard; it could not be stopped");
				return Ok(());
			}
		},
		Ok(_) => return Ok(()),
		Err(_) => {},
	}
	let mut start = std::process::Command::new("systemctl");
	start.args(["--user", "start", "pyroshine-desktop-guard.service"]);
	if wait_command(start).is_err() {
		// Development/portable builds use their exact executable in a separate
		// user unit, so the guard survives the server's own cgroup being killed.
		let mut start = std::process::Command::new("systemd-run");
		start
			.args([
				"--user",
				"--unit=pyroshine-desktop-guard",
				"--collect",
				"--property=Restart=on-failure",
				"--property=UMask=0077",
			])
			.arg(std::env::current_exe().map_err(|e| e.to_string())?)
			.args(["desktop", "guard"]);
		wait_command(start)?;
	}
	let deadline = Instant::now() + Duration::from_secs(5);
	while Instant::now() < deadline {
		if request(&Request::Status).is_ok() {
			return Ok(());
		}
		std::thread::sleep(Duration::from_millis(50));
	}
	Err("Desktop guard did not become ready; inspect its user-session service log".into())
}
#[derive(Clone, Debug)]
pub(crate) struct Client {
	pub lease: String,
	_owner: Option<std::sync::Arc<UnixStream>>,
}
impl Client {
	pub fn observer(lease: String) -> Self {
		Self { lease, _owner: None }
	}
	pub fn prepare(mode: Mode, audio: bool, client: Option<String>) -> Result<(Self, Target)> {
		ensure()?;
		let reply = request(&Request::Prepare {
			lease: None,
			mode,
			audio,
			client,
		})?;
		let lease = reply.lease.ok_or("Guard supplied no desktop lease")?;
		let target = reply.target.ok_or("Guard supplied no capture target")?;
		let watch = (|| -> Result<UnixStream> {
			let mut stream = UnixStream::connect(socket_path()?).map_err(|e| e.to_string())?;
			stream
				.set_read_timeout(Some(Duration::from_secs(5)))
				.map_err(|e| e.to_string())?;
			write_message(&mut stream, &Request::Watch { lease: lease.clone() })?;
			let reply: Reply = read_message(&mut stream)?;
			if let Some(error) = reply.error {
				return Err(error);
			}
			Ok(stream)
		})();
		match watch {
			Ok(stream) => Ok((
				Self {
					lease,
					_owner: Some(std::sync::Arc::new(stream)),
				},
				target,
			)),
			Err(error) => {
				let _ = request(&Request::Restore { lease: Some(lease) });
				Err(error)
			},
		}
	}
	pub fn replacement(&self, mode: Mode) -> Result<Target> {
		request(&Request::Prepare {
			lease: Some(self.lease.clone()),
			mode,
			audio: false,
			client: None,
		})?
		.target
		.ok_or_else(|| "Guard supplied no replacement target".into())
	}
	pub fn select_client(&self, client: String) -> Result<()> {
		request(&Request::SelectClient {
			lease: self.lease.clone(),
			client,
		})
		.map(|_| ())
	}
	pub fn commit(&self, target: &str) -> Result<Target> {
		request(&Request::Commit {
			lease: self.lease.clone(),
			target: target.into(),
		})?
		.target
		.ok_or_else(|| "Guard supplied no committed target".into())
	}
	pub fn retire(&self, target: &str) -> Result<()> {
		request(&Request::Retire {
			lease: self.lease.clone(),
			target: target.into(),
		})
		.map(|_| ())
	}
	pub fn grace(&self, deadline_ms: Option<u64>, revision: u64) -> Result<()> {
		request(&Request::Grace {
			lease: self.lease.clone(),
			deadline_ms,
			revision,
		})
		.map(|_| ())
	}
	pub fn restore(&self) -> Result<()> {
		request(&Request::Restore {
			lease: Some(self.lease.clone()),
		})
		.map(|_| ())
	}
}

#[cfg(test)]
mod tests {
	/// An upgrade deletes the running guard's executable; the current one is not.
	#[test]
	fn detects_a_process_whose_executable_was_replaced() {
		assert!(!super::executable_replaced(std::process::id() as i32));
		let directory = tempfile::tempdir().unwrap();
		let copy = directory.path().join("sleep");
		std::fs::copy("/usr/bin/sleep", &copy).unwrap();
		let mut child = std::process::Command::new(&copy).arg("30").spawn().unwrap();
		assert!(!super::executable_replaced(child.id() as i32));
		std::fs::remove_file(&copy).unwrap();
		assert!(super::executable_replaced(child.id() as i32));
		child.kill().unwrap();
		child.wait().unwrap();
	}
	use super::*;
	#[test]
	fn bounded_protocol_rejects_truncation_and_oversized_requests() {
		for data in [b"{}".to_vec(), vec![b'x'; MAX_MESSAGE as usize + 1]] {
			let (mut writer, mut reader) = UnixStream::pair().unwrap();
			let thread = std::thread::spawn(move || {
				let _ = writer.write_all(&data);
			});
			assert!(read_message::<Request>(&mut reader).is_err());
			drop(reader);
			thread.join().unwrap();
		}
	}
}

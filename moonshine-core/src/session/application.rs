use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use zbus::proxy::SignalStream;
use zbus::{Connection, MatchRule, MessageStream, Proxy};
use zvariant::OwnedObjectPath;

pub fn default_launch_timeout() -> u64 {
	2
}

/// Configuration for a single application that can be launched in a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplicationConfig {
	/// Title of the application.
	pub title: String,

	/// Path to a boxart image.
	pub boxart: Option<PathBuf>,

	/// The command to run.
	pub command: Vec<String>,

	/// Optional scale advertised by the compositor for this application's output.
	/// The stream resolution remains the physical framebuffer size.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub output_scale: Option<f64>,

	/// Commands to run before launching the application.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub pre_command: Vec<Vec<String>>,

	/// Commands to run after the streaming session ends.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub post_command: Vec<Vec<String>>,

	/// systemd StandardOutput value. If not set, defaults to "null".
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stdout: Option<String>,

	/// systemd StandardError value. If not set, defaults to "null".
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stderr: Option<String>,

	/// Seconds to wait for the application to reach an active state after launch.
	#[serde(default = "default_launch_timeout")]
	pub launch_timeout_secs: u64,
}

impl Default for ApplicationConfig {
	fn default() -> Self {
		Self {
			title: String::new(),
			boxart: None,
			command: Vec::new(),
			output_scale: None,
			pre_command: Vec::new(),
			post_command: Vec::new(),
			stdout: None,
			stderr: None,
			launch_timeout_secs: default_launch_timeout(),
		}
	}
}

impl ApplicationConfig {
	pub fn id(&self) -> i32 {
		let mut hasher = DefaultHasher::new();
		self.title.hash(&mut hasher);
		// Clients only accept non-negative application IDs.
		(hasher.finish() as i32) & i32::MAX
	}
}

use crate::session::failure::{SessionFailure, StopCause, StopCauseSlot};
use crate::session::manager::SessionShutdownReason;

const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const PROPERTIES_CHANGED: &str = "PropertiesChanged";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";
const ACTIVE_STATE_PROPERTY: &str = "ActiveState";

/// systemd's `TimeoutStopUSec` for the application unit: how long the
/// application may take to exit after SIGTERM before systemd sends SIGKILL.
/// The same allowance bounds `ExecStopPost` hooks.
const UNIT_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Client-side wait for the stop job: the SIGTERM allowance, the post-hook
/// allowance, and slack for SIGKILL and job bookkeeping. A process that exits
/// within systemd's allowance is never reported as a failed stop.
const STOP_JOB_TIMEOUT: Duration = Duration::from_secs(2 * UNIT_STOP_TIMEOUT.as_secs() + 2);
/// Wait for systemd to report the unit inactive with no processes, or unloaded.
const UNIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound of [`stop_application_unit`], including the session-bus
/// connection. The session manager's teardown allows exactly this long.
pub(crate) const APPLICATION_STOP_DEADLINE: Duration =
	Duration::from_secs(STOP_JOB_TIMEOUT.as_secs() + UNIT_SETTLE_TIMEOUT.as_secs() + 1);

/// Start-job wait (includes `ExecStartPre`), decoupled from `launch_timeout_secs` so a `pre_command` isn't cut off.
const START_JOB_TIMEOUT: Duration = Duration::from_secs(90);

/// systemd only emits JobRemoved/UnitRemoved/PropertiesChanged bus signals to
/// connections that have called org.freedesktop.systemd1.Manager.Subscribe().
/// Without it, waiting for those signals only works when some *other* client
/// on the user bus happens to hold a subscription. Call this once for every
/// new session bus connection, before waiting on any systemd signals.
async fn subscribe_to_systemd_signals(conn: &Connection) -> Result<(), ()> {
	conn.call_method(Some(SYSTEMD_BUS), SYSTEMD_PATH, Some(SYSTEMD_MANAGER), "Subscribe", &())
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to systemd signals: {e}"))?;
	Ok(())
}

#[derive(Clone)]
pub(crate) struct LaunchOptions<'a> {
	/// Application title, for operator-facing messages.
	pub title: &'a str,
	pub unit_name: &'a str,
	pub program: &'a str,
	pub args: &'a [String],
	pub envs: &'a [String],
	pub timeout: Duration,
	pub pre_commands: &'a Vec<Vec<String>>,
	pub post_commands: &'a Vec<Vec<String>>,
	pub stdout_value: &'a Option<String>,
	pub stderr_value: &'a Option<String>,
}

/// Runtime context required to launch an application.
pub(crate) struct ApplicationContext {
	/// systemd transient unit name (e.g. `"moonshine-session.service"`).
	pub unit_name: String,
	/// Path to the PulseAudio socket created by the audio stream.
	pub pulse_socket_path: PathBuf,
	/// X11 display number reported by XWayland (e.g. `0` → `":0"`).
	pub xdisplay: u32,
	/// Wayland socket name reported by the compositor.
	pub wayland_display: String,
	/// Effective HDR mode — `true` only when the compositor confirmed an HDR-capable DMA-BUF format is in use.
	pub hdr: bool,
	/// Environment variables to pass on.
	pub extra_env: HashMap<String, String>,
}

/// A launched application unit and its exit monitor.
///
/// Dropping this only stops observing the unit. Stopping the unit is an
/// asynchronous systemd job owned by the session manager's teardown (see
/// [`stop_application_unit`]), which records the unit before the launch starts
/// so a cancelled launch is cleaned up as well.
pub(crate) struct Application {
	config: ApplicationConfig,
	exit_monitor: Option<JoinHandle<()>>,
}

impl Application {
	/// Launch the application unit. `cause` receives the application's exit
	/// status if it ends the session.
	pub async fn spawn(
		config: ApplicationConfig,
		context: ApplicationContext,
		stop: ShutdownManager<SessionShutdownReason>,
		cause: StopCauseSlot,
	) -> Result<Self, SessionFailure> {
		let Some(program) = config.command.first() else {
			tracing::error!("Application command is empty.");
			return Err(SessionFailure::new(
				"application_command_empty",
				format!("The application \"{}\" has no command.", config.title),
			)
			.with_hint("Set its command in the Pyroshine configuration."));
		};
		let args = &config.command[1..];
		let envs = make_envs(&context)?;

		tracing::info!(program, ?args, "Launching application.");

		// Connect to the user session bus.
		let conn = Connection::session().await.map_err(|e| {
			tracing::error!("Failed to connect to session bus: {e}");
			session_bus_failure(&e.to_string())
		})?;
		subscribe_to_systemd_signals(&conn)
			.await
			.map_err(|()| session_bus_failure("subscribing to systemd signals failed"))?;

		// Stop any leftover unit from a previous session. A unit whose processes
		// cannot be shown gone, or that is still loaded, cannot be replaced
		// safely: refuse the launch rather than overlap it.
		stop_unit(&conn, &context.unit_name, Settled::Unloaded)
			.await
			.map_err(|()| {
				tracing::error!(
					unit = context.unit_name,
					"A previous application unit is still present; refusing to launch over it"
				);
				SessionFailure::new(
					"previous_application_running",
					"The previous session's application is still running.",
				)
				.with_hint(format!("Stop it with: systemctl --user stop {}", context.unit_name))
			})?;

		// Launch the application as a transient systemd service unit.
		let options = LaunchOptions {
			title: &config.title,
			unit_name: &context.unit_name,
			program,
			args,
			envs: &envs,
			timeout: Duration::from_secs(config.launch_timeout_secs),
			pre_commands: &config.pre_command,
			post_commands: &config.post_command,
			stdout_value: &config.stdout,
			stderr_value: &config.stderr,
		};

		let watch = match start_transient_service(&conn, &options).await {
			Ok(started) => started,
			Err(failure) => {
				// Best effort cleanup on launch failure; the session's teardown
				// stops (and verifies) the unit again.
				stop_unit(&conn, &context.unit_name, Settled::Terminated).await.ok();
				return Err(failure);
			},
		};
		let exit_monitor = spawn_unit_exit_monitor(
			conn.clone(),
			ExitReport {
				title: config.title.clone(),
				unit_name: context.unit_name.clone(),
				output: output_hint(&context.unit_name, &config.stdout, &config.stderr),
				launched_at: Instant::now(),
			},
			watch,
			stop,
			cause,
		);

		Ok(Self {
			config,
			exit_monitor: Some(exit_monitor),
		})
	}
}

fn session_bus_failure(error: &str) -> SessionFailure {
	SessionFailure::new(
		"session_bus_unavailable",
		format!("Pyroshine cannot reach the user session bus to start the application ({error})."),
	)
	.with_hint(
		"Pyroshine starts applications through the user's systemd instance. Run it as the logged-in desktop user \
		 (the packaged pyroshine@USER service does this).",
	)
}

/// Where the operator can read the application's output, as configured by
/// its `stdout`/`stderr` settings.
fn output_hint(unit_name: &str, stdout: &Option<String>, stderr: &Option<String>) -> String {
	let journal = format!("journalctl --user -u {unit_name} -e");
	for value in [stderr, stdout].into_iter().flatten() {
		let (setting, path) = split_standard_io(&Some(value.clone()));
		if let Some((_, path)) = path.filter(|(property, _)| *property != "FileDescriptorName") {
			return format!("Its output is in {path}.");
		}
		if setting.starts_with("journal") || setting.starts_with("kmsg") {
			return format!("Read its output with: {journal}");
		}
	}
	format!(
		"Its output is discarded: set stdout = \"journal\" and stderr = \"journal\" for this application, \
		 launch it again, then read it with: {journal}"
	)
}

impl Drop for Application {
	fn drop(&mut self) {
		// Never block here: this runs on runtime workers. The owning session's
		// teardown stops the unit before it reports completion.
		tracing::debug!("Releasing application '{}' handle.", self.config.title);
		if let Some(handle) = self.exit_monitor.take() {
			handle.abort();
		}
	}
}

/// Stop the session's application unit and establish that it terminated.
///
/// `Ok` means systemd reports the unit gone, or inactive/failed with no
/// processes left in its cgroup (it may still be awaiting garbage collection).
/// `Err` means termination could not be established: the stop failed, timed
/// out, or the bus was unavailable. Idempotent; bounded by
/// [`APPLICATION_STOP_DEADLINE`].
pub(crate) async fn stop_application_unit(unit_name: &str) -> Result<(), ()> {
	tracing::info!(unit = unit_name, "Stopping application unit.");
	stop_unit_owned(unit_name.to_string()).await
}

/// Build environment variables for the application based on the context (e.g. display, PulseAudio socket).
fn make_envs(context: &ApplicationContext) -> Result<Vec<String>, SessionFailure> {
	// Build environment variables as "KEY=value" strings for systemd.
	let mut envs: Vec<String> = vec![
		format!("PULSE_SERVER=unix:{}", context.pulse_socket_path.display()),
		format!(
			"PULSE_RUNTIME_PATH={}",
			context
				.pulse_socket_path
				.parent()
				.ok_or_else(|| {
					tracing::error!("Failed to get parent directory of PulseAudio socket.");
					SessionFailure::see_log("internal_error", "The audio socket path is invalid.")
				})?
				.to_string_lossy()
		),
		format!("DISPLAY=:{}", context.xdisplay),
		format!("WAYLAND_DISPLAY={}", context.wayland_display),
		// Force Proton to use winepulse.drv instead of winepipewire.drv,
		// so it respects PULSE_SERVER and routes audio through Moonshine.
		"PROTON_USE_PIPEWIRE=0".to_string(),
	];

	// Impersonate gamescope so Steam uses its external-overlay mode.
	envs.push("XDG_CURRENT_DESKTOP=gamescope".to_string());
	envs.push(format!("GAMESCOPE_WAYLAND_DISPLAY={}", context.wayland_display));
	envs.push(format!("STEAM_GAME_DISPLAY_0=:{}", context.xdisplay));
	envs.push("STEAM_GAMESCOPE_FANCY_SCALING_SUPPORT=1".to_string());
	envs.push("STEAM_GAMESCOPE_NIS_SUPPORTED=1".to_string());
	envs.push("STEAM_GAMESCOPE_VRR_SUPPORTED=1".to_string());

	if context.hdr {
		// DXVK's dxgi.dll gates HDR color space exposure on this env var.
		// Without it, both DX11 (DXVK) and DX12 (vkd3d-proton via DXVK dxgi)
		// games will not see HDR as available.
		envs.push("DXVK_HDR=1".to_string());
	}

	for (key, value) in &context.extra_env {
		if obsolete_presentation_environment(key, value) {
			tracing::error!(
				key,
				"Removed presentation-layer setting; remove it from the application environment"
			);
			return Err(SessionFailure::new(
				"application_environment_obsolete",
				format!("The application environment sets {key}, a removed presentation-layer setting."),
			)
			.with_hint("Remove it from the application's environment."));
		}
		envs.push(format!("{key}={value}"));
	}

	Ok(envs)
}

// Upgrade validation only: these settings have no supported native equivalent.
fn obsolete_presentation_environment(key: &str, value: &str) -> bool {
	key.starts_with("MOONSHINE_WSI_")
		|| matches!(
			key,
			"ENABLE_MOONSHINE_WSI"
				| "DISABLE_MOONSHINE_WSI"
				| "MOONSHINE_WAYLAND_DISPLAY"
				| "MOONSHINE_HDR"
				| "MOONSHINE_LIMITER_FILE"
		) || (key == "VK_INSTANCE_LAYERS" && value.split(':').any(|layer| layer == "VK_LAYER_MOONSHINE_wsi"))
}

/// Wait for a `JobRemoved` signal matching the given job path, accepting only
/// `"done"` as success. The error is systemd's job result (for example
/// `failed`), `timeout` or `unavailable`.
async fn wait_for_job_signal(
	job_stream: &mut SignalStream<'_>,
	job_path: &OwnedObjectPath,
	timeout: Duration,
	label: &str,
) -> Result<(), String> {
	let result = tokio::time::timeout(timeout, async {
		while let Some(message) = job_stream.next().await {
			let body: (u32, OwnedObjectPath, String, String) = message.body().deserialize().map_err(|e| {
				tracing::error!("Failed to deserialize JobRemoved signal: {e}");
				"unavailable".to_string()
			})?;
			let (_, ref path, ref unit, ref result) = body;

			if path != job_path {
				continue;
			}

			return match result.as_str() {
				"done" => Ok(()),
				other => {
					tracing::warn!(result = other, unit = unit, "{label} job finished unsuccessfully.");
					Err(other.to_string())
				},
			};
		}
		Err("unavailable".to_string())
	})
	.await;

	match result {
		Ok(Ok(())) => Ok(()),
		Ok(Err(result)) => {
			tracing::warn!(label, "Received failure result for {label} job.");
			Err(result)
		},
		Err(_) => {
			tracing::warn!(timeout_secs = timeout.as_secs(), "Timed out waiting for {label} job.");
			Err("timeout".to_string())
		},
	}
}

/// What a stop must reach before it counts as complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settled {
	/// No process of the unit remains (it may still be loaded).
	Terminated,
	/// The unit is unloaded, so a transient unit of the same name can start.
	Unloaded,
}

/// The unit as systemd reports it after a stop.
#[derive(Debug, PartialEq, Eq)]
enum UnitState {
	Absent,
	Loaded { active_state: String, processes: usize },
}

impl UnitState {
	fn reached(&self, settled: Settled) -> bool {
		match self {
			Self::Absent => true,
			Self::Loaded {
				active_state,
				processes,
			} => {
				settled == Settled::Terminated
					&& *processes == 0
					&& matches!(active_state.as_str(), "inactive" | "failed")
			},
		}
	}
}

fn no_such_unit(error: &zbus::Error) -> bool {
	matches!(error, zbus::Error::MethodError(name, ..) if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit")
}

/// The cgroup of the running application unit (relative to the cgroup2
/// root), for attributing GPU time to the application. `None` when the unit
/// is not loaded or the user's systemd cannot be reached.
pub(crate) async fn application_cgroup() -> Option<String> {
	let unit_name = super::application_unit_name();
	let conn = Connection::session().await.ok()?;
	let path: OwnedObjectPath = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnit",
			&(unit_name,),
		)
		.await
		.ok()?
		.body()
		.deserialize()
		.ok()?;
	let cgroup: zvariant::OwnedValue = conn
		.call_method(
			Some(SYSTEMD_BUS),
			&path,
			Some(PROPERTIES_INTERFACE),
			"Get",
			&(SERVICE_INTERFACE, "ControlGroup"),
		)
		.await
		.ok()?
		.body()
		.deserialize()
		.ok()?;
	String::try_from(cgroup).ok().filter(|cgroup| !cgroup.is_empty())
}

/// Query whether `unit_name` is loaded, its `ActiveState` and the processes
/// left in its cgroup.
async fn unit_state(conn: &Connection, unit_name: &str) -> Result<UnitState, ()> {
	let path: OwnedObjectPath = match conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnit",
			&(unit_name,),
		)
		.await
	{
		Ok(reply) => reply
			.body()
			.deserialize()
			.map_err(|e| tracing::warn!("Failed to deserialize GetUnit reply for {unit_name}: {e}"))?,
		Err(error) if no_such_unit(&error) => return Ok(UnitState::Absent),
		Err(error) => {
			tracing::warn!("Failed to look up unit {unit_name}: {error}");
			return Err(());
		},
	};
	let active_state: zvariant::OwnedValue = conn
		.call_method(
			Some(SYSTEMD_BUS),
			&path,
			Some(PROPERTIES_INTERFACE),
			"Get",
			&(UNIT_INTERFACE, ACTIVE_STATE_PROPERTY),
		)
		.await
		.map_err(|e| tracing::warn!("Failed to read ActiveState of {unit_name}: {e}"))?
		.body()
		.deserialize()
		.map_err(|e| tracing::warn!("Failed to deserialize ActiveState of {unit_name}: {e}"))?;
	let active_state = String::try_from(active_state)
		.map_err(|e| tracing::warn!("Unexpected ActiveState type for {unit_name}: {e}"))?;
	let processes: Vec<(String, u32, String)> = match conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnitProcesses",
			&(unit_name,),
		)
		.await
	{
		Ok(reply) => reply
			.body()
			.deserialize()
			.map_err(|e| tracing::warn!("Failed to deserialize the processes of {unit_name}: {e}"))?,
		Err(error) if no_such_unit(&error) => return Ok(UnitState::Absent),
		Err(error) => {
			tracing::warn!("Failed to list the processes of {unit_name}: {error}");
			return Err(());
		},
	};
	Ok(UnitState::Loaded {
		active_state,
		processes: processes.len(),
	})
}

/// Stop a unit via the user session bus and establish the `settled` outcome.
///
/// The stop job's own result is informative only: systemd may report
/// `canceled` (another stop replaced it) or `failed` (an `ExecStopPost` hook
/// failed) after the processes are gone. What counts is the unit state
/// afterwards, observed for up to [`UNIT_SETTLE_TIMEOUT`].
async fn stop_unit(conn: &Connection, unit_name: &str, settled: Settled) -> Result<(), ()> {
	// Subscribe to JobRemoved before calling StopUnit to avoid races.
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut job_removed_stream = proxy
		.receive_signal("JobRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to JobRemoved signals: {e}"))?;

	// Call StopUnit, which queues a stop job but does not wait for it to complete.
	match conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"StopUnit",
			&(unit_name, "replace"),
		)
		.await
	{
		Ok(reply) => {
			let job_path: OwnedObjectPath = reply
				.body()
				.deserialize()
				.map_err(|e| tracing::warn!("Failed to deserialize StopUnit reply for {unit_name}: {e}"))?;
			// Bounded by systemd's own stop policy; the state check decides.
			let _ = wait_for_job_signal(&mut job_removed_stream, &job_path, STOP_JOB_TIMEOUT, "Stop").await;
		},
		Err(error) if no_such_unit(&error) => {
			tracing::debug!(unit = unit_name, "Unit was already stopped.");
			return Ok(());
		},
		Err(error) => {
			tracing::error!("Failed to stop unit {unit_name}: {error}");
			return Err(());
		},
	}

	let deadline = tokio::time::Instant::now() + UNIT_SETTLE_TIMEOUT;
	let mut last = None;
	loop {
		match unit_state(conn, unit_name).await {
			Ok(state) if state.reached(settled) => {
				tracing::debug!(unit = unit_name, ?state, "Unit stopped.");
				return Ok(());
			},
			Ok(state) => last = Some(state),
			Err(()) => {},
		}
		if tokio::time::Instant::now() >= deadline {
			tracing::error!(
				unit = unit_name,
				?settled,
				state = ?last,
				"Could not establish that the unit stopped"
			);
			return Err(());
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
}

/// Stop a unit with a new session bus connection.
async fn stop_unit_owned(unit_name: String) -> Result<(), ()> {
	let conn = Connection::session().await.map_err(|e| {
		tracing::error!("Failed to connect to session bus: {e}");
	})?;
	subscribe_to_systemd_signals(&conn).await?;
	stop_unit(&conn, &unit_name, Settled::Terminated).await
}

/// How the application's main process ended, as systemd reported it on the
/// unit's `Service` interface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ExitStatus {
	/// systemd's service `Result`, for example `success`, `exit-code`,
	/// `signal`, `core-dump` or `oom-kill`.
	result: Option<String>,
	/// `ExecMainCode`: `CLD_EXITED` (1), `CLD_KILLED` (2) or `CLD_DUMPED` (3).
	code: Option<i32>,
	/// `ExecMainStatus`: the exit status or the signal number.
	status: Option<i32>,
}

const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;
const CLD_DUMPED: i32 = 3;

impl ExitStatus {
	fn absorb(&mut self, changed: &HashMap<String, zvariant::Value<'_>>) {
		if let Some(zvariant::Value::Str(result)) = changed.get("Result") {
			self.result = Some(result.to_string());
		}
		if let Some(zvariant::Value::I32(code)) = changed.get("ExecMainCode") {
			// Zero means no main process has exited yet.
			self.code = (*code != 0).then_some(*code);
		}
		if let Some(zvariant::Value::I32(status)) = changed.get("ExecMainStatus") {
			self.status = Some(*status);
		}
	}

	/// What happened to the process, completing "<title> ...".
	fn describe(&self) -> String {
		if self.result.as_deref() == Some("oom-kill") {
			return "was killed because the system ran out of memory".into();
		}
		match (self.code, self.status) {
			(Some(CLD_EXITED), Some(0)) => "exited normally".into(),
			(Some(CLD_EXITED), Some(status)) => format!("exited with status {status}"),
			(Some(CLD_KILLED), Some(signal)) => format!("was killed by {}", signal_name(signal)),
			(Some(CLD_DUMPED), Some(signal)) => format!("crashed ({}, core dumped)", signal_name(signal)),
			// Without an exit code, `success` may be the value from before the
			// process started; claim nothing.
			_ => match self.result.as_deref() {
				Some("timeout") => "timed out".into(),
				_ => "exited".into(),
			},
		}
	}

	/// The process failed rather than quitting on its own: a non-zero
	/// status, a fatal signal or a resource limit.
	fn failed(&self) -> bool {
		match (self.code, self.status) {
			(Some(CLD_EXITED), Some(status)) => status != 0,
			(Some(CLD_KILLED | CLD_DUMPED), _) => true,
			_ => self
				.result
				.as_deref()
				.is_some_and(|result| !matches!(result, "success" | "exit-code")),
		}
	}

	/// A crash rather than an exit the application chose.
	fn crashed(&self) -> bool {
		self.code == Some(CLD_DUMPED)
			|| self.result.as_deref() == Some("oom-kill")
			|| (self.code == Some(CLD_KILLED)
				&& self
					.status
					.is_some_and(|signal| ![libc::SIGTERM, libc::SIGINT, libc::SIGHUP].contains(&signal)))
	}
}

fn signal_name(signal: i32) -> String {
	let name = match signal {
		libc::SIGHUP => "SIGHUP",
		libc::SIGINT => "SIGINT",
		libc::SIGQUIT => "SIGQUIT",
		libc::SIGILL => "SIGILL",
		libc::SIGABRT => "SIGABRT",
		libc::SIGBUS => "SIGBUS",
		libc::SIGFPE => "SIGFPE",
		libc::SIGKILL => "SIGKILL",
		libc::SIGSEGV => "SIGSEGV",
		libc::SIGPIPE => "SIGPIPE",
		libc::SIGTERM => "SIGTERM",
		_ => return format!("signal {signal}"),
	};
	name.to_string()
}

/// How long an exit waits for systemd's exit-status signal, which follows
/// the state change in the same batch.
const EXIT_STATUS_WAIT: Duration = Duration::from_millis(250);

/// An exit within this long after launch is reported as a startup failure.
const EARLY_EXIT: Duration = Duration::from_secs(30);

/// systemd's object path for `unit_name` (`sd_bus_path_encode`): every byte
/// other than an ASCII letter or a non-leading digit becomes `_xx`.
fn unit_object_path(unit_name: &str) -> String {
	let mut path = String::from("/org/freedesktop/systemd1/unit/");
	for (index, byte) in unit_name.bytes().enumerate() {
		if byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit()) {
			path.push(byte as char);
		} else {
			path.push_str(&format!("_{byte:02x}"));
		}
	}
	path
}

/// Property changes of one unit, subscribed before the unit starts so its exit
/// status is seen even when systemd unloads the unit right after it exits
/// (`CollectMode=inactive-or-failed`).
struct UnitWatch {
	unit_name: String,
	path: OwnedObjectPath,
	stream: MessageStream,
	exit: ExitStatus,
}

impl UnitWatch {
	async fn new(conn: &Connection, unit_name: &str, path: OwnedObjectPath) -> Result<Self, String> {
		let rule = MatchRule::builder()
			.msg_type(zbus::message::Type::Signal)
			.sender(SYSTEMD_BUS)
			.and_then(|builder| builder.path(path.clone()))
			.and_then(|builder| builder.interface(PROPERTIES_INTERFACE))
			.and_then(|builder| builder.member(PROPERTIES_CHANGED))
			.map_err(|e| format!("cannot build the match rule: {e}"))?
			.build();
		let stream = MessageStream::for_match_rule(rule, conn, None)
			.await
			.map_err(|e| format!("cannot follow the unit: {e}"))?;
		Ok(Self {
			unit_name: unit_name.to_string(),
			path,
			stream,
			exit: ExitStatus::default(),
		})
	}

	/// The next terminal `ActiveState` (`inactive` or `failed`), recording
	/// exit-status changes on the way. `None` once the stream ends or a signal
	/// cannot be read.
	///
	/// The subscription predates the unit, so it also delivers the state the
	/// unit was loaded in (`inactive`, before it started). A terminal state is
	/// therefore confirmed with systemd before it counts: a unit that is still
	/// running, or gone (unloaded after it exited), answers authoritatively. A
	/// confirmation that fails even after retrying is no answer: the signal is
	/// not trusted and watching continues. A later state change is confirmed
	/// again, and the exit monitor also ends on `UnitRemoved`.
	async fn next_terminal_state(&mut self, conn: &Connection) -> Option<&'static str> {
		loop {
			let state = self.next_active_state().await?;
			let Some(terminal) = terminal_state(&state) else {
				continue;
			};
			match exit_confirmed(&query_activity(conn, &self.unit_name).await) {
				Some(true) => {
					// A stale state can be confirmed when the process has exited
					// since: its exit status is then still queued behind it.
					self.drain_exit_status().await;
					return Some(terminal);
				},
				Some(false) => {},
				None => tracing::warn!(
					unit = self.unit_name,
					"Could not confirm a reported application exit with systemd; still watching"
				),
			}
		}
	}

	/// Read queued signals until the exit status is known, briefly.
	async fn drain_exit_status(&mut self) {
		let deadline = tokio::time::Instant::now() + EXIT_STATUS_WAIT;
		while self.exit.code.is_none() {
			match tokio::time::timeout_at(deadline, self.next_active_state()).await {
				Ok(Some(_)) => {},
				Ok(None) | Err(_) => return,
			}
		}
	}

	async fn next_active_state(&mut self) -> Option<String> {
		while let Some(Ok(signal)) = self.stream.next().await {
			let body = signal.body();
			let Ok((iface, changed, _)) =
				body.deserialize::<(String, HashMap<String, zvariant::Value<'_>>, Vec<String>)>()
			else {
				tracing::error!("Failed to deserialize PropertiesChanged signal");
				return None;
			};
			match iface.as_str() {
				SERVICE_INTERFACE => self.exit.absorb(&changed),
				UNIT_INTERFACE => {
					if let Some(zvariant::Value::Str(state)) = changed.get(ACTIVE_STATE_PROPERTY) {
						return Some(state.to_string());
					}
				},
				_ => {},
			}
		}
		None
	}

	/// Read the exit status directly when no change signal carried it (the
	/// process exited before the subscription, or signals were coalesced).
	/// Best effort: the unit may already be unloaded.
	async fn complete_exit_status(&mut self, conn: &Connection) {
		self.drain_exit_status().await;
		if self.exit.code.is_some() {
			return;
		}
		let reply = tokio::time::timeout(
			Duration::from_millis(500),
			conn.call_method(
				Some(SYSTEMD_BUS),
				&self.path,
				Some(PROPERTIES_INTERFACE),
				"GetAll",
				&(SERVICE_INTERFACE,),
			),
		)
		.await;
		if let Ok(Ok(reply)) = reply
			&& let Ok(properties) = reply.body().deserialize::<HashMap<String, zvariant::Value<'_>>>()
		{
			self.exit.absorb(&properties);
		}
	}
}

/// What to do about an early or failed exit. A command that quits cleanly
/// right away usually handed off to a process that was already running (a
/// second `steam` launch does this), so it gets no stream of its own.
fn exit_hint(exit: &ExitStatus, output: &str) -> String {
	if exit.failed() {
		output.to_string()
	} else {
		format!(
			"A command that exits right away often hands off to an instance that is already running, or starts \
			 another process and returns; Pyroshine ends the session when the launched command exits. Close the \
			 running instance, or launch a command that stays running. {output}"
		)
	}
}

/// What the exit monitor needs to explain an exit.
struct ExitReport {
	title: String,
	unit_name: String,
	/// Where the application's output can be read.
	output: String,
	launched_at: Instant,
}

impl ExitReport {
	fn cause(&self, exit: &ExitStatus) -> StopCause {
		let elapsed = self.launched_at.elapsed();
		let early = elapsed < EARLY_EXIT;
		let detail = format!(
			"{} {}, {} after launch.",
			self.title,
			exit.describe(),
			crate::session::failure::human_duration(elapsed)
		);
		let hint = (exit.failed() || early).then(|| exit_hint(exit, &self.output));
		StopCause {
			detail,
			hint,
			unexpected: exit.crashed() || (early && exit.failed()),
			reported: false,
		}
	}
}

fn spawn_unit_exit_monitor(
	conn: Connection,
	report: ExitReport,
	mut watch: UnitWatch,
	stop: ShutdownManager<SessionShutdownReason>,
	cause: StopCauseSlot,
) -> JoinHandle<()> {
	tokio::spawn(async move {
		tokio::select! {
			state = wait_for_unit_terminal_state(&conn, &report.unit_name, &mut watch) => {
				match state {
					Ok(state) => {
						watch.complete_exit_status(&conn).await;
						let exit_cause = report.cause(&watch.exit);
						tracing::info!(
							unit = report.unit_name,
							state,
							exit = ?watch.exit,
							"{} Stopping session.",
							exit_cause.detail
						);
						// Recorded before the stop so teardown reports it.
						cause.record(exit_cause);
						let _ = stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
					},
					Err(()) => {
						tracing::warn!(unit = report.unit_name, "Application unit monitor stopped unexpectedly.");
					},
				}
			},
			_ = stop.wait_shutdown_triggered() => {},
		}
	})
}

async fn wait_for_unit_terminal_state(conn: &Connection, unit_name: &str, watch: &mut UnitWatch) -> Result<String, ()> {
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut unit_removed_stream = proxy
		.receive_signal("UnitRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to UnitRemoved signals: {e}"))?;

	// Only an answer from systemd ends the session here. If the state cannot
	// be read, the unit is followed by its signals instead.
	match query_activity(conn, unit_name).await {
		Ok(Activity::Absent) => return Ok("removed".to_string()),
		Ok(Activity::State(state)) => {
			if let Some(state) = terminal_state(&state) {
				return Ok(state.to_string());
			}
		},
		Err(error) => tracing::warn!(
			unit = unit_name,
			error,
			"Could not read the application unit's state; following its signals"
		),
	}

	loop {
		tokio::select! {
			state = watch.next_terminal_state(conn) => {
				return state.map(str::to_string).ok_or(());
			},
			message = unit_removed_stream.next() => {
				let Some(message) = message else {
					return Err(());
				};
				let (id, _path): (String, OwnedObjectPath) = message
					.body()
					.deserialize()
					.map_err(|e| tracing::error!("Failed to deserialize UnitRemoved signal: {e}"))?;
				if id == unit_name {
					return Ok("removed".to_string());
				}
			},
		}
	}
}

/// What systemd answered about a unit's activity.
#[derive(Debug, PartialEq, Eq)]
enum Activity {
	/// Loaded, with this `ActiveState`.
	State(String),
	/// Not loaded: systemd collected it after it stopped.
	Absent,
}

/// Query a unit's `ActiveState` by name. An error means systemd gave no
/// answer (bus failure, timeout, unexpected reply), never that the unit
/// stopped.
async fn unit_activity(conn: &Connection, unit_name: &str) -> Result<Activity, String> {
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnit",
			&(unit_name,),
		)
		.await;
	let path: OwnedObjectPath = match reply {
		Ok(reply) => {
			reply
				.body()
				.deserialize::<(OwnedObjectPath,)>()
				.map_err(|e| format!("unexpected GetUnit reply: {e}"))?
				.0
		},
		Err(error) if no_such_unit(&error) => return Ok(Activity::Absent),
		Err(error) => return Err(format!("GetUnit failed: {error}")),
	};
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			&path,
			Some(PROPERTIES_INTERFACE),
			"Get",
			&(UNIT_INTERFACE, ACTIVE_STATE_PROPERTY),
		)
		.await
		.map_err(|e| format!("reading ActiveState failed: {e}"))?;
	let (state,): (zvariant::OwnedValue,) = reply
		.body()
		.deserialize()
		.map_err(|e| format!("unexpected ActiveState reply: {e}"))?;
	String::try_from(state)
		.map(Activity::State)
		.map_err(|e| format!("unexpected ActiveState type: {e}"))
}

/// Delays before each attempt of [`query_activity`].
const ACTIVITY_RETRY_DELAYS: [Duration; 3] = [Duration::ZERO, Duration::from_millis(200), Duration::from_secs(1)];
/// Bound of one [`unit_activity`] attempt.
const ACTIVITY_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// [`unit_activity`], retried briefly so a transient bus failure is not
/// mistaken for an answer. Every attempt is bounded; dropping the future
/// cancels it.
async fn query_activity(conn: &Connection, unit_name: &str) -> Result<Activity, String> {
	let mut error = String::new();
	for delay in ACTIVITY_RETRY_DELAYS {
		tokio::time::sleep(delay).await;
		match tokio::time::timeout(ACTIVITY_QUERY_TIMEOUT, unit_activity(conn, unit_name)).await {
			Ok(Ok(activity)) => return Ok(activity),
			Ok(Err(e)) => error = e,
			Err(_) => error = format!("no answer within {ACTIVITY_QUERY_TIMEOUT:?}"),
		}
	}
	Err(error)
}

/// Whether an answer establishes that the unit has stopped: `Some(true)` for
/// a terminal or unloaded unit, `Some(false)` while it runs, `None` when
/// systemd gave no answer.
fn exit_confirmed(activity: &Result<Activity, String>) -> Option<bool> {
	match activity {
		Ok(Activity::Absent) => Some(true),
		Ok(Activity::State(state)) => Some(terminal_state(state).is_some()),
		Err(_) => None,
	}
}

fn terminal_state(state: &str) -> Option<&'static str> {
	match state {
		"inactive" => Some("inactive"),
		"failed" => Some("failed"),
		_ => None,
	}
}

/// Split a systemd `StandardOutput`/`StandardError` setting into the enum value
/// and, for path-based settings, the companion path property.
///
/// Unit files accept `StandardOutput=file:/path`, but over D-Bus
/// (StartTransientUnit) the path must go in a separate property:
/// `StandardOutput=file` + `StandardOutputFile=/path`. Sending the combined
/// `file:/path` string is rejected with "Invalid StandardOutput setting".
/// Mirrors systemd's `bus_append_standard_inputs()` in bus-unit-util.c.
fn split_standard_io(value: &Option<String>) -> (String, Option<(&'static str, String)>) {
	let Some(v) = value.as_deref() else {
		return ("null".to_string(), None);
	};
	for (prefix, prop) in [
		("file:", "File"),
		("append:", "FileToAppend"),
		("truncate:", "FileToTruncate"),
		("fd:", "FileDescriptorName"),
	] {
		if let Some(path) = v.strip_prefix(prefix) {
			return (v[..prefix.len() - 1].to_string(), Some((prop, path.to_string())));
		}
	}
	(v.to_string(), None)
}

/// Launch the application as a transient systemd service unit via D-Bus.
///
/// Returns the unit's object path and a watch subscribed before the unit
/// started, so the exit monitor sees how the application ends.
async fn start_transient_service(conn: &Connection, options: &LaunchOptions<'_>) -> Result<UnitWatch, SessionFailure> {
	let internal = |message: &str| SessionFailure::see_log("application_start_failed", message.to_string());
	// Resolve exec entries in a blocking task — `which::which` does filesystem lookups.
	let (pre_entries, main_entry, post_entries) = tokio::task::spawn_blocking({
		let pre_commands = options.pre_commands.clone();
		let post_commands = options.post_commands.clone();
		let main_program = options.program.to_string();
		let args = options.args.to_vec();
		move || -> Result<_, SessionFailure> {
			let program_for_error = main_program.clone();
			// Every configured hook is installed in order, or the launch fails;
			// systemd can only enforce hooks it is given.
			let hooks = |stage, commands| {
				build_exec_entries(stage, commands).map_err(|error| {
					tracing::error!(%error, "Application hook cannot be installed; not launching");
					SessionFailure::new("application_hook_invalid", format!("Application hook {error}."))
						.with_hint("Fix the application's pre_command/post_command in the Pyroshine configuration.")
				})
			};
			Ok((
				hooks("pre_command", &pre_commands)?,
				build_exec_entry(main_program, args.clone()).ok_or_else(move || {
					tracing::error!("Main program '{}' not found in PATH.", program_for_error);
					SessionFailure::new(
						"application_not_found",
						format!("The application command '{program_for_error}' was not found."),
					)
					.with_hint(
						"Install it, or use an absolute path in the application's command \
						 (commands are looked up in the PATH of the Pyroshine service).",
					)
				})?,
				hooks("post_command", &post_commands)?,
			))
		}
	})
	.await
	.map_err(|e| {
		tracing::error!("spawn_blocking panicked: {e}");
		internal("Resolving the application command failed unexpectedly.")
	})??;

	tracing::debug!(?pre_entries, ?main_entry, ?post_entries, "Building transient service");

	// Split StandardOutput/StandardError into the enum value plus an optional
	// companion path property. Unit-file syntax (`StandardOutput=file:/path`) is
	// not accepted over D-Bus; the path must be a separate property
	// (`StandardOutput=file` + `StandardOutputFile=/path`).
	let (stdout_setting, stdout_path) = split_standard_io(options.stdout_value);
	let (stderr_setting, stderr_path) = split_standard_io(options.stderr_value);

	let exec_array = |entries: &[(String, Vec<String>, bool)]| {
		build_exec_array(entries).map_err(|()| internal("The application command could not be encoded for systemd."))
	};

	// Properties: a(sv) — array of (property_name: s, value: v)
	// zvariant::Value has D-Bus type 'v' (variant), so Vec<(String, Value)> serialises as a(sv).
	//
	// IMPORTANT: do NOT use zvariant::Array::from(Vec<Value>) — it always produces `av`
	// (array of variant). Build typed arrays with Array::new(signature) + append() instead.
	let mut properties: Vec<(String, zvariant::Value<'_>)> = vec![
		("Type".to_string(), zvariant::Value::Str("exec".into())),
		("Slice".to_string(), zvariant::Value::Str("moonshine.slice".into())),
		// Environment: as
		("Environment".to_string(), zvariant::Value::from(options.envs.to_vec())),
		// A lingering user manager can retain activation from an older install.
		// Unset it at the final systemd environment merge, including ExecStartPre.
		(
			"UnsetEnvironment".to_string(),
			zvariant::Value::from(vec![
				"ENABLE_MOONSHINE_WSI",
				"MOONSHINE_WAYLAND_DISPLAY",
				"MOONSHINE_HDR",
				"MOONSHINE_LIMITER_FILE",
			]),
		),
		// ExecStart: a(sasb)
		("ExecStart".to_string(), exec_array(&[main_entry])?),
		(
			"TimeoutStopUSec".to_string(),
			zvariant::Value::U64(UNIT_STOP_TIMEOUT.as_micros() as u64),
		),
		(
			"CollectMode".to_string(),
			zvariant::Value::Str("inactive-or-failed".into()),
		),
		// StandardOutput/StandardError: bare enum value
		(
			"StandardOutput".to_string(),
			zvariant::Value::Str(stdout_setting.into()),
		),
		("StandardError".to_string(), zvariant::Value::Str(stderr_setting.into())),
	];

	// Path-based outputs carry their path in a companion property.
	if let Some((prop, path)) = stdout_path {
		properties.push((format!("StandardOutput{prop}"), zvariant::Value::Str(path.into())));
	}
	if let Some((prop, path)) = stderr_path {
		properties.push((format!("StandardError{prop}"), zvariant::Value::Str(path.into())));
	}

	// Only include ExecStartPre/ExecStopPost when non-empty: an empty a(sasb) array still
	// needs a valid element signature, and omitting absent properties is cleaner.
	if !pre_entries.is_empty() {
		properties.push(("ExecStartPre".to_string(), exec_array(&pre_entries)?));
	}
	if !post_entries.is_empty() {
		properties.push(("ExecStopPost".to_string(), exec_array(&post_entries)?));
	}

	// Aux units: empty a(sa(sv))
	let aux: Vec<(&str, Vec<(&str, zvariant::Value)>)> = Vec::new();

	let output = output_hint(options.unit_name, options.stdout_value, options.stderr_value);
	let exited = |exit: &ExitStatus, stage: &str| {
		SessionFailure::new(
			"application_exited",
			format!("{} {} {stage}.", options.title, exit.describe()),
		)
		.with_hint(exit_hint(exit, &output))
	};

	// Follow the unit before it exists: an application that exits at once
	// is unloaded immediately, together with its exit status.
	let expected_path = OwnedObjectPath::try_from(unit_object_path(options.unit_name))
		.map_err(|e| internal(&format!("The unit name cannot be followed: {e}")))?;
	let mut watch = UnitWatch::new(conn, options.unit_name, expected_path)
		.await
		.map_err(|e| {
			tracing::error!("Failed to follow the application unit: {e}");
			internal("Pyroshine could not follow the application unit.")
		})?;

	// Subscribe to JobRemoved before calling StartTransientUnit to avoid a race condition.
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| {
			tracing::error!("Failed to create systemd proxy: {e}");
			internal("Pyroshine could not talk to systemd.")
		})?;
	let mut job_stream = proxy.receive_signal("JobRemoved").await.map_err(|e| {
		tracing::error!("Failed to subscribe to JobRemoved signals: {e}");
		internal("Pyroshine could not talk to systemd.")
	})?;

	// Call StartTransientUnit.
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"StartTransientUnit",
			&(options.unit_name, "replace", &properties, &aux),
		)
		.await
		.map_err(|e| {
			tracing::warn!("Failed to start transient service: {e}");
			SessionFailure::new(
				"application_start_rejected",
				format!("systemd refused to start {}: {e}", options.title),
			)
			.with_hint("Check the application's stdout/stderr settings and command in the Pyroshine configuration.")
		})?;

	let (job_path,): (OwnedObjectPath,) = reply.body().deserialize().map_err(|e| {
		tracing::warn!("Failed to deserialize StartTransientUnit reply: {e}");
		internal("systemd sent an unexpected reply.")
	})?;

	// Wait for the launch job to complete.
	if let Err(result) = wait_for_job_signal(&mut job_stream, &job_path, START_JOB_TIMEOUT, "Application launch").await
	{
		watch.complete_exit_status(conn).await;
		let failure = if !options.pre_commands.is_empty() {
			SessionFailure::new(
				"application_start_failed",
				format!(
					"{} could not start (systemd start job result: {result}). One of its pre_command hooks may have failed.",
					options.title
				),
			)
			.with_hint(output.clone())
		} else if watch.exit.code.is_some() {
			exited(&watch.exit, "while starting")
		} else {
			SessionFailure::new(
				"application_start_failed",
				format!(
					"{} could not start (systemd start job result: {result}).",
					options.title
				),
			)
			.with_hint(output.clone())
		};
		return Err(failure);
	}

	// Get the unit object path — now that the job is done, the unit should exist.
	let unit_path = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnit",
			&options.unit_name,
		)
		.await;
	let path: OwnedObjectPath = match unit_path {
		Ok(reply) => {
			reply
				.body()
				.deserialize::<(OwnedObjectPath,)>()
				.map_err(|e| {
					tracing::error!("Failed to deserialize unit object path: {e}");
					internal("systemd sent an unexpected reply.")
				})?
				.0
		},
		Err(error) if no_such_unit(&error) => {
			// Already exited and unloaded.
			watch.complete_exit_status(conn).await;
			tracing::warn!(exit = ?watch.exit, "Application exited immediately after launch.");
			return Err(exited(&watch.exit, "immediately after launch"));
		},
		Err(e) => {
			tracing::error!("Failed to get unit object path: {e}");
			return Err(internal("systemd did not report the application unit."));
		},
	};
	if path != watch.path {
		// systemd's path encoding is stable; follow the reported path anyway.
		tracing::warn!(expected = %watch.path, reported = %path, "Unexpected application unit path");
		watch = UnitWatch::new(conn, options.unit_name, path.clone())
			.await
			.map_err(|e| {
				tracing::error!("Failed to follow the application unit: {e}");
				internal("Pyroshine could not follow the application unit.")
			})?;
	}

	// Check current state — catches apps that exit immediately.
	let activity = query_activity(conn, options.unit_name).await;
	match exit_confirmed(&activity) {
		Some(true) => {
			watch.complete_exit_status(conn).await;
			tracing::warn!(?activity, exit = ?watch.exit, "Application exited immediately after launch.");
			return Err(exited(&watch.exit, "immediately after launch"));
		},
		Some(false) => {},
		None => {
			tracing::error!(?activity, "Failed to read the application unit's state");
			return Err(internal("Pyroshine could not read the application unit's state."));
		},
	}

	// Wait for ActiveState to change to a terminal failure state.
	let early_exit = tokio::time::timeout(options.timeout, watch.next_terminal_state(conn)).await;

	if let Ok(Some(state)) = early_exit {
		watch.complete_exit_status(conn).await;
		tracing::warn!(state, exit = ?watch.exit, "Application exited shortly after launch.");
		return Err(exited(
			&watch.exit,
			&format!(
				"within {} of launch",
				crate::session::failure::human_duration(options.timeout)
			),
		));
	}
	// The unit is still alive after the launch timeout (or its signals ended,
	// which the exit monitor reports): the launch succeeded.
	tracing::info!("Application launched in service {}", options.unit_name);
	Ok(watch)
}

/// Resolve configured `stage` (`pre_command`/`post_command`) hooks into exec
/// entries `(absolute_path, argv, ignore_errors=false)`, in order.
///
/// Fails on the first entry that is empty or whose executable cannot be
/// resolved, naming the stage, its index and the executable. Arguments are not
/// included in the error: users may keep secrets in them.
fn build_exec_entries(stage: &str, commands: &[Vec<String>]) -> Result<Vec<(String, Vec<String>, bool)>, String> {
	commands
		.iter()
		.enumerate()
		.map(|(index, cmd)| {
			let first = cmd
				.first()
				.filter(|program| !program.trim().is_empty())
				.ok_or_else(|| format!("{stage}[{index}] is empty"))?;
			let abs = which::which(first)
				.map_err(|error| format!("{stage}[{index}]: executable '{first}' cannot be resolved: {error}"))?;
			let abs_str = abs
				.to_str()
				.ok_or_else(|| format!("{stage}[{index}]: resolved path {} is not UTF-8", abs.display()))?
				.to_string();
			let argv: Vec<String> = std::iter::once(abs_str.clone())
				.chain(cmd[1..].iter().cloned())
				.collect();
			Ok((abs_str, argv, false))
		})
		.collect()
}

/// Build a single exec command entry from a program path and args.
/// Returns (absolute_path, argv, ignore_errors=false), or None if the program is not found.
fn build_exec_entry(program: String, args: Vec<String>) -> Option<(String, Vec<String>, bool)> {
	let abs = which::which(&program).ok()?;
	let abs_str = abs.to_str()?.to_string();
	let argv: Vec<String> = std::iter::once(abs_str.clone()).chain(args.iter().cloned()).collect();
	Some((abs_str, argv, false))
}

/// Build a properly-typed `a(sasb)` array for systemd ExecStart/ExecStartPre/ExecStopPost.
///
/// Must use `Array::new(signature)` + `append()` rather than `Array::from(Vec<Value>)`,
/// which always produces `av` regardless of the element type.
fn build_exec_array(entries: &[(String, Vec<String>, bool)]) -> Result<zvariant::Value<'static>, ()> {
	let element_sig = zvariant::Signature::structure([
		zvariant::Signature::Str,
		zvariant::Signature::array(zvariant::Signature::Str),
		zvariant::Signature::Bool,
	]);
	let mut arr = zvariant::Array::new(&element_sig);
	for entry in entries {
		arr.append(zvariant::Value::from(entry.clone()))
			.map_err(|e| tracing::error!("Failed to append exec entry: {e}"))?;
	}
	Ok(zvariant::Value::Array(arr))
}

#[cfg(test)]
mod tests {
	/// Review 2026-10-05 CFG-002: every configured pre/post hook reaches the
	/// unit in order, or unit preparation fails naming the stage, index and
	/// executable. An empty or unresolvable entry is never silently dropped.
	/// An executable that exists regardless of `PATH` (another test replaces
	/// `PATH` temporarily).
	fn absolute_executable() -> String {
		std::env::current_exe().unwrap().to_str().unwrap().to_string()
	}

	#[test]
	fn unresolvable_hooks_fail_with_their_position() {
		let error = super::build_exec_entries(
			"pre_command",
			&[
				vec![absolute_executable()],
				vec![
					"pyroshine-review-missing-hook".to_string(),
					"--secret=hunter2".to_string(),
				],
				vec!["/bin/sh".to_string(), "-c".to_string(), "exit 0".to_string()],
			],
		)
		.unwrap_err();
		assert!(
			error.starts_with("pre_command[1]: executable 'pyroshine-review-missing-hook'"),
			"{error}"
		);
		assert!(!error.contains("hunter2"), "arguments are not logged: {error}");
		for empty in [Vec::new(), vec![String::new()], vec!["  ".to_string()]] {
			let error = super::build_exec_entries("post_command", &[vec![absolute_executable()], empty]).unwrap_err();
			assert_eq!(error, "post_command[1] is empty");
		}
		assert_eq!(super::build_exec_entries("post_command", &[]), Ok(Vec::new()));
	}

	/// Valid hooks keep their order and arguments (current behavior to retain).
	#[test]
	fn resolvable_hooks_keep_order_and_arguments() {
		let commands = vec![
			vec![absolute_executable()],
			vec!["/bin/sh".to_string(), "-c".to_string(), "exit 0".to_string()],
		];
		let entries = super::build_exec_entries("pre_command", &commands).unwrap();
		assert_eq!(entries.len(), 2);
		assert!(entries[0].0 == absolute_executable() && entries[0].1.len() == 1);
		assert_eq!(entries[1].0, "/bin/sh");
		assert_eq!(entries[1].1[1..], ["-c".to_string(), "exit 0".to_string()]);
		assert!(
			entries
				.iter()
				.all(|(path, argv, ignore_errors)| argv[0] == *path && !ignore_errors)
		);
	}

	/// Review 2026-10-05 STAB-003: the client-side wait for the stop job must
	/// cover the stop allowance the unit is created with; otherwise a process
	/// that exits within systemd's allowance (for example after three seconds)
	/// is reported as a failed stop and teardown proceeds while it still runs.
	#[test]
	fn stop_job_wait_covers_the_unit_stop_allowance() {
		assert!(
			super::STOP_JOB_TIMEOUT > super::UNIT_STOP_TIMEOUT,
			"review 2026-10-05 STAB-003: the stop job wait ({:?}) is shorter than the unit's TimeoutStopUSec ({:?})",
			super::STOP_JOB_TIMEOUT,
			super::UNIT_STOP_TIMEOUT
		);
		assert!(
			super::APPLICATION_STOP_DEADLINE >= super::STOP_JOB_TIMEOUT + super::UNIT_SETTLE_TIMEOUT,
			"the overall stop deadline covers the job wait and the state check"
		);
	}

	/// Review 2026-10-05 STAB-003 against the real user systemd: each stop is
	/// reported complete only once the unit's processes are gone, within the
	/// stop deadline, including an exit that takes longer than the old 2 s wait
	/// and a process that ignores SIGTERM until systemd kills it.
	/// `MOONSHINE_TEST_SYSTEMD=1 cargo test -p moonshine-core transient_unit_stop -- --ignored --nocapture`
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	#[ignore = "needs a user systemd session: MOONSHINE_TEST_SYSTEMD=1"]
	async fn transient_unit_stop_establishes_termination() {
		use super::*;
		assert!(
			std::env::var_os("MOONSHINE_TEST_SYSTEMD").is_some(),
			"set MOONSHINE_TEST_SYSTEMD=1"
		);
		let conn = Connection::session().await.unwrap();
		subscribe_to_systemd_signals(&conn).await.unwrap();
		let secs = Duration::from_secs;
		// (name, shell script, post hooks, minimum and maximum stop time)
		type StopCase = (&'static str, &'static str, Vec<Vec<String>>, Duration, Duration);
		let cases: [StopCase; 5] = [
			("immediate", "exec sleep 600", Vec::new(), Duration::ZERO, secs(2)),
			(
				"slow-exit",
				"trap 'sleep 3; exit 0' TERM; while :; do sleep 0.1; done",
				Vec::new(),
				secs(3),
				secs(5),
			),
			(
				"ignores-term",
				"trap '' TERM; while :; do sleep 0.1; done",
				Vec::new(),
				UNIT_STOP_TIMEOUT,
				UNIT_STOP_TIMEOUT + secs(3),
			),
			(
				"descendants",
				"setsid sh -c \"trap '' TERM; while :; do sleep 0.1; done\" & wait",
				Vec::new(),
				Duration::ZERO,
				UNIT_STOP_TIMEOUT + secs(3),
			),
			(
				"failing-post-hook",
				"exec sleep 600",
				vec![vec!["false".to_string()]],
				Duration::ZERO,
				secs(3),
			),
		];
		for (name, script, post, min, max) in cases {
			let unit = format!("pyroshine-test-{}-{name}.service", std::process::id());
			let args = vec!["-c".to_string(), script.to_string()];
			let options = LaunchOptions {
				title: name,
				unit_name: &unit,
				program: "sh",
				args: &args,
				envs: &[],
				timeout: Duration::from_millis(500),
				pre_commands: &Vec::new(),
				post_commands: &post,
				stdout_value: &None,
				stderr_value: &None,
			};
			start_transient_service(&conn, &options).await.unwrap();
			let started = std::time::Instant::now();
			let stopped = stop_application_unit(&unit).await;
			let elapsed = started.elapsed();
			eprintln!("{name}: {stopped:?} after {elapsed:?}");
			assert!(stopped.is_ok(), "{name}: termination was not established");
			assert!(elapsed >= min && elapsed <= max, "{name}: took {elapsed:?}");
			assert!(elapsed <= APPLICATION_STOP_DEADLINE, "{name}");
			let state = unit_state(&conn, &unit).await.unwrap();
			assert!(state.reached(Settled::Terminated), "{name}: {state:?}");
			// The next launch's leftover cleanup waits until the unit is unloaded.
			stop_unit(&conn, &unit, Settled::Unloaded).await.unwrap();
			assert_eq!(unit_state(&conn, &unit).await.unwrap(), UnitState::Absent, "{name}");
		}
		// An absent unit is already stopped.
		assert_eq!(stop_application_unit("pyroshine-test-absent.service").await, Ok(()));
	}

	/// Launch failures and exits name the exit status systemd reported, even
	/// though the unit is unloaded as soon as it exits.
	/// `MOONSHINE_TEST_SYSTEMD=1 cargo test -p moonshine-core exit_status -- --ignored --nocapture`
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	#[ignore = "needs a user systemd session: MOONSHINE_TEST_SYSTEMD=1"]
	async fn launch_failures_and_exits_report_the_exit_status() {
		use super::*;
		assert!(
			std::env::var_os("MOONSHINE_TEST_SYSTEMD").is_some(),
			"set MOONSHINE_TEST_SYSTEMD=1"
		);
		let conn = Connection::session().await.unwrap();
		subscribe_to_systemd_signals(&conn).await.unwrap();
		let cases = [
			("exit-3", "exit 3", "exited with status 3 "),
			("immediate-success", "exit 0", "exited normally "),
		];
		for (name, script, expected) in cases {
			let unit = format!("pyroshine-test-{}-{name}.service", std::process::id());
			let args = vec!["-c".to_string(), script.to_string()];
			let options = LaunchOptions {
				title: "Test game",
				unit_name: &unit,
				program: "sh",
				args: &args,
				envs: &[],
				timeout: Duration::from_millis(1500),
				pre_commands: &Vec::new(),
				post_commands: &Vec::new(),
				stdout_value: &None,
				stderr_value: &None,
			};
			let failure = start_transient_service(&conn, &options).await.err().expect(name);
			eprintln!("{name}: {failure:?}");
			assert_eq!(failure.code, "application_exited", "{name}");
			assert!(failure.message.starts_with("Test game "), "{name}: {}", failure.message);
			assert!(failure.message.contains(expected), "{name}: {}", failure.message);
			let hint = failure.hint.as_deref().unwrap();
			assert!(hint.contains("stdout = \"journal\""), "{name}");
			assert_eq!(
				hint.contains("hands off"),
				name == "immediate-success",
				"{name}: {hint}"
			);
			stop_unit(&conn, &unit, Settled::Unloaded).await.unwrap();
		}

		// A program that does not exist never reaches systemd.
		let options = LaunchOptions {
			title: "Missing",
			unit_name: "pyroshine-test-missing.service",
			program: "pyroshine-test-no-such-program",
			args: &[],
			envs: &[],
			timeout: Duration::from_millis(500),
			pre_commands: &Vec::new(),
			post_commands: &Vec::new(),
			stdout_value: &None,
			stderr_value: &None,
		};
		let failure = start_transient_service(&conn, &options).await.err().unwrap();
		assert_eq!(failure.code, "application_not_found");

		// An exit after the launch window reaches the exit monitor, which
		// records the cause before it stops the session.
		let unit = format!("pyroshine-test-{}-late-exit.service", std::process::id());
		let args = vec!["-c".to_string(), "sleep 1; exit 7".to_string()];
		let journal = Some("journal".to_string());
		let options = LaunchOptions {
			title: "Late game",
			unit_name: &unit,
			program: "sh",
			args: &args,
			envs: &[],
			timeout: Duration::from_millis(200),
			pre_commands: &Vec::new(),
			post_commands: &Vec::new(),
			stdout_value: &journal,
			stderr_value: &journal,
		};
		let watch = start_transient_service(&conn, &options).await.unwrap();
		let stop = ShutdownManager::new();
		let cause = StopCauseSlot::default();
		let report = ExitReport {
			title: "Late game".into(),
			unit_name: unit.clone(),
			output: output_hint(&unit, &journal, &journal),
			launched_at: Instant::now(),
		};
		let monitor = spawn_unit_exit_monitor(conn.clone(), report, watch, stop.clone(), cause.clone());
		let reason = tokio::time::timeout(Duration::from_secs(10), stop.wait_shutdown_triggered())
			.await
			.unwrap();
		assert_eq!(reason, SessionShutdownReason::ApplicationStopped);
		let cause = cause.get().expect("the exit monitor records a cause");
		eprintln!("late exit: {cause:?}");
		assert!(
			cause.detail.starts_with("Late game exited with status 7, "),
			"{}",
			cause.detail
		);
		assert!(cause.unexpected, "an early failing exit is unexpected");
		assert_eq!(
			cause.hint.as_deref(),
			Some(format!("Read its output with: journalctl --user -u {unit} -e").as_str())
		);
		monitor.await.unwrap();
		stop_unit(&conn, &unit, Settled::Unloaded).await.unwrap();

		// A crash after launch is unexpected and names the signal.
		let unit = format!("pyroshine-test-{}-crash.service", std::process::id());
		let args = vec!["60".to_string()];
		let options = LaunchOptions {
			title: "Crashing game",
			unit_name: &unit,
			program: "sleep",
			args: &args,
			envs: &[],
			timeout: Duration::from_millis(200),
			pre_commands: &Vec::new(),
			post_commands: &Vec::new(),
			stdout_value: &None,
			stderr_value: &None,
		};
		let watch = start_transient_service(&conn, &options).await.unwrap();
		let stop = ShutdownManager::new();
		let cause = StopCauseSlot::default();
		let report = ExitReport {
			title: "Crashing game".into(),
			unit_name: unit.clone(),
			output: output_hint(&unit, &None, &None),
			launched_at: Instant::now(),
		};
		let monitor = spawn_unit_exit_monitor(conn.clone(), report, watch, stop.clone(), cause.clone());
		conn.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"KillUnit",
			&(unit.as_str(), "main", libc::SIGSEGV),
		)
		.await
		.unwrap();
		tokio::time::timeout(Duration::from_secs(30), stop.wait_shutdown_triggered())
			.await
			.unwrap();
		let cause = cause.get().unwrap();
		eprintln!("crash: {cause:?}");
		assert!(cause.detail.contains("SIGSEGV"), "{}", cause.detail);
		assert!(cause.unexpected);
		monitor.await.unwrap();
		stop_unit(&conn, &unit, Settled::Unloaded).await.unwrap();
	}

	#[test]
	fn exit_status_descriptions() {
		use super::{CLD_DUMPED, CLD_EXITED, CLD_KILLED, ExitStatus};
		let status = |result: &str, code, status| ExitStatus {
			result: Some(result.into()),
			code: Some(code),
			status: Some(status),
		};
		let clean = status("success", CLD_EXITED, 0);
		assert_eq!(clean.describe(), "exited normally");
		assert!(!clean.failed() && !clean.crashed());
		let failed = status("exit-code", CLD_EXITED, 2);
		assert_eq!(failed.describe(), "exited with status 2");
		assert!(failed.failed() && !failed.crashed());
		let segv = status("core-dump", CLD_DUMPED, libc::SIGSEGV);
		assert_eq!(segv.describe(), "crashed (SIGSEGV, core dumped)");
		assert!(segv.crashed());
		let killed = status("signal", CLD_KILLED, libc::SIGKILL);
		assert_eq!(killed.describe(), "was killed by SIGKILL");
		assert!(killed.crashed(), "SIGKILL from outside Pyroshine's stop is a crash");
		assert!(!status("signal", CLD_KILLED, libc::SIGTERM).crashed());
		let oom = status("oom-kill", CLD_KILLED, libc::SIGKILL);
		assert_eq!(oom.describe(), "was killed because the system ran out of memory");
		assert_eq!(ExitStatus::default().describe(), "exited");
	}

	#[test]
	fn unit_object_paths_follow_systemd_encoding() {
		assert_eq!(
			super::unit_object_path("moonshine-session.service"),
			"/org/freedesktop/systemd1/unit/moonshine_2dsession_2eservice"
		);
		assert_eq!(
			super::unit_object_path("1app@x.service"),
			"/org/freedesktop/systemd1/unit/_31app_40x_2eservice"
		);
	}

	#[test]
	fn output_hints_follow_the_configured_destination() {
		let unit = "moonshine-session.service";
		assert!(super::output_hint(unit, &None, &None).starts_with("Its output is discarded"));
		assert_eq!(
			super::output_hint(unit, &None, &Some("journal".into())),
			"Read its output with: journalctl --user -u moonshine-session.service -e"
		);
		assert_eq!(
			super::output_hint(unit, &Some("append:/tmp/game.log".into()), &None),
			"Its output is in /tmp/game.log."
		);
	}

	#[test]
	fn unit_state_decides_termination() {
		use super::{Settled, UnitState};
		let loaded = |state: &str, processes| UnitState::Loaded {
			active_state: state.to_string(),
			processes,
		};
		assert!(UnitState::Absent.reached(Settled::Terminated));
		assert!(UnitState::Absent.reached(Settled::Unloaded));
		for state in ["inactive", "failed"] {
			assert!(loaded(state, 0).reached(Settled::Terminated), "{state}");
			assert!(!loaded(state, 0).reached(Settled::Unloaded), "{state}");
			assert!(
				!loaded(state, 1).reached(Settled::Terminated),
				"{state}: a process remains"
			);
		}
		for state in ["active", "deactivating", "activating", "reloading"] {
			assert!(!loaded(state, 0).reached(Settled::Terminated), "{state}");
		}
	}

	/// Only an answer from systemd confirms an application exit; a failed or
	/// undecodable query is no answer and must not end the session.
	#[test]
	fn only_systemd_answers_confirm_an_exit() {
		use super::{Activity, exit_confirmed};
		assert_eq!(exit_confirmed(&Ok(Activity::Absent)), Some(true));
		for state in ["inactive", "failed"] {
			assert_eq!(
				exit_confirmed(&Ok(Activity::State(state.into()))),
				Some(true),
				"{state}"
			);
		}
		for state in ["active", "activating", "deactivating", "reloading"] {
			assert_eq!(
				exit_confirmed(&Ok(Activity::State(state.into()))),
				Some(false),
				"{state}"
			);
		}
		for error in ["GetUnit failed: timeout", "unexpected ActiveState reply: signature"] {
			assert_eq!(exit_confirmed(&Err(error.into())), None, "{error}");
		}
	}

	#[test]
	fn launch_environment_exposes_real_displays_without_presentation_injection() {
		let context = super::ApplicationContext {
			unit_name: "test.service".into(),
			pulse_socket_path: "/tmp/audio/socket".into(),
			xdisplay: 5,
			wayland_display: "wayland-pyroshine-test".into(),
			hdr: true,
			extra_env: Default::default(),
		};
		let env = super::make_envs(&context).unwrap();
		assert!(env.contains(&"DISPLAY=:5".into()));
		assert!(env.contains(&"WAYLAND_DISPLAY=wayland-pyroshine-test".into()));
		assert!(env.iter().all(|e| !super::obsolete_presentation_environment(
			e.split('=').next().unwrap(),
			e.split_once('=').unwrap().1
		)));
		let mut obsolete = context;
		obsolete.extra_env.insert("ENABLE_MOONSHINE_WSI".into(), "1".into());
		assert!(super::make_envs(&obsolete).is_err());
		assert!(super::obsolete_presentation_environment(
			"MOONSHINE_WSI_DISABLE_BYPASS",
			"1"
		));
		assert!(super::obsolete_presentation_environment(
			"VK_INSTANCE_LAYERS",
			"VK_LAYER_MESA_device_select:VK_LAYER_MOONSHINE_wsi"
		));
	}

	use super::{ApplicationConfig, split_standard_io};

	#[test]
	fn application_output_scale_is_optional_and_deserializes() {
		let unscaled: ApplicationConfig = toml::from_str(
			r#"
title = "Example"
command = ["example"]
"#,
		)
		.unwrap();
		assert_eq!(unscaled.output_scale, None);

		let scaled: ApplicationConfig = toml::from_str(
			r#"
title = "Example"
command = ["example"]
output_scale = 1.5
"#,
		)
		.unwrap();
		assert_eq!(scaled.output_scale, Some(1.5));
	}

	#[test]
	fn test_standard_io_defaults_to_null() {
		assert_eq!(split_standard_io(&None), ("null".to_string(), None));
	}

	#[test]
	fn test_standard_io_passes_bare_enums_through() {
		assert_eq!(
			split_standard_io(&Some("journal".to_string())),
			("journal".to_string(), None)
		);
		assert_eq!(
			split_standard_io(&Some("inherit".to_string())),
			("inherit".to_string(), None)
		);
		assert_eq!(
			split_standard_io(&Some("kmsg+console".to_string())),
			("kmsg+console".to_string(), None)
		);
	}

	#[test]
	fn test_standard_io_splits_file_paths() {
		assert_eq!(
			split_standard_io(&Some("file:/var/log/app.log".to_string())),
			("file".to_string(), Some(("File", "/var/log/app.log".to_string())))
		);
	}

	#[test]
	fn test_standard_io_splits_append_and_truncate() {
		assert_eq!(
			split_standard_io(&Some("append:/var/log/app.log".to_string())),
			(
				"append".to_string(),
				Some(("FileToAppend", "/var/log/app.log".to_string()))
			)
		);
		assert_eq!(
			split_standard_io(&Some("truncate:/var/log/app.log".to_string())),
			(
				"truncate".to_string(),
				Some(("FileToTruncate", "/var/log/app.log".to_string()))
			)
		);
	}

	#[test]
	fn test_standard_io_splits_fd_names() {
		assert_eq!(
			split_standard_io(&Some("fd:stdout".to_string())),
			("fd".to_string(), Some(("FileDescriptorName", "stdout".to_string())))
		);
	}
}

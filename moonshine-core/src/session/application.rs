use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

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

use crate::session::manager::SessionShutdownReason;

const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const PROPERTIES_CHANGED: &str = "PropertiesChanged";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
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
	pub async fn spawn(
		config: ApplicationConfig,
		context: ApplicationContext,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<Self, ()> {
		let Some(program) = config.command.first() else {
			tracing::error!("Application command is empty.");
			return Err(());
		};
		let args = &config.command[1..];
		let envs = make_envs(&context)?;

		tracing::info!(program, ?args, "Launching application.");

		// Connect to the user session bus.
		let conn = Connection::session()
			.await
			.map_err(|e| tracing::error!("Failed to connect to session bus: {e}"))?;
		subscribe_to_systemd_signals(&conn).await?;

		// Stop any leftover unit from a previous session. A unit whose processes
		// cannot be shown gone, or that is still loaded, cannot be replaced
		// safely: refuse the launch rather than overlap it.
		stop_unit(&conn, &context.unit_name, Settled::Unloaded)
			.await
			.map_err(|()| {
				tracing::error!(
					unit = context.unit_name,
					"A previous application unit is still present; refusing to launch over it"
				)
			})?;

		// Launch the application as a transient systemd service unit.
		let options = LaunchOptions {
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

		let unit_path = match start_transient_service(&conn, &options).await {
			Ok(unit_path) => unit_path,
			Err(_) => {
				// Best effort cleanup on launch failure; the session's teardown
				// stops (and verifies) the unit again.
				stop_unit(&conn, &context.unit_name, Settled::Terminated).await.ok();
				return Err(());
			},
		};
		let exit_monitor = spawn_unit_exit_monitor(conn.clone(), context.unit_name.clone(), unit_path, stop);

		Ok(Self {
			config,
			exit_monitor: Some(exit_monitor),
		})
	}
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
fn make_envs(context: &ApplicationContext) -> Result<Vec<String>, ()> {
	// Build environment variables as "KEY=value" strings for systemd.
	let mut envs: Vec<String> = vec![
		format!("PULSE_SERVER=unix:{}", context.pulse_socket_path.display()),
		format!(
			"PULSE_RUNTIME_PATH={}",
			context
				.pulse_socket_path
				.parent()
				.ok_or_else(|| tracing::error!("Failed to get parent directory of PulseAudio socket."))?
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
			return Err(());
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

/// Wait for a `JobRemoved` signal matching the given job path, accepting only `"done"` as success.
async fn wait_for_job_signal(
	job_stream: &mut SignalStream<'_>,
	job_path: &OwnedObjectPath,
	timeout: Duration,
	label: &str,
) -> Result<(), ()> {
	let result = tokio::time::timeout(timeout, async {
		while let Some(message) = job_stream.next().await {
			let body: (u32, OwnedObjectPath, String, String) = message
				.body()
				.deserialize()
				.map_err(|e| tracing::error!("Failed to deserialize JobRemoved signal: {e}"))?;
			let (_, ref path, ref unit, ref result) = body;

			if path != job_path {
				continue;
			}

			return match result.as_str() {
				"done" => Ok(()),
				other => {
					tracing::warn!(result = other, unit = unit, "{label} job finished unsuccessfully.");
					Err(())
				},
			};
		}
		Err(())
	})
	.await;

	match result {
		Ok(Ok(())) => Ok(()),
		Ok(Err(())) => {
			tracing::warn!(label, "Received failure result for {label} job.");
			Err(())
		},
		Err(_) => {
			tracing::warn!(timeout_secs = timeout.as_secs(), "Timed out waiting for {label} job.");
			Err(())
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

fn spawn_unit_exit_monitor(
	conn: Connection,
	unit_name: String,
	unit_path: OwnedObjectPath,
	stop: ShutdownManager<SessionShutdownReason>,
) -> JoinHandle<()> {
	tokio::spawn(async move {
		tokio::select! {
			state = wait_for_unit_terminal_state(&conn, &unit_name, &unit_path) => {
				match state {
					Ok(state) => {
						tracing::info!(unit = unit_name, state, "Application unit exited; stopping session.");
						let _ = stop.trigger_shutdown(SessionShutdownReason::ApplicationStopped);
					},
					Err(()) => {
						tracing::warn!(unit = unit_name, "Application unit monitor stopped unexpectedly.");
					},
				}
			},
			_ = stop.wait_shutdown_triggered() => {},
		}
	})
}

async fn wait_for_unit_terminal_state(
	conn: &Connection,
	unit_name: &str,
	unit_path: &OwnedObjectPath,
) -> Result<String, ()> {
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut unit_removed_stream = proxy
		.receive_signal("UnitRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to UnitRemoved signals: {e}"))?;

	let rule = MatchRule::builder()
		.msg_type(zbus::message::Type::Signal)
		.sender(SYSTEMD_BUS)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.path(unit_path)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.interface(PROPERTIES_INTERFACE)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.member(PROPERTIES_CHANGED)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.build();
	let mut state_stream = MessageStream::for_match_rule(rule, conn, None)
		.await
		.map_err(|e| tracing::error!("Failed to create state stream: {e}"))?;

	match current_unit_state(conn, unit_path).await {
		Ok(state) => {
			if let Some(state) = terminal_state(state.as_str()) {
				return Ok(state.to_string());
			}
		},
		Err(()) => return Ok("removed".to_string()),
	}

	loop {
		tokio::select! {
			message = state_stream.next() => {
				let Some(Ok(signal)) = message else {
					return Err(());
				};
				let body = signal.body();
				let (iface, changed, _): (String, HashMap<String, zvariant::Value<'_>>, Vec<String>) =
					body.deserialize()
						.map_err(|e| tracing::error!("Failed to deserialize PropertiesChanged signal: {e}"))?;
				if iface != UNIT_INTERFACE {
					continue;
				}
				if let Some(zvariant::Value::Str(state)) = changed.get(ACTIVE_STATE_PROPERTY)
					&& let Some(state) = terminal_state(state.as_str()) {
						return Ok(state.to_string());
					}
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

async fn current_unit_state(conn: &Connection, unit_path: &OwnedObjectPath) -> Result<String, ()> {
	let reply = conn
		.call_method(
			Some(SYSTEMD_BUS),
			unit_path,
			Some(PROPERTIES_INTERFACE),
			"Get",
			&(UNIT_INTERFACE, ACTIVE_STATE_PROPERTY),
		)
		.await
		.map_err(|e| tracing::error!("Failed to get unit state: {e}"))?;

	let body = reply.body();
	let (variant,): (zvariant::Value<'_>,) = body.deserialize().map_err(|e| {
		tracing::error!("Failed to deserialize unit state: {e}");
	})?;
	match variant {
		zvariant::Value::Str(state) => Ok(state.to_string()),
		_ => Ok("unknown".to_string()),
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
async fn start_transient_service(conn: &Connection, options: &LaunchOptions<'_>) -> Result<OwnedObjectPath, ()> {
	// Resolve exec entries in a blocking task — `which::which` does filesystem lookups.
	let (pre_entries, main_entry, post_entries) = tokio::task::spawn_blocking({
		let pre_commands = options.pre_commands.clone();
		let post_commands = options.post_commands.clone();
		let main_program = options.program.to_string();
		let args = options.args.to_vec();
		move || -> Result<_, ()> {
			let program_for_error = main_program.clone();
			// Every configured hook is installed in order, or the launch fails;
			// systemd can only enforce hooks it is given.
			let hooks = |stage, commands| {
				build_exec_entries(stage, commands)
					.map_err(|error| tracing::error!(%error, "Application hook cannot be installed; not launching"))
			};
			Ok((
				hooks("pre_command", &pre_commands)?,
				build_exec_entry(main_program, args.clone()).ok_or_else(move || {
					tracing::error!("Main program '{}' not found in PATH.", program_for_error);
				})?,
				hooks("post_command", &post_commands)?,
			))
		}
	})
	.await
	.map_err(|e| tracing::error!("spawn_blocking panicked: {e}"))??;

	tracing::debug!(?pre_entries, ?main_entry, ?post_entries, "Building transient service");

	// Split StandardOutput/StandardError into the enum value plus an optional
	// companion path property. Unit-file syntax (`StandardOutput=file:/path`) is
	// not accepted over D-Bus; the path must be a separate property
	// (`StandardOutput=file` + `StandardOutputFile=/path`).
	let (stdout_setting, stdout_path) = split_standard_io(options.stdout_value);
	let (stderr_setting, stderr_path) = split_standard_io(options.stderr_value);

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
		("ExecStart".to_string(), build_exec_array(&[main_entry])?),
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
		properties.push(("ExecStartPre".to_string(), build_exec_array(&pre_entries)?));
	}
	if !post_entries.is_empty() {
		properties.push(("ExecStopPost".to_string(), build_exec_array(&post_entries)?));
	}

	// Aux units: empty a(sa(sv))
	let aux: Vec<(&str, Vec<(&str, zvariant::Value)>)> = Vec::new();

	// Subscribe to JobRemoved before calling StartTransientUnit to avoid a race condition.
	let proxy = Proxy::new(conn, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
		.await
		.map_err(|e| tracing::error!("Failed to create systemd proxy: {e}"))?;
	let mut job_stream = proxy
		.receive_signal("JobRemoved")
		.await
		.map_err(|e| tracing::error!("Failed to subscribe to JobRemoved signals: {e}"))?;

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
		.map_err(|e| tracing::warn!("Failed to start transient service: {e}"))?;

	let (job_path,): (OwnedObjectPath,) = reply
		.body()
		.deserialize()
		.map_err(|e| tracing::warn!("Failed to deserialize StartTransientUnit reply: {e}"))?;

	// Wait for the launch job to complete.
	wait_for_job_signal(&mut job_stream, &job_path, START_JOB_TIMEOUT, "Application launch").await?;

	// Get the unit object path — now that the job is done, the unit should exist.
	let unit_path = conn
		.call_method(
			Some(SYSTEMD_BUS),
			SYSTEMD_PATH,
			Some(SYSTEMD_MANAGER),
			"GetUnit",
			&options.unit_name,
		)
		.await
		.map_err(|e| tracing::error!("Failed to get unit object path: {e}"))?;
	let (path,): (OwnedObjectPath,) = unit_path
		.body()
		.deserialize()
		.map_err(|e| tracing::error!("Failed to deserialize unit object path: {e}"))?;

	// Check current state before subscribing — catches apps that exit immediately.
	let state = current_unit_state(conn, &path).await?;
	if terminal_state(&state).is_some() {
		tracing::warn!(state = state, "Application exited immediately after launch.");
		return Err(());
	}

	// Subscribe to PropertiesChanged on this unit object.
	let rule = MatchRule::builder()
		.msg_type(zbus::message::Type::Signal)
		.sender(SYSTEMD_BUS)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.path(&path)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.interface(PROPERTIES_INTERFACE)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.member(PROPERTIES_CHANGED)
		.map_err(|e| tracing::error!("Failed to create match rule: {e}"))?
		.build();
	let mut state_stream = MessageStream::for_match_rule(rule, conn, None)
		.await
		.map_err(|e| tracing::error!("Failed to create state stream: {e}"))?;

	// Wait for ActiveState to change to a terminal failure state.
	let failure_result = tokio::time::timeout(options.timeout, async {
		while let Some(Ok(signal)) = state_stream.next().await {
			let body = signal.body();
			let (iface, changed, _): (String, HashMap<String, zvariant::Value<'_>>, Vec<String>) =
				body.deserialize()
					.map_err(|e| tracing::error!("Failed to deserialize PropertiesChanged signal: {e}"))?;
			if iface != UNIT_INTERFACE {
				continue;
			}
			if let Some(zvariant::Value::Str(state)) = changed.get(ACTIVE_STATE_PROPERTY) {
				return match state.to_string().as_str() {
					"failed" | "inactive" => {
						tracing::warn!(state = state.to_string(), "Application exited shortly after launch.");
						Err(())
					},
					_ => continue, // e.g. "activating" → "active" — keep waiting
				};
			}
		}
		Ok(())
	})
	.await;

	match failure_result {
		Ok(Ok(())) => {
			// Timeout expired — unit is still alive, launch succeeded.
			tracing::info!("Application launched in service {}", options.unit_name);
			Ok(path)
		},
		Ok(Err(())) => Err(()),
		Err(_) => {
			// Timeout expired — unit is still alive, launch succeeded.
			tracing::info!("Application launched in service {}", options.unit_name);
			Ok(path)
		},
	}
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

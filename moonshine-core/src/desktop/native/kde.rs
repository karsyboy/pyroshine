//! Native KDE virtual outputs and atomic topology configuration.
use super::super::{Mode, Output, Result, Snapshot, Target};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_callback, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child};

#[allow(dead_code, unused_imports, clippy::all)]
mod devices {
	use wayland_client;
	use wayland_client::protocol::*;
	pub mod __interfaces {
		use wayland_client::backend as wayland_backend;
		use wayland_client::protocol::__interfaces::*;
		wayland_scanner::generate_interfaces!("protocols/kde-output-device-v2.xml");
	}
	use self::__interfaces::*;
	wayland_scanner::generate_client_code!("protocols/kde-output-device-v2.xml");
}
#[allow(dead_code, unused_imports, clippy::all)]
mod management {
	use super::devices::*;
	use wayland_client;
	pub mod __interfaces {
		use super::super::devices::__interfaces::*;
		use wayland_client::backend as wayland_backend;
		wayland_scanner::generate_interfaces!("protocols/kde-output-management-v2.xml");
	}
	use self::__interfaces::*;
	wayland_scanner::generate_client_code!("protocols/kde-output-management-v2.xml");
}
#[allow(dead_code, unused_imports, clippy::all)]
mod cast {
	use wayland_client;
	use wayland_client::protocol::*;
	pub mod __interfaces {
		use wayland_client::backend as wayland_backend;
		use wayland_client::protocol::__interfaces::*;
		wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
	}
	use self::__interfaces::*;
	wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}
use cast::{zkde_screencast_stream_unstable_v1 as cast_stream, zkde_screencast_unstable_v1 as cast_manager};
use devices::{
	kde_output_device_mode_v2 as mode, kde_output_device_registry_v2 as registry, kde_output_device_v2 as device,
};
use management::{
	kde_mode_list_v2 as modes, kde_output_configuration_v2 as config, kde_output_management_v2 as manager,
};

#[derive(Default)]
struct Device {
	proxy: Option<device::KdeOutputDeviceV2>,
	name: String,
	uuid: String,
	enabled: bool,
	x: i32,
	y: i32,
	scale: f64,
	transform: u32,
	priority: u32,
	hdr: u32,
	wcg: u32,
	replication: String,
	can_disable: bool,
	modes: Vec<ObjectId>,
	current: Option<ObjectId>,
}
#[derive(Default)]
struct Stream {
	proxy: Option<cast_stream::ZkdeScreencastStreamUnstableV1>,
	node: Option<u32>,
	serial: Option<u64>,
	failure: Option<String>,
}
#[derive(Default)]
struct State {
	registry: Option<registry::KdeOutputDeviceRegistryV2>,
	classic: bool,
	classic_offers: Vec<(u32, u32)>,
	classic_devices: HashMap<u32, ObjectId>,
	manager: Option<manager::KdeOutputManagementV2>,
	cast: Option<cast_manager::ZkdeScreencastUnstableV1>,
	devices: HashMap<ObjectId, Device>,
	modes: HashMap<ObjectId, (mode::KdeOutputDeviceModeV2, Mode)>,
	streams: HashMap<String, Stream>,
	sync: u64,
	applied: Option<Result<()>>,
	failure_reason: String,
}
pub(in crate::desktop) struct Kde {
	connection: Connection,
	queue: EventQueue<State>,
	state: State,
	sync: u64,
}
impl Kde {
	pub fn connect(socket: &Path) -> Result<Self> {
		let stream = std::os::unix::net::UnixStream::connect(socket).map_err(|e| e.to_string())?;
		let connection = Connection::from_socket(stream).map_err(|e| e.to_string())?;
		let queue = connection.new_event_queue();
		let registry = connection.display().get_registry(&queue.handle(), ());
		let mut this = Self {
			connection,
			queue,
			state: State::default(),
			sync: 0,
		};
		this.synchronize()?;
		if this.state.manager.is_none() || this.state.cast.is_none() {
			return Err("KWin lacks native virtual-output or custom-mode configuration support; update Plasma".into());
		}
		// Prefer registry discovery when both styles are advertised. Binding
		// both represents each physical output twice and makes restoration unsafe.
		if this.state.registry.is_none() {
			this.state.classic = true;
			for (name, version) in std::mem::take(&mut this.state.classic_offers) {
				this.state.bind_classic(&registry, name, version, &this.queue.handle());
			}
		}
		this.synchronize()?;
		if this.state.devices.is_empty() {
			return Err("KWin exposes no outputs with the required mode, priority and topology capabilities".into());
		}
		Ok(this)
	}
	pub fn pump(&mut self, wait: Duration) -> Result<()> {
		self.queue
			.dispatch_pending(&mut self.state)
			.map_err(|e| e.to_string())?;
		self.connection.flush().map_err(|e| e.to_string())?;
		if let Some(read) = self.queue.prepare_read() {
			let mut fd = libc::pollfd {
				fd: self.connection.as_fd().as_raw_fd(),
				events: libc::POLLIN,
				revents: 0,
			};
			// SAFETY: one initialized pollfd, borrowed descriptor outlives poll.
			let ready = unsafe { libc::poll(&mut fd, 1, wait.as_millis().min(i32::MAX as u128) as i32) };
			if ready < 0 {
				return Err(std::io::Error::last_os_error().to_string());
			}
			if fd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
				return Err("KWin connection closed".into());
			}
			if ready > 0 {
				read.read().map_err(|e| e.to_string())?;
			}
		}
		self.queue
			.dispatch_pending(&mut self.state)
			.map_err(|e| e.to_string())?;
		Ok(())
	}
	fn wait(&mut self, done: impl Fn(&State) -> bool) -> Result<()> {
		let deadline = Instant::now() + Duration::from_secs(5);
		while !done(&self.state) {
			if Instant::now() >= deadline {
				return Err("KWin did not acknowledge the desktop operation within 5 seconds".into());
			}
			self.pump(Duration::from_millis(20))?;
		}
		Ok(())
	}
	fn synchronize(&mut self) -> Result<()> {
		self.sync += 1;
		self.connection.display().sync(&self.queue.handle(), self.sync);
		let serial = self.sync;
		self.wait(|s| s.sync >= serial)
	}
	pub fn snapshot(&mut self) -> Result<Snapshot> {
		self.synchronize()?;
		let mut outputs = Vec::new();
		for device in self.state.devices.values() {
			let mode = device
				.current
				.as_ref()
				.and_then(|m| self.state.modes.get(m))
				.or_else(|| device.modes.iter().find_map(|m| self.state.modes.get(m)))
				.map(|m| m.1)
				.unwrap_or_default();
			if device.name.is_empty() {
				continue;
			}
			outputs.push(Output {
				key: device.uuid.clone(),
				name: device.name.clone(),
				enabled: device.enabled,
				can_disable: if device.proxy.as_ref().is_some_and(|p| p.version() >= 25) {
					device.can_disable
				} else {
					!device.name.starts_with("Virtual-")
				},
				mode,
				x: device.x,
				y: device.y,
				scale: device.scale,
				transform: device.transform,
				priority: device.priority,
				group: device.priority,
				hdr: device.hdr,
				wcg: device.wcg,
				replication: device.replication.clone(),
			});
		}
		outputs.sort_by(|a, b| a.key.cmp(&b.key));
		if outputs.iter().any(|o| o.key.is_empty()) {
			return Err("KWin returned an output without stable identity".into());
		}
		Ok(Snapshot {
			outputs,
			layout_mode: None,
		})
	}
	fn apply_config(&mut self, config: config::KdeOutputConfigurationV2) -> Result<()> {
		self.state.applied = None;
		self.state.failure_reason.clear();
		config.apply();
		let waited = self.wait(|s| s.applied.is_some());
		config.destroy();
		waited?;
		self.state
			.applied
			.take()
			.unwrap_or_else(|| Err("KWin configuration lost".into()))?;
		self.synchronize()
	}
	pub fn prepare(&mut self, id: &str, requested: Mode) -> Result<Target> {
		let name = format!("Pyroshine-{id}");
		let stream = self
			.state
			.cast
			.as_ref()
			.ok_or("KWin screencast unavailable")?
			.stream_virtual_output_with_description(
				name.clone(),
				"Pyroshine Desktop Mode".into(),
				requested.width as i32,
				requested.height as i32,
				1.0,
				cast_manager::Pointer::Embedded.into(),
				&self.queue.handle(),
				id.to_string(),
			);
		self.state.streams.insert(
			id.into(),
			Stream {
				proxy: Some(stream),
				..Default::default()
			},
		);
		self.wait(|s| {
			s.streams
				.get(id)
				.is_some_and(|s| s.node.is_some() || s.failure.is_some())
		})?;
		if let Some(error) = self.state.streams[id].failure.clone() {
			return Err(error);
		}
		self.wait(|s| s.devices.values().any(|d| d.name == format!("Virtual-{name}")))?;
		self.synchronize()?;
		let proxy = self
			.state
			.devices
			.values()
			.find(|d| d.name == format!("Virtual-{name}"))
			.and_then(|d| d.proxy.clone())
			.ok_or("KWin did not publish the virtual output")?;
		// Creating a virtual output initially installs 60 Hz. Add/select the
		// actual client mode before exposing this capture target to the daemon.
		let manager = self.state.manager.as_ref().ok_or("KWin output manager unavailable")?;
		let list = manager.create_mode_list(&self.queue.handle(), ());
		list.set_resolution(requested.width, requested.height);
		list.set_refresh_rate(requested.refresh_millihertz);
		list.set_reduced_blanking(1);
		list.add_mode();
		let configuration = manager.create_configuration(&self.queue.handle(), ());
		configuration.set_custom_modes(&proxy, &list);
		self.apply_config(configuration)?;
		list.destroy();
		// KWin generates the custom mode with CVT reduced blanking, whose
		// 0.25 MHz pixel-clock step moves the refresh rate (2560x1440 at
		// 116 Hz becomes 115.886 Hz). Select the mode KWin actually created.
		let generated = self
			.state
			.devices
			.values()
			.find(|d| d.name == format!("Virtual-{name}"))
			.map(|d| d.modes.iter().filter_map(|m| self.state.modes.get(m)).map(|m| m.1))
			.and_then(|modes| generated_mode(modes, requested))
			.ok_or_else(|| {
				format!(
					"KWin did not create a {}x{} mode near {} mHz for the virtual output",
					requested.width, requested.height, requested.refresh_millihertz
				)
			})?;
		let mut snapshot = self.snapshot()?;
		let output = snapshot
			.outputs
			.iter_mut()
			.find(|o| o.name == format!("Virtual-{name}"))
			.ok_or("Virtual output disappeared")?;
		output.mode = generated;
		let output_key = output.key.clone();
		self.apply(&snapshot)?;
		let observed = self
			.snapshot()?
			.outputs
			.into_iter()
			.find(|o| o.key == output_key)
			.ok_or("Virtual output disappeared after mode apply")?;
		if observed.mode.width != generated.width
			|| observed.mode.height != generated.height
			|| observed.mode.refresh_millihertz.abs_diff(generated.refresh_millihertz) > 100
		{
			return Err("KWin did not apply the requested output mode".into());
		}
		let stream = &self.state.streams[id];
		Ok(Target {
			id: id.into(),
			node_id: stream.node.ok_or("KWin supplied no PipeWire node")?,
			serial: stream.serial,
			observed_mode: Some(observed.mode),
			output: output_key,
			// The output's real mode: takeover, verification and the capture
			// format check all compare against it.
			mode: observed.mode,
		})
	}
	pub fn apply(&mut self, snapshot: &Snapshot) -> Result<()> {
		self.synchronize()?;
		// Resolve every identity and mode before constructing the mutation.
		let mut resolved = Vec::new();
		for output in &snapshot.outputs {
			let device = self
				.state
				.devices
				.values()
				.find(|d| d.uuid == output.key)
				.ok_or_else(|| format!("Output {} is disconnected", output.name))?;
			let selected = device
				.modes
				.iter()
				.filter_map(|id| self.state.modes.get(id))
				.find(|(_, mode)| {
					mode.width == output.mode.width
						&& mode.height == output.mode.height
						&& mode.refresh_millihertz.abs_diff(output.mode.refresh_millihertz) <= 100
				});
			if output.enabled && selected.is_none() {
				return Err(format!("KWin does not expose requested mode for {}", output.name));
			}
			resolved.push((
				device.proxy.clone().ok_or("Output proxy missing")?,
				selected.map(|m| m.0.clone()),
				output,
			));
		}
		let configuration = self
			.state
			.manager
			.as_ref()
			.ok_or("KWin output manager unavailable")?
			.create_configuration(&self.queue.handle(), ());
		for (device, mode, output) in resolved {
			configuration.enable(&device, i32::from(output.enabled));
			if output.enabled {
				configuration.mode(&device, &mode.ok_or("Mode missing")?);
				configuration.position(&device, output.x, output.y);
				configuration.scale(&device, output.scale);
				configuration.transform(&device, output.transform as i32);
				configuration.set_priority(&device, output.priority);
				configuration.set_high_dynamic_range(&device, output.hdr);
				configuration.set_wide_color_gamut(&device, output.wcg);
				configuration.set_replication_source(&device, output.replication.clone());
			}
		}
		self.apply_config(configuration)
	}
	/// The capture worker drains old buffers before committing a replacement.
	/// Non-disableable virtual outputs must then be closed, rather than sent
	/// an unsupported enable(false) request in the new topology.
	pub fn retire_other(&mut self, selected: &str) -> Result<()> {
		let others: Vec<_> = self
			.state
			.streams
			.keys()
			.filter(|id| id.as_str() != selected)
			.cloned()
			.collect();
		for id in others {
			self.retire(&id)?;
		}
		Ok(())
	}
	pub fn retire(&mut self, id: &str) -> Result<()> {
		if let Some(stream) = self.state.streams.remove(id).and_then(|s| s.proxy) {
			stream.close();
		}
		self.synchronize()
	}
}
/// The mode of exactly `requested`'s size whose refresh is nearest to it,
/// within the rounding of a CVT-generated custom mode (1%, at least 100 mHz).
fn generated_mode(modes: impl IntoIterator<Item = Mode>, requested: Mode) -> Option<Mode> {
	let tolerance = (requested.refresh_millihertz / 100).max(100);
	modes
		.into_iter()
		.filter(|m| m.width == requested.width && m.height == requested.height)
		.min_by_key(|m| m.refresh_millihertz.abs_diff(requested.refresh_millihertz))
		.filter(|m| m.refresh_millihertz.abs_diff(requested.refresh_millihertz) <= tolerance)
}

/// Every property we mutate is available at management version 18; devices
/// need 18 as well so priority and custom-mode state can be observed on restore.
fn supported_management(version: u32) -> Option<u32> {
	(version >= 18).then_some(version.min(22))
}
impl State {
	fn bind_classic(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32, q: &QueueHandle<Self>) {
		if version < 18 {
			return;
		}
		let proxy: device::KdeOutputDeviceV2 = registry.bind(name, version.min(26), q, ());
		self.classic_devices.insert(name, proxy.id());
		self.devices.insert(
			proxy.id(),
			Device {
				proxy: Some(proxy),
				scale: 1.0,
				..Default::default()
			},
		);
	}
	fn remove_device(&mut self, id: &ObjectId) {
		if let Some(device) = self.devices.remove(id) {
			for mode in device.modes {
				self.modes.remove(&mode);
			}
		}
	}
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
	fn event(
		state: &mut Self,
		registry: &wl_registry::WlRegistry,
		event: wl_registry::Event,
		_: &(),
		_: &Connection,
		q: &QueueHandle<Self>,
	) {
		if let wl_registry::Event::GlobalRemove { name } = event {
			state.classic_offers.retain(|(global, _)| *global != name);
			if let Some(id) = state.classic_devices.remove(&name) {
				state.remove_device(&id);
			}
			return;
		}
		if let wl_registry::Event::Global {
			name,
			interface,
			version,
		} = event
		{
			match interface.as_str() {
				"kde_output_device_registry_v2" if version >= 21 => {
					state.registry = Some(registry.bind(name, version.min(26), q, ()))
				},
				"kde_output_management_v2" => {
					if let Some(version) = supported_management(version) {
						state.manager = Some(registry.bind(name, version, q, ()));
					}
				},
				"kde_output_device_v2" => {
					if state.classic {
						state.bind_classic(registry, name, version, q);
					} else {
						state.classic_offers.push((name, version));
					}
				},
				"zkde_screencast_unstable_v1" if version >= 4 => {
					state.cast = Some(registry.bind(name, version.min(6), q, ()))
				},
				_ => {},
			}
		}
	}
}
impl Dispatch<wl_callback::WlCallback, u64> for State {
	fn event(
		state: &mut Self,
		_: &wl_callback::WlCallback,
		_: wl_callback::Event,
		serial: &u64,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		state.sync = state.sync.max(*serial);
	}
}
impl Dispatch<registry::KdeOutputDeviceRegistryV2, ()> for State {
	fn event(
		state: &mut Self,
		_: &registry::KdeOutputDeviceRegistryV2,
		event: registry::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let registry::Event::Output { output } = event {
			state.devices.insert(
				output.id(),
				Device {
					proxy: Some(output),
					scale: 1.0,
					..Default::default()
				},
			);
		}
	}
	event_created_child!(State, registry::KdeOutputDeviceRegistryV2, [1 => (device::KdeOutputDeviceV2, ())]);
}
impl Dispatch<device::KdeOutputDeviceV2, ()> for State {
	fn event(
		state: &mut Self,
		proxy: &device::KdeOutputDeviceV2,
		event: device::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if matches!(event, device::Event::Removed) {
			state.remove_device(&proxy.id());
			return;
		}
		let d = state.devices.entry(proxy.id()).or_insert_with(|| Device {
			proxy: Some(proxy.clone()),
			scale: 1.0,
			..Default::default()
		});
		match event {
			device::Event::Geometry { x, y, transform, .. } => {
				d.x = x;
				d.y = y;
				d.transform = transform as u32;
			},
			device::Event::Name { name } => d.name = name,
			device::Event::Uuid { uuid } => d.uuid = uuid,
			device::Event::Enabled { enabled } => d.enabled = enabled != 0,
			device::Event::Scale { factor } => d.scale = factor,
			device::Event::CurrentMode { mode } => d.current = Some(mode.id()),
			device::Event::Mode { mode } => {
				d.modes.push(mode.id());
				state.modes.insert(mode.id(), (mode, Mode::default()));
			},
			device::Event::Priority { priority } => d.priority = priority,
			device::Event::Capabilities { flags } => {
				let bits: u32 = flags.into();
				d.can_disable = bits & u32::from(device::Capability::Disable) != 0;
			},
			device::Event::HighDynamicRange { hdr_enabled } => d.hdr = hdr_enabled,
			device::Event::WideColorGamut { wcg_enabled } => d.wcg = wcg_enabled,
			device::Event::ReplicationSource { source } => d.replication = source,
			_ => {},
		}
	}
	event_created_child!(State, device::KdeOutputDeviceV2, [2 => (mode::KdeOutputDeviceModeV2, ())]);
}
impl Dispatch<mode::KdeOutputDeviceModeV2, ()> for State {
	fn event(
		state: &mut Self,
		proxy: &mode::KdeOutputDeviceModeV2,
		event: mode::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if matches!(event, mode::Event::Removed) {
			state.modes.remove(&proxy.id());
			for device in state.devices.values_mut() {
				device.modes.retain(|id| *id != proxy.id());
			}
			if proxy.version() >= 26 {
				proxy.release();
			}
			return;
		}
		if let Some((_, mode)) = state.modes.get_mut(&proxy.id()) {
			match event {
				mode::Event::Size { width, height } => {
					mode.width = width.max(0) as u32;
					mode.height = height.max(0) as u32;
				},
				mode::Event::Refresh { refresh } => mode.refresh_millihertz = refresh.max(0) as u32,
				_ => {},
			}
		}
	}
}
impl Dispatch<cast_stream::ZkdeScreencastStreamUnstableV1, String> for State {
	fn event(
		state: &mut Self,
		_: &cast_stream::ZkdeScreencastStreamUnstableV1,
		event: cast_stream::Event,
		id: &String,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let Some(stream) = state.streams.get_mut(id) {
			match event {
				cast_stream::Event::Created { node } => stream.node = Some(node),
				cast_stream::Event::Serial {
					object_serial_hi,
					object_serial_low,
				} => stream.serial = Some((u64::from(object_serial_hi) << 32) | u64::from(object_serial_low)),
				cast_stream::Event::Failed { error } => stream.failure = Some(error),
				cast_stream::Event::Closed => stream.failure = Some("KWin closed the virtual output".into()),
			}
		}
	}
}
impl Dispatch<config::KdeOutputConfigurationV2, ()> for State {
	fn event(
		state: &mut Self,
		_: &config::KdeOutputConfigurationV2,
		event: config::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			config::Event::Applied => state.applied = Some(Ok(())),
			config::Event::FailureReason { reason } => state.failure_reason = reason,
			config::Event::Failed => {
				state.applied = Some(Err(format!(
					"KWin rejected output configuration: {}",
					state.failure_reason
				)))
			},
		}
	}
}
delegate_noop!(State: ignore manager::KdeOutputManagementV2);
delegate_noop!(State: ignore modes::KdeModeListV2);
delegate_noop!(State: ignore cast_manager::ZkdeScreencastUnstableV1);

#[cfg(test)]
mod tests {
	/// CVT reduced blanking rounds the refresh by more than 100 mHz for
	/// common stream modes (libxcvt 0.1.3 values, as KWin 6.8 generates them).
	#[test]
	fn selects_the_cvt_rounded_custom_mode() {
		let mode = |width, height, refresh_millihertz| super::Mode {
			width,
			height,
			refresh_millihertz,
		};
		// The virtual output starts with a 60 Hz mode of the same size.
		let modes = [mode(2560, 1440, 60000), mode(2560, 1440, 115886)];
		assert_eq!(
			super::generated_mode(modes, mode(2560, 1440, 116000)),
			Some(mode(2560, 1440, 115886))
		);
		assert_eq!(
			super::generated_mode([mode(1280, 720, 59745)], mode(1280, 720, 60000)),
			Some(mode(1280, 720, 59745))
		);
		// Another size, or a refresh KWin did not generate, is not a match.
		assert_eq!(
			super::generated_mode([mode(2560, 1440, 60000)], mode(2560, 1440, 116000)),
			None
		);
		assert_eq!(
			super::generated_mode([mode(2560, 1600, 115936)], mode(2560, 1440, 116000)),
			None
		);
	}

	#[test]
	fn compatibility_floor_covers_every_required_request_and_observation() {
		assert_eq!(super::supported_management(17), None);
		assert_eq!(super::supported_management(18), Some(18));
		assert_eq!(super::supported_management(21), Some(21));
		assert_eq!(super::supported_management(26), Some(22));
	}
	use super::*;

	/// Adds/removes one virtual output; physical displays stay enabled.
	#[test]
	#[ignore = "requires an unlocked KDE Wayland desktop and capture GPU"]
	fn native_120hz_capture_and_owner_cleanup_on_kde() {
		assert_eq!(std::env::var("MOONSHINE_TEST_DESKTOP").as_deref(), Ok("1"));
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let desktop = runtime.block_on(async {
			let bus = zbus::Connection::session().await.unwrap();
			crate::session::capture::portal::desktop::discover(&bus).await.unwrap()
		});
		assert_eq!(desktop.kind, crate::session::capture::portal::desktop::DesktopKind::Kde);
		let mut native = Kde::connect(&desktop.wayland_socket).unwrap();
		let baseline = native.snapshot().unwrap();
		let baseline_modes = native.state.modes.len();
		let id = uuid::Uuid::new_v4().to_string();
		// MOONSHINE_TEST_DESKTOP_MODE=2560x1440@116 tests another client mode.
		let requested = std::env::var("MOONSHINE_TEST_DESKTOP_MODE").map_or(
			Mode {
				width: 2880,
				height: 1920,
				refresh_millihertz: 120000,
			},
			|mode| {
				let (size, rate) = mode.split_once('@').expect("<width>x<height>@<hz>");
				let (width, height) = size.split_once('x').expect("<width>x<height>@<hz>");
				Mode {
					width: width.parse().unwrap(),
					height: height.parse().unwrap(),
					refresh_millihertz: rate.parse::<u32>().unwrap() * 1000,
				}
			},
		);
		let result = (|| -> Result<()> {
			let target = native.prepare(&id, requested)?;
			let state = native.snapshot()?;
			let output = state
				.outputs
				.iter()
				.find(|o| o.key == target.output)
				.ok_or("Virtual output missing")?;
			assert!(!output.can_disable, "KWin virtual outputs are owner-closed");
			assert!(
				output.mode.refresh_millihertz.abs_diff(requested.refresh_millihertz)
					<= requested.refresh_millihertz / 100
			);
			let device = crate::session::capture::portal::device::compositor_main_device(&desktop.wayland_socket)?
				.ok_or("Missing compositor GPU identity")?;
			let node = crate::session::capture::portal::device::render_node(device).ok_or("Missing render node")?;
			let file = std::fs::OpenOptions::new()
				.read(true)
				.write(true)
				.open(node)
				.map_err(|e| e.to_string())?;
			let gpu = crate::gpu::capture_context(&file)?;
			let importable = crate::gpu::ImportableModifiers::query(&gpu);
			let offered = crate::session::capture::portal::stream::offered_formats(|f| importable.modifiers(f));
			let (sender, _receiver) = crate::session::capture::admission::capture_channel();
			sender.set_context(gpu);
			pipewire::init();
			let mainloop = pipewire::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
			let context = pipewire::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
			let core = context.connect_rc(None).map_err(|e| e.to_string())?;
			let stream = crate::session::capture::portal::stream::CaptureStream::connect(
				&core,
				&target,
				offered,
				sender,
				crate::session::capture::portal::readiness::Readiness::new()?,
				false,
				crate::session::capture::portal::sync::SyncDevice::probe(file.as_fd()),
				false,
			)?;
			let format = crate::session::capture::portal::wait_format(
				&mainloop,
				&stream,
				&target,
				async_shutdown::ShutdownManager::new(),
			)?;
			eprintln!(
				"kde-native-capture: {}x{}, max rate {:?}",
				format.width, format.height, format.max_framerate
			);
			for scale in [1.25, 1.75, 2.0, 1.0] {
				let mut desired = native.snapshot()?;
				desired
					.outputs
					.iter_mut()
					.find(|o| o.key == target.output)
					.ok_or("Virtual output missing")?
					.scale = scale;
				native.apply(&desired)?;
				let observed = native.snapshot()?;
				super::super::verify(&observed, &desired)?;
				let managed = super::super::takeover_configuration(&observed, &target, None)?;
				if managed.outputs.iter().find(|o| o.key == target.output).map(|o| o.scale) != Some(scale) {
					return Err("Takeover would reset the user's UI scale".into());
				}
				// Dispatch renegotiation without taking over physical monitors.
				let until = Instant::now() + Duration::from_millis(200);
				while Instant::now() < until {
					mainloop
						.loop_()
						.iterate(pipewire::loop_::Timeout::Finite(Duration::from_millis(10)));
				}
				let scaled_format = crate::session::capture::portal::wait_format(
					&mainloop,
					&stream,
					&target,
					async_shutdown::ShutdownManager::new(),
				)?;
				if (scaled_format.width, scaled_format.height) != (requested.width, requested.height) {
					return Err("UI scale changed the capture pixel resolution".into());
				}
				eprintln!(
					"kde-native-capture: UI scale {scale}, capture {}x{}",
					scaled_format.width, scaled_format.height
				);
			}
			Ok(())
		})();
		// Exercise the guard's restore-before-close ordering with a real
		// non-disableable output. Physical displays remain enabled throughout.
		let recovery = native.snapshot().and_then(|current| {
			let owned: Vec<_> = current
				.outputs
				.iter()
				.filter(|o| o.name == format!("Virtual-Pyroshine-{id}"))
				.map(|o| o.key.clone())
				.collect();
			let (desired, partial) = super::super::restore_configuration(&current, &baseline, &owned)?;
			if partial {
				return Err("Physical monitor disappeared during the test".into());
			}
			native.apply(&desired)?;
			super::super::verify(&native.snapshot()?, &desired)
		});
		// Always close the owner, including when negotiation/recovery fails.
		let cleanup = native.retire(&id);
		let restored = native.snapshot().and_then(|s| super::super::verify(&s, &baseline));
		assert_eq!(cleanup, Ok(()));
		assert_eq!(recovery, Ok(()));
		assert_eq!(restored, Ok(()));
		assert_eq!(
			native.state.modes.len(),
			baseline_modes,
			"retired output modes must not accumulate"
		);
		assert_eq!(result, Ok(()));
	}
}

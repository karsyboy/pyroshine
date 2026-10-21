//! The GPU the desktop compositor renders, and so shares its frames, on.
//!
//! Screen-cast DMA-BUFs are allocated on the compositor's main device, which
//! it advertises in its `zwp_linux_dmabuf_v1` default feedback (version 4).
//! Pyroshine imports frames only on that device: there is no cross-device
//! path. A compositor without feedback leaves the choice to `compositor.gpu`
//! (or the detected render node), which must then be the compositor's GPU.

use std::path::{Path, PathBuf};

use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1};

#[derive(Default)]
struct State {
	dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
	main_device: Option<u64>,
	done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
	fn event(
		state: &mut Self,
		registry: &wl_registry::WlRegistry,
		event: wl_registry::Event,
		_: &(),
		_: &Connection,
		queue: &QueueHandle<Self>,
	) {
		if let wl_registry::Event::Global {
			name,
			interface,
			version,
		} = event && interface == "zwp_linux_dmabuf_v1"
			&& version >= 4
		{
			state.dmabuf = Some(registry.bind(name, 4, queue, ()));
		}
	}
}

delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for State {
	fn event(
		state: &mut Self,
		_: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
		event: zwp_linux_dmabuf_feedback_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
				state.main_device = <[u8; 8]>::try_from(device.as_slice()).ok().map(u64::from_ne_bytes);
			},
			zwp_linux_dmabuf_feedback_v1::Event::Done => state.done = true,
			_ => {},
		}
	}
}

/// The compositor's main DRM device (`dev_t`) from its DMA-BUF feedback, or
/// `None` when it does not advertise one (feedback before version 4).
pub(crate) fn compositor_main_device(socket: &Path) -> Result<Option<u64>, String> {
	let stream = std::os::unix::net::UnixStream::connect(socket)
		.map_err(|e| format!("cannot connect to the desktop compositor at {}: {e}", socket.display()))?;
	let connection =
		Connection::from_socket(stream).map_err(|e| format!("cannot talk to the desktop compositor: {e}"))?;
	let mut queue = connection.new_event_queue();
	let handle = queue.handle();
	connection.display().get_registry(&handle, ());
	let mut state = State::default();
	queue
		.roundtrip(&mut state)
		.map_err(|e| format!("desktop compositor registry: {e}"))?;
	let Some(dmabuf) = state.dmabuf.clone() else {
		return Ok(None);
	};
	let feedback = dmabuf.get_default_feedback(&handle, ());
	for _ in 0..4 {
		if state.done {
			break;
		}
		queue
			.roundtrip(&mut state)
			.map_err(|e| format!("desktop compositor DMA-BUF feedback: {e}"))?;
	}
	feedback.destroy();
	dmabuf.destroy();
	let _ = connection.flush();
	Ok(state.main_device)
}

/// The render node of DRM device `dev` (a primary or render node).
pub(crate) fn render_node(dev: u64) -> Option<PathBuf> {
	render_node_in(Path::new("/sys/dev/char"), Path::new("/dev/dri"), dev)
}

fn render_node_in(sys_char: &Path, dev_dri: &Path, dev: u64) -> Option<PathBuf> {
	let (major, minor) = (libc::major(dev), libc::minor(dev));
	let drm = sys_char.join(format!("{major}:{minor}")).join("device").join("drm");
	std::fs::read_dir(drm)
		.ok()?
		.filter_map(Result::ok)
		.filter_map(|entry| entry.file_name().into_string().ok())
		.find(|name| name.starts_with("renderD"))
		.map(|name| dev_dri.join(name))
}

/// The `dev_t` of the device node at `path`.
pub(crate) fn device_number(path: &Path) -> Option<u64> {
	use std::os::unix::fs::MetadataExt;
	std::fs::metadata(path).ok().map(|metadata| metadata.rdev())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn primary_and_render_nodes_resolve_to_the_render_node() {
		let root = tempfile::tempdir().unwrap();
		let sys = root.path().join("char");
		for node in ["226:0", "226:128"] {
			let drm = sys.join(node).join("device").join("drm");
			std::fs::create_dir_all(drm.join("card0")).unwrap();
			std::fs::create_dir_all(drm.join("renderD128")).unwrap();
		}
		let dri = Path::new("/dev/dri");
		assert_eq!(
			render_node_in(&sys, dri, libc::makedev(226, 0)),
			Some(dri.join("renderD128"))
		);
		assert_eq!(
			render_node_in(&sys, dri, libc::makedev(226, 128)),
			Some(dri.join("renderD128"))
		);
		assert_eq!(render_node_in(&sys, dri, libc::makedev(226, 1)), None);
	}
}

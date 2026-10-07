//! Embedded headless Smithay compositor for Moonshine.
//!
//! This module replaces the external Gamescope compositor and PipeWire capture
//! with an in-process Smithay compositor. Frames are rendered to GBM-backed
//! DMA-BUFs and exported directly to the video encoder.

pub(crate) mod admission;
mod capture;
mod color_management;
mod color_render;
mod cursor;
mod focus;
mod foreground;
pub(crate) mod frame;
mod gpu_timing;
mod handlers;
pub(crate) mod input;
mod scaling;
mod state;
mod x11_focus;
mod xwayland_process;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{Capability, GlesRenderer};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::Transform;

use crate::session::SessionContext;
use crate::session::manager::SessionShutdownReason;

use self::admission::{CaptureReceiver, CaptureSender, capture_channel};
use self::input::CompositorInputEvent;
use self::state::MoonshineCompositor;

/// Keyboard configuration for the compositor's XKB state.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct KeyboardConfig {
	pub layout: String,
	pub variant: String,
	pub model: String,
	pub options: Option<String>,
}

impl Default for KeyboardConfig {
	fn default() -> Self {
		Self {
			layout: "us".to_string(),
			variant: String::new(),
			model: String::new(),
			options: None,
		}
	}
}

/// How focus is split between windows. Mirrors gamescope's
/// `VirtualConnectorStrategy` (`backend_virtual_connector_strategy`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualConnectorStrategy {
	/// One focus across the whole output; the highest-priority window wins.
	#[default]
	SingleApplication,
	/// Steam names the focus window/app-id list.
	SteamControlled,
	/// One focus per app id.
	PerAppId,
	/// One focus per window.
	PerWindow,
}

impl VirtualConnectorStrategy {
	/// Strategies that only ever drive a single output/connector.
	pub fn is_single_output(self) -> bool {
		matches!(self, Self::SingleApplication | Self::SteamControlled)
	}
}

/// Select complete-scene capture; auto preserves the zero-copy fast path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
	#[default]
	Auto,
	Composited,
}

/// Benchmark-only synthetic pointer, injected through the normal input path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchPointer {
	/// One absolute motion at the output centre activates the cursor.
	Static,
	/// Absolute motion on every refresh tick along a fixed circle.
	Moving,
}

/// Configuration for the embedded headless compositor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CompositorConfig {
	/// Optional GPU device identifier for compositor rendering.
	pub gpu: Option<String>,
	/// Automatic direct export, or forced composition for compatibility diagnosis.
	pub capture_mode: CaptureMode,

	/// Whether to enable HDR mode in the compositor if the client supports it.
	pub hdr: bool,

	/// Steam integration mode, equivalent to gamescope's `-e`. Promotes the
	/// connector strategy to [`VirtualConnectorStrategy::SteamControlled`] and
	/// enables Steam's window filtering.
	pub steam_mode: bool,

	/// Focus split strategy, equivalent to gamescope's
	/// `backend_virtual_connector_strategy`. Only meaningful without
	/// `steam_mode`/multiple outputs.
	pub virtual_connector_strategy: VirtualConnectorStrategy,

	/// Keyboard configuration for the compositor's XKB state.
	pub keyboard: KeyboardConfig,

	/// Benchmark diagnostic: emulate client pointer use so cursor capture
	/// paths can be measured without a Moonlight control stream.
	#[serde(skip)]
	pub bench_pointer: Option<BenchPointer>,
}

impl Default for CompositorConfig {
	fn default() -> Self {
		Self {
			gpu: None,
			capture_mode: CaptureMode::Auto,
			hdr: true,
			steam_mode: true,
			virtual_connector_strategy: VirtualConnectorStrategy::SingleApplication,
			keyboard: KeyboardConfig::default(),
			bench_pointer: None,
		}
	}
}

/// Runtime context derived from the client's session request.
pub(crate) struct CompositorContext {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
	pub output_scale: f64,
	pub log_stats: bool,
}

/// Information sent from the compositor thread once XWayland is ready.
pub(crate) struct CompositorReady {
	/// X11 display number (e.g. `:1` → `1`).
	pub xdisplay: u32,
	/// Wayland socket name for the session compositor.
	pub wayland_display: String,
	/// Whether this compositor can switch HDR on later without restarting.
	pub hdr_capable: bool,
}

/// Properties of the live virtual output. Only these require changing the
/// running compositor; codec, bitrate, chroma, bit depth and transport
/// settings belong to the video pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputMode {
	pub width: u32,
	pub height: u32,
	pub refresh_rate: u32,
	pub hdr: bool,
}

struct CompositorReconfigure {
	mode: OutputMode,
	applied: tokio::sync::oneshot::Sender<Result<bool, String>>,
}

/// Handles returned by `Compositor::new()` for wiring into streams.
pub(crate) struct CompositorHandles {
	pub frame_rx: CaptureReceiver,
	pub input_tx: calloop::channel::Sender<CompositorInputEvent>,
}

/// Producer endpoints handed to the compositor thread at launch.
struct CompositorOutputs {
	frame_tx: CaptureSender,
	ready_tx: mpsc::SyncSender<CompositorReady>,
	foreground_tx: tokio::sync::watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
}

/// Unlaunched compositor — holds channel endpoints, can only be launched.
pub(crate) struct Compositor {
	config: CompositorConfig,
	context: CompositorContext,
	stop: ShutdownManager<SessionShutdownReason>,
	frame_tx: CaptureSender,
	foreground_tx: tokio::sync::watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
	input_rx: calloop::channel::Channel<CompositorInputEvent>,
	ready_tx: std::sync::mpsc::SyncSender<CompositorReady>,
	ready_rx: std::sync::mpsc::Receiver<CompositorReady>,
	reconfigure_tx: calloop::channel::Sender<CompositorReconfigure>,
	reconfigure_rx: calloop::channel::Channel<CompositorReconfigure>,
}

/// Launched compositor — can be queried, cannot be launched again.
pub(crate) struct LaunchedCompositor {
	ready: CompositorReady,
	reconfigure_tx: calloop::channel::Sender<CompositorReconfigure>,
	/// The applied output mode, with the effective (not requested) HDR state.
	mode: OutputMode,
}

impl CompositorContext {
	pub fn from_session(ctx: &SessionContext, log_stats: bool) -> Self {
		let output_scale = sanitize_output_scale(ctx.application.output_scale);
		if ctx.application.output_scale.is_some_and(|scale| scale != output_scale) {
			tracing::warn!(
				scale = ctx.application.output_scale,
				"Invalid application output scale; using 1.0"
			);
		}
		Self {
			width: ctx.resolution.0,
			height: ctx.resolution.1,
			refresh_rate: ctx.refresh_rate,
			hdr: ctx.hdr,
			output_scale,
			log_stats,
		}
	}
}

fn sanitize_output_scale(scale: Option<f64>) -> f64 {
	match scale {
		Some(scale) if scale.is_finite() && (0.25..=8.0).contains(&scale) => scale,
		_ => 1.0,
	}
}

impl Compositor {
	pub fn new(
		config: CompositorConfig,
		context: CompositorContext,
		stop: ShutdownManager<SessionShutdownReason>,
		foreground_tx: tokio::sync::watch::Sender<Option<moonshine_management::dto::ForegroundApplication>>,
	) -> (Self, CompositorHandles) {
		let (frame_tx, frame_rx) = capture_channel();
		let (input_tx, input_rx) = calloop::channel::channel();
		let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
		let (reconfigure_tx, reconfigure_rx) = calloop::channel::channel();

		(
			Self {
				config,
				context,
				stop,
				frame_tx,
				foreground_tx,
				input_rx,
				ready_tx,
				ready_rx,
				reconfigure_tx,
				reconfigure_rx,
			},
			CompositorHandles { frame_rx, input_tx },
		)
	}

	pub fn launch(self) -> Result<LaunchedCompositor, ()> {
		let Self {
			config,
			context,
			stop,
			frame_tx,
			foreground_tx,
			input_rx,
			ready_tx,
			ready_rx,
			reconfigure_tx,
			reconfigure_rx,
		} = self;

		let requested = OutputMode {
			width: context.width,
			height: context.height,
			refresh_rate: context.refresh_rate,
			hdr: context.hdr,
		};
		// Registered before the thread exists: session completion then implies
		// the compositor state (buffer pools, client buffers, Xwayland) is gone.
		let worker = crate::session::lifecycle::WorkerGuard::register(&stop, SessionShutdownReason::CompositorStopped)?;
		let outputs = CompositorOutputs {
			frame_tx,
			ready_tx,
			foreground_tx,
		};
		std::thread::Builder::new()
			.name("compositor".to_string())
			.spawn(move || {
				let _worker = worker;
				if let Err(e) = run_compositor(config, context, outputs, input_rx, reconfigure_rx, stop) {
					tracing::error!("Compositor failed: {e}");
				}
			})
			.map_err(|e| {
				tracing::error!("Failed to spawn compositor thread: {e}");
			})?;

		let ready = ready_rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| {
			tracing::warn!("Timed out waiting for compositor ready: {e}");
		})?;

		// Mirrors the launch-time format selection: HDR only when capable.
		let mode = OutputMode {
			hdr: requested.hdr && ready.hdr_capable,
			..requested
		};
		Ok(LaunchedCompositor {
			ready,
			reconfigure_tx,
			mode,
		})
	}
}

impl LaunchedCompositor {
	pub fn ready(&self) -> &CompositorReady {
		&self.ready
	}
	/// Apply `mode` to the live output and return the effective HDR state.
	///
	/// An unchanged mode is not sent to the compositor: resetting the output,
	/// its damage tracking or the clients' advertised wl_output state for a
	/// media-only reconnect (codec, bitrate, chroma, FEC, ...) has no purpose.
	pub async fn reconfigure(&mut self, mode: OutputMode) -> Result<bool, ()> {
		if mode == self.mode {
			tracing::debug!(?mode, "Reconnect keeps the live compositor output mode");
			return Ok(self.mode.hdr);
		}
		let (applied, waiting) = tokio::sync::oneshot::channel();
		self.reconfigure_tx
			.send(CompositorReconfigure { mode, applied })
			.map_err(|_| ())?;
		let hdr = waiting
			.await
			.map_err(|_| ())?
			.map_err(|error| tracing::warn!(%error, "Compositor reconfiguration failed"))?;
		self.mode = OutputMode { hdr, ..mode };
		Ok(hdr)
	}
}

/// Main compositor loop running on a dedicated thread.
fn run_compositor(
	config: CompositorConfig,
	context: CompositorContext,
	outputs: CompositorOutputs,
	input_rx: calloop::channel::Channel<CompositorInputEvent>,
	reconfigure_rx: calloop::channel::Channel<CompositorReconfigure>,
	stop: ShutdownManager<SessionShutdownReason>,
) -> Result<(), String> {
	let CompositorOutputs {
		mut frame_tx,
		ready_tx,
		foreground_tx,
	} = outputs;
	let capture_demand = frame_tx.take_demand_source();

	// Open a render node (no DRM master required for headless operation).
	let render_node = find_render_node(&config.gpu)?;
	tracing::debug!("Using render node: {}", render_node.display());

	// Open the render node.
	// Must use read-write access: DRM render nodes require O_RDWR for
	// GPU buffer mapping (amdgpu_bo_cpu_map fails with EACCES otherwise).
	let render_fd_alloc = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(&render_node)
		.map_err(|e| format!("Failed to open render node {}: {e}", render_node.display()))?;

	// Resolve against the actual opened device before publishing readiness or
	// launching an application. Clones retain this GPU across stream epochs.
	frame_tx.set_context(crate::gpu::capture_context(&render_fd_alloc)?);

	// Clone the file handle for the EGL display's GBM device.
	// GbmDevice takes ownership of the file, so we need a separate handle.
	let render_fd_egl = render_fd_alloc
		.try_clone()
		.map_err(|e| format!("Failed to clone render node handle: {e}"))?;

	// Initialize GBM.
	let gbm_device_alloc =
		GbmDevice::new(render_fd_alloc).map_err(|e| format!("Failed to create GBM device for allocator: {e}"))?;
	let gbm_allocator = GbmAllocator::new(gbm_device_alloc, GbmBufferFlags::RENDERING);

	// Initialize EGL + GLES renderer.
	let gbm_device_egl =
		GbmDevice::new(render_fd_egl).map_err(|e| format!("Failed to create GBM device for EGL: {e}"))?;
	let egl_display =
		unsafe { EGLDisplay::new(gbm_device_egl) }.map_err(|e| format!("Failed to create EGL display: {e}"))?;
	let egl_context = EGLContext::new(&egl_display).map_err(|e| format!("Failed to create EGL context: {e}"))?;

	// Use all supported capabilities except per-texture Fencing.
	// Moonshine renders with a single non-shared EGL context and the
	// frame-level EGLFence (ExportFence) already ensures all GPU work is
	// complete before the DMA-BUF is handed to Vulkan.  Per-texture read
	// fences are redundant here and DMA-BUF implicit sync protects client
	// buffer reuse.  Removing them saves ~3% compositor-thread CPU
	// (TextureSync::update_read overhead).
	let capabilities = unsafe { GlesRenderer::supported_capabilities(&egl_context) }
		.map_err(|e| format!("Failed to query renderer capabilities: {e}"))?;
	let capabilities = capabilities.into_iter().filter(|c| *c != Capability::Fencing);
	let mut renderer = unsafe { GlesRenderer::with_capabilities(egl_context, capabilities) }
		.map_err(|e| format!("Failed to create GLES renderer: {e}"))?;

	// Query the EGL display for formats that can be used as render targets.
	let render_formats = renderer.egl_context().dmabuf_render_formats();
	tracing::debug!("Supported DMA-BUF render formats: {}", render_formats.iter().count());

	// Select preferred render format based on HDR mode.
	// HDR: prefer FP16 > 10-bit > 8-bit ABGR. FP16 is required for scRGB
	// (EXTENDED_SRGB_LINEAR) content whose HDR highlights carry values > 1.0 that a
	// 10-bit UNORM render buffer would clamp at composite time; it also holds
	// BT.2020+PQ content (values in [0,1]) losslessly for the passthrough path.
	// SDR: prefer 8-bit ABGR/XBGR to match Vulkan WSI and avoid GL R↔B channel swaps.
	// Vulkan WSI on Wayland defaults to XBGR/ABGR formats, so using ARGB causes
	// GL to incorrectly swap red/blue channels during blit operations.
	let select_format = |preferred_fourccs: &[Fourcc]| {
		preferred_fourccs.iter().find_map(|&fourcc| {
			let modifiers: Vec<Modifier> = render_formats
				.iter()
				.filter(|f| f.code == fourcc)
				.map(|f| f.modifier)
				.collect();
			if modifiers.is_empty() {
				None
			} else {
				Some((fourcc, modifiers))
			}
		})
	};
	let sdr_render_format = select_format(&[Fourcc::Abgr8888, Fourcc::Xbgr8888, Fourcc::Argb8888, Fourcc::Xrgb8888])
		.or_else(|| {
			// Fall back to first available format, collecting all its modifiers.
			let first = render_formats.iter().next()?;
			let fourcc = first.code;
			let modifiers: Vec<Modifier> = render_formats
				.iter()
				.filter(|f| f.code == fourcc)
				.map(|f| f.modifier)
				.collect();
			Some((fourcc, modifiers))
		})
		.ok_or_else(|| "No supported DMA-BUF render formats found".to_string())?;
	let hdr_render_format = config
		.hdr
		.then(|| select_format(&[Fourcc::Abgr16161616f, Fourcc::Abgr2101010]))
		.flatten();
	let hdr_capable = hdr_render_format.is_some();
	let hdr = context.hdr && hdr_capable;
	let (render_fourcc, render_modifiers) = if hdr {
		hdr_render_format.clone().expect("HDR capability checked")
	} else {
		sdr_render_format.clone()
	};

	tracing::debug!(
		"Selected render format: {:?} with {} modifier(s)",
		render_fourcc,
		render_modifiers.len()
	);

	if config.hdr && context.hdr && !hdr {
		tracing::warn!(
			"HDR requested but no HDR-capable format available (using {:?}), falling back to SDR",
			render_fourcc
		);
	}

	// Create the calloop event loop.
	let mut event_loop: EventLoop<MoonshineCompositor> =
		EventLoop::try_new().map_err(|e| format!("Failed to create event loop: {e}"))?;

	// Create the Wayland display.
	let display = smithay::reexports::wayland_server::Display::<MoonshineCompositor>::new()
		.map_err(|e| format!("Failed to create Wayland display: {e}"))?;
	let display_handle = display.handle();

	// Create a virtual output.
	let mode = Mode {
		size: (context.width as i32, context.height as i32).into(),
		refresh: (context.refresh_rate * 1000) as i32,
	};

	// Synthesize plausible physical dimensions so games that check the
	// display's physical size (e.g. Ghost of Tsushima) see a valid monitor
	// instead of a 0x0mm virtual output. Approximate a 27" display at
	// the configured resolution's aspect ratio, but fall back to 16:9 if
	// the configured dimensions are invalid.
	let diag_mm = 686.0_f64; // 27 inches in mm
	let aspect = if context.width == 0 || context.height == 0 {
		16.0_f64 / 9.0_f64
	} else {
		context.width as f64 / context.height as f64
	};
	let h_mm = ((diag_mm / (1.0 + aspect * aspect).sqrt()) as i32).max(1);
	let w_mm = ((h_mm as f64 * aspect) as i32).max(1);

	let output = Output::new(
		"moonshine-virtual".to_string(),
		PhysicalProperties {
			size: (w_mm, h_mm).into(),
			subpixel: Subpixel::Unknown,
			make: "Moonshine".into(),
			model: "Virtual Output".into(),
			serial_number: "".into(),
		},
	);
	output.change_current_state(
		Some(mode),
		Some(Transform::Normal),
		Some(Scale::Fractional(context.output_scale)),
		Some((0, 0).into()),
	);
	output.set_preferred(mode);

	// Create the damage tracker for this output.
	let damage_tracker = OutputDamageTracker::from_output(&output);

	// Compile scene color transforms once; direct export never invokes them.
	let color_shaders = color_render::ColorShaders::new(&mut renderer)
		.map_err(|e| format!("Failed to compile scene color shaders: {e}"))?;

	// Build the compositor state.
	let (mut state, display) = MoonshineCompositor::new(
		display,
		display_handle.clone(),
		event_loop.handle(),
		output,
		damage_tracker,
		gbm_allocator,
		renderer,
		color_shaders,
		frame_tx,
		foreground_tx,
		context.width,
		context.height,
		render_fourcc,
		render_modifiers,
		sdr_render_format,
		hdr_render_format,
		ready_tx,
		&render_node,
		hdr,
		hdr_capable,
		config.steam_mode,
		config.virtual_connector_strategy,
		config.keyboard.clone(),
		config.capture_mode,
		context.log_stats,
	);

	// Insert the Wayland display as a calloop event source so client
	// messages (including XWayland's protocol handshake) are dispatched
	// whenever data arrives on the Wayland socket, not only after the
	// frame timer fires. A dispatcher keeps the Display reachable outside
	// the loop: teardown must run client cleanup after killing XWayland.
	let display_dispatcher = calloop::Dispatcher::new(
		calloop::generic::Generic::new(display, calloop::Interest::READ, calloop::Mode::Level),
		|_, display, state: &mut MoonshineCompositor| {
			// Safety: the Display is dropped only after the loop stops.
			dispatch_wayland_clients(unsafe { display.get_mut() }, state);
			Ok(calloop::PostAction::Continue)
		},
	);
	event_loop
		.handle()
		.register_dispatcher(display_dispatcher.clone())
		.map_err(|e| format!("Failed to insert Wayland display source: {e}"))?;

	// Sources that only serve a live session. Teardown removes them before
	// it dispatches the loop to terminate XWayland.
	let mut session_sources = Vec::new();

	// Register the input channel from the control stream.
	let token = event_loop
		.handle()
		.insert_source(input_rx, |event, _, state: &mut MoonshineCompositor| {
			if let calloop::channel::Event::Msg(input_event) = event {
				input::process_input(input_event, state);
				// Flush queued Wayland events (pointer enter/motion/button, keyboard
				// key, etc.) to the client immediately. Without this the events sit
				// in the outgoing buffer until the next Display dispatch cycle.
				let _ = state.display_handle.flush_clients();
			}
		})
		.map_err(|e| format!("Failed to insert input channel: {e}"))?;
	session_sources.push(token);

	let refresh_rate = Arc::new(AtomicU32::new(context.refresh_rate.max(1)));
	let reconfigured_refresh_rate = refresh_rate.clone();
	let token = event_loop
		.handle()
		.insert_source(reconfigure_rx, move |event, _, state: &mut MoonshineCompositor| {
			if let calloop::channel::Event::Msg(request) = event {
				let OutputMode {
					width,
					height,
					refresh_rate,
					hdr,
				} = request.mode;
				let result = state.reconfigure_output(width, height, refresh_rate, hdr);
				if result.is_ok() {
					reconfigured_refresh_rate.store(refresh_rate.max(1), Ordering::Release);
				}
				let effective_hdr = result.map(|()| state.hdr);
				let _ = request.applied.send(effective_hdr);
			}
		})
		.map_err(|e| format!("Failed to insert compositor reconfiguration channel: {e}"))?;
	session_sources.push(token);

	// Set up the frame timer.
	// Use Instant-based absolute scheduling so that render time inside
	// the callback doesn't drift the cadence. `ToDuration` would add the
	// interval *after* the callback returns, progressively skewing the
	// actual period and producing ~58 Hz instead of 60 Hz.
	let bench_pointer = config.bench_pointer;
	let mut bench_pointer_ticks = 0u32;
	let frame_nanos: u64 = 1_000_000_000u64 / u64::from(context.refresh_rate.max(1));
	let frame_interval = std::time::Duration::from_nanos(frame_nanos);
	// The refresh timer is the only capture clock: each deadline offers one
	// capture, taken before that slot's frame callbacks (see
	// `capture::CaptureSchedule`). Consumer demand only completes a slot the
	// tick deferred, so neither demand nor reconnects can start another phase.
	state.next_refresh_at = std::time::Instant::now() + frame_interval;
	let timer = smithay::reexports::calloop::timer::Timer::from_deadline(state.next_refresh_at);
	let token = event_loop
		.handle()
		.insert_source(timer, move |deadline, _metadata, state: &mut MoonshineCompositor| {
			// Type a bounded batch of any clipboard text queued since the last tick.
			input::drain_pending_text(state);
			if let Some(mode) = bench_pointer {
				if mode == BenchPointer::Moving || bench_pointer_ticks == 0 {
					let angle = f64::from(bench_pointer_ticks) * 0.05;
					let (x, y) = (16384.0 + 8000.0 * angle.cos(), 16384.0 + 8000.0 * angle.sin());
					input::process_input(
						CompositorInputEvent::MouseMoveAbsolute {
							x: x as i16,
							y: y as i16,
							screen_width: i16::MAX,
							screen_height: i16::MAX,
						},
						state,
					);
				}
				bench_pointer_ticks = bench_pointer_ticks.wrapping_add(1);
			}
			state.refresh_tick(deadline);
			// Schedule the next frame relative to the ideal wall-clock
			// target, not relative to "now". This absorbs render-time
			// jitter and keeps a steady cadence.
			let interval = std::time::Duration::from_nanos(
				1_000_000_000u64 / u64::from(refresh_rate.load(Ordering::Acquire).max(1)),
			);
			state.next_refresh_at =
				capture::next_refresh_deadline(state.next_refresh_at, std::time::Instant::now(), interval);
			smithay::reexports::calloop::timer::TimeoutAction::ToInstant(state.next_refresh_at)
		})
		.map_err(|e| format!("Failed to insert frame timer: {e}"))?;
	session_sources.push(token);

	if let Some(demand) = capture_demand {
		// Coalesced consumer demand. Without a deferred slot this is a no-op;
		// the next refresh tick samples admission itself.
		let token = event_loop
			.handle()
			.insert_source(demand, |_, _, state: &mut MoonshineCompositor| {
				state.complete_deferred_capture();
			})
			.map_err(|e| format!("Failed to register capture demand wakeup: {e}"))?;
		session_sources.push(token);
	}

	tracing::info!(
		"Compositor started: {}x{} @ {}Hz, output scale {}",
		context.width,
		context.height,
		context.refresh_rate,
		context.output_scale
	);

	// Run the event loop.
	// Use `None` as timeout so dispatch blocks until the next calloop
	// source fires (frame timer, input channel, or Wayland client event).
	// A hard timeout like 16ms would compete with the frame timer cadence.
	state.start_xwayland();

	tracing::debug!(
		shutdown_triggered = stop.is_shutdown_triggered(),
		"Entering compositor event loop"
	);

	while !stop.is_shutdown_triggered() {
		event_loop
			.dispatch(None, &mut state)
			.map_err(|e| format!("Event loop dispatch error: {e}"))?;
		if state.capture_failed {
			let _ = stop.trigger_shutdown(SessionShutdownReason::CompositorStopped);
		}
	}

	// The manager stopped the application before triggering this stop. Close
	// the session's own X11 helpers, stop rendering and input, then terminate
	// XWayland explicitly: neither Smithay's drop behavior nor `-terminate`
	// ends the process while the loop still holds its connections.
	state.shutdown_session_processes();
	for token in session_sources {
		event_loop.handle().remove(token);
	}
	let xwayland = state.terminate_xwayland(
		|state, client| {
			// Safety: the Display stays owned by the dispatcher; no dispatch
			// of the loop is running.
			let mut source = display_dispatcher.as_source_mut();
			let display = unsafe { source.get_mut() };
			// `dispatch_clients` only cleans up dead clients when some client
			// is readable; dispatching the killed client always does, closing
			// its socket now rather than whenever the Display is dropped. The
			// result is an error for a killed client, as expected.
			let _ = display.backend().dispatch_single_client(state, client);
			let _ = display.flush_clients();
		},
		|state, timeout| {
			if let Err(error) = event_loop.dispatch(Some(timeout), state) {
				tracing::warn!(%error, "Event loop dispatch failed during compositor teardown");
			}
		},
	);
	if let Err(error) = xwayland {
		// Holding the worker guard keeps the session in `Stopping`: the
		// manager's teardown deadline then fails terminally instead of
		// reporting `Idle` while a session process is alive.
		tracing::error!(%error, "Session XWayland survived teardown; holding session completion");
		if let Some(process) = state.xwayland_process.as_ref() {
			while !matches!(process.wait_exit(std::time::Duration::from_secs(5)), Ok(true)) {
				tracing::error!(pid = process.pid(), "Session XWayland is still alive");
			}
		}
	}

	// Drop everything the loop owns now rather than at an unspecified point,
	// and detect any remaining reference cycle that would keep the Display,
	// its client sockets or GPU resources alive past session completion.
	let loop_handle = event_loop.handle().downgrade();
	drop(state);
	drop(display_dispatcher);
	drop(event_loop);
	if !loop_handle.expired() {
		tracing::error!("Compositor event loop leaked after teardown; session resources may outlive the session");
	}

	tracing::info!("Compositor stopped.");
	Ok(())
}

/// Dispatch and flush Wayland clients, including client cleanup for killed
/// or disconnected clients (which is what closes their sockets).
fn dispatch_wayland_clients(
	display: &mut smithay::reexports::wayland_server::Display<MoonshineCompositor>,
	state: &mut MoonshineCompositor,
) {
	if let Err(e) = display.dispatch_clients(state) {
		tracing::error!("Failed to dispatch Wayland clients: {e}");
	}

	// Send deferred wp_image_description_info_v1 destructor events.
	for info in state.deferred_info_done.drain(..) {
		info.done();
	}

	// Flush pending events back to clients. Without this, responses (e.g.
	// wl_registry.global, wl_callback.done) remain buffered and are never
	// sent, causing XWayland's initial roundtrip to block indefinitely.
	if let Err(e) = display.flush_clients() {
		tracing::error!("Failed to flush Wayland clients: {e}");
	}
}

/// Find the appropriate DRM render node.
///
/// Delegates to the shared implementation in the healthcheck module.
fn find_render_node(gpu_config: &Option<String>) -> Result<std::path::PathBuf, String> {
	crate::healthcheck::find_render_node(gpu_config)
}

#[cfg(test)]
mod tests {
	use super::sanitize_output_scale;

	#[test]
	fn output_scale_accepts_fractional_values() {
		assert_eq!(sanitize_output_scale(Some(1.5)), 1.5);
		assert_eq!(sanitize_output_scale(Some(2.0)), 2.0);
	}

	#[test]
	fn output_scale_defaults_or_rejects_invalid_values() {
		assert_eq!(sanitize_output_scale(None), 1.0);
		assert_eq!(sanitize_output_scale(Some(0.0)), 1.0);
		assert_eq!(sanitize_output_scale(Some(f64::NAN)), 1.0);
		assert_eq!(sanitize_output_scale(Some(9.0)), 1.0);
	}
}

#[cfg(test)]
mod reconfigure_tests {
	use super::{CompositorReady, LaunchedCompositor, OutputMode};

	#[tokio::test]
	async fn only_output_mode_changes_reach_the_live_compositor() {
		let (reconfigure_tx, requests) = calloop::channel::channel();
		let mode = OutputMode {
			width: 2880,
			height: 1920,
			refresh_rate: 120,
			hdr: false,
		};
		let mut compositor = LaunchedCompositor {
			ready: CompositorReady {
				xdisplay: 0,
				wayland_display: String::new(),
				hdr_capable: true,
			},
			reconfigure_tx,
			mode,
		};
		// A codec/bitrate/chroma-only reconnect keeps the output untouched.
		assert_eq!(compositor.reconfigure(mode).await, Ok(false));
		assert!(requests.try_recv().is_err());

		let responder = std::thread::spawn(move || {
			let mut applied = Vec::new();
			while let Ok(request) = requests.recv() {
				applied.push(request.mode);
				let _ = request.applied.send(Ok(request.mode.hdr));
			}
			applied
		});
		let changes = [
			OutputMode {
				width: 1920,
				height: 1080,
				..mode
			},
			OutputMode {
				refresh_rate: 60,
				..mode
			},
			OutputMode { hdr: true, ..mode },
		];
		for change in changes {
			assert_eq!(compositor.reconfigure(change).await, Ok(change.hdr));
			// Repeating the now-applied mode is again a no-op.
			assert_eq!(compositor.reconfigure(change).await, Ok(change.hdr));
		}
		drop(compositor);
		assert_eq!(responder.join().unwrap(), changes);
	}
}

#[cfg(test)]
mod xwayland_tests {
	use super::{Compositor, CompositorConfig, CompositorContext};
	use crate::session::manager::SessionShutdownReason;
	use x11rb::connection::Connection;
	use x11rb::protocol::xproto::{
		AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass,
	};
	use x11rb::wrapper::ConnectionExt as _;

	const WIDTH: u16 = 1280;
	const HEIGHT: u16 = 720;

	/// The compositor in Steam mode, as the service runs it.
	fn launch() -> (
		async_shutdown::ShutdownManager<SessionShutdownReason>,
		super::CompositorHandles,
		super::LaunchedCompositor,
	) {
		let (stop, handles, launched, _) = launch_with_foreground();
		(stop, handles, launched)
	}

	fn launch_with_foreground() -> (
		async_shutdown::ShutdownManager<SessionShutdownReason>,
		super::CompositorHandles,
		super::LaunchedCompositor,
		tokio::sync::watch::Receiver<Option<moonshine_management::dto::ForegroundApplication>>,
	) {
		let stop = async_shutdown::ShutdownManager::new();
		let (foreground_tx, foreground) = tokio::sync::watch::channel(None);
		let (compositor, handles) = Compositor::new(
			CompositorConfig {
				steam_mode: true,
				..Default::default()
			},
			CompositorContext {
				width: WIDTH.into(),
				height: HEIGHT.into(),
				refresh_rate: 60,
				hdr: false,
				output_scale: 1.0,
				log_stats: false,
			},
			stop.clone(),
			foreground_tx,
		);
		let launched = compositor.launch().expect("compositor launch");
		(stop, handles, launched, foreground)
	}

	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn foreground_follows_primary_focus_titles_and_window_lifetime() {
		let (stop, _handles, launched, mut foreground) = launch_with_foreground();
		let (conn, screen_num) = x11rb::connect(Some(&format!(":{}", launched.ready().xdisplay))).unwrap();
		let root = conn.setup().roots[screen_num].root;
		let set_title = |window, title: &str| {
			conn.change_property8(
				PropMode::REPLACE,
				window,
				AtomEnum::WM_NAME,
				AtomEnum::STRING,
				title.as_bytes(),
			)
			.unwrap();
			conn.flush().unwrap();
		};
		let observed = foreground.clone();
		let title_is = |title: &str| observed.borrow().as_ref().is_some_and(|app| app.title == title);
		let steam = map_fullscreen(&conn, root, &[(b"STEAM_GAME", 769)]);
		set_title(steam, "Steam");
		assert!(wait_until(|| title_is("Steam")));
		let game = map_fullscreen(&conn, root, &[(b"STEAM_GAME", 219990)]);
		set_title(game, "Grim Dawn");
		assert!(wait_until(|| title_is("Grim Dawn")));
		set_title(game, "Grim Dawn - Running");
		assert!(wait_until(|| title_is("Grim Dawn - Running")));
		// Small Steam overlay = passive notification; full-width = interactive.
		let overlay = map_fullscreen(&conn, root, &[(b"STEAM_GAME", 769), (b"STEAM_OVERLAY", 1)]);
		set_title(overlay, "Steam helper");
		conn.configure_window(overlay, &x11rb::protocol::xproto::ConfigureWindowAux::new().width(200))
			.unwrap();
		conn.flush().unwrap();
		assert!(wait_until(|| cardinal(&conn, root, b"_NET_ACTIVE_WINDOW") == Some(game)));
		assert!(title_is("Grim Dawn - Running"));
		foreground.borrow_and_update();
		set_cardinal(&conn, overlay, b"STEAM_INPUT_FOCUS", 1);
		assert!(wait_until(
			|| conn.get_input_focus().unwrap().reply().unwrap().focus == overlay
		));
		assert!(title_is("Grim Dawn - Running"));
		assert!(!foreground.has_changed().unwrap());
		conn.destroy_window(overlay).unwrap();
		conn.destroy_window(game).unwrap();
		conn.flush().unwrap();
		assert!(wait_until(|| title_is("Steam")));
		conn.destroy_window(steam).unwrap();
		conn.flush().unwrap();
		assert!(wait_until(|| foreground.borrow().is_none()));
		drop(conn);
		shut_down(stop);
	}

	fn shut_down(stop: async_shutdown::ShutdownManager<SessionShutdownReason>) {
		let _ = stop.trigger_shutdown(SessionShutdownReason::UserStopped);
		tokio::runtime::Builder::new_current_thread()
			.enable_time()
			.build()
			.unwrap()
			.block_on(async {
				tokio::time::timeout(std::time::Duration::from_secs(10), stop.wait_shutdown_complete())
					.await
					.expect("compositor teardown")
			});
	}

	fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
		while std::time::Instant::now() < deadline {
			if condition() {
				return true;
			}
			std::thread::sleep(std::time::Duration::from_millis(20));
		}
		false
	}

	fn cardinal(conn: &impl Connection, window: u32, name: &[u8]) -> Option<u32> {
		let atom = conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
		let reply = conn
			.get_property(false, window, atom, AtomEnum::ANY, 0, 1)
			.unwrap()
			.reply()
			.ok()?;
		reply.value32().and_then(|mut values| values.next())
	}

	fn set_cardinal(conn: &impl Connection, window: u32, name: &[u8], value: u32) {
		let atom = conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
		conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[value])
			.unwrap();
		conn.flush().unwrap();
	}

	fn map_fullscreen(conn: &impl Connection, root: u32, properties: &[(&[u8], u32)]) -> u32 {
		let window = conn.generate_id().unwrap();
		conn.create_window(
			x11rb::COPY_DEPTH_FROM_PARENT,
			window,
			root,
			0,
			0,
			WIDTH,
			HEIGHT,
			0,
			WindowClass::INPUT_OUTPUT,
			x11rb::COPY_FROM_PARENT,
			&CreateWindowAux::new().background_pixel(0),
		)
		.unwrap();
		for (name, value) in properties {
			set_cardinal(conn, window, name, *value);
		}
		conn.map_window(window).unwrap();
		conn.flush().unwrap();
		window
	}

	/// The Grim Dawn sequence observed on hardware: the Steam overlay takes
	/// keyboard focus (`STEAM_INPUT_FOCUS=1`), the deactivated Wine game asks
	/// to be iconic, and Steam hides the overlay again without changing the
	/// focus window. The game must be made normal again when it regains
	/// keyboard focus, or Wine never restores it.
	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn game_restored_when_steam_clears_its_overlay_properties() {
		// What Steam did on hardware when Grim Dawn's overlay closed.
		overlay_close_restores_minimized_game(|conn, overlay| {
			set_cardinal(conn, overlay, b"STEAM_INPUT_FOCUS", 0);
			set_cardinal(conn, overlay, b"STEAM_OVERLAY", 0);
		});
	}

	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn game_restored_when_steam_hides_its_overlay_by_opacity() {
		overlay_close_restores_minimized_game(|conn, overlay| {
			set_cardinal(conn, overlay, b"_NET_WM_WINDOW_OPACITY", 0);
		});
	}

	fn overlay_close_restores_minimized_game(close: impl Fn(&x11rb::rust_connection::RustConnection, u32)) {
		const GAME_APP: u32 = 219990;
		const STEAM_APP: u32 = 769;
		let (stop, _handles, launched) = launch();
		let (conn, screen_num) = x11rb::connect(Some(&format!(":{}", launched.ready().xdisplay))).unwrap();
		let root = conn.setup().roots[screen_num].root;
		let active = |conn: &x11rb::rust_connection::RustConnection| cardinal(conn, root, b"_NET_ACTIVE_WINDOW");

		// Steam's focus contract names the game first, as in the live session.
		let baselayer = conn
			.intern_atom(false, b"GAMESCOPECTRL_BASELAYER_APPID")
			.unwrap()
			.reply()
			.unwrap()
			.atom;
		conn.change_property32(
			PropMode::REPLACE,
			root,
			baselayer,
			AtomEnum::CARDINAL,
			&[GAME_APP, STEAM_APP],
		)
		.unwrap();
		let game = map_fullscreen(&conn, root, &[(b"STEAM_GAME", GAME_APP)]);
		assert!(
			wait_until(|| active(&conn) == Some(game)),
			"the game never became active"
		);

		let overlay = map_fullscreen(
			&conn,
			root,
			&[
				(b"STEAM_GAME", STEAM_APP),
				(b"STEAM_OVERLAY", 1),
				(b"STEAM_INPUT_FOCUS", 1),
			],
		);
		let input_focus =
			|conn: &x11rb::rust_connection::RustConnection| conn.get_input_focus().unwrap().reply().unwrap().focus;
		assert!(
			wait_until(|| input_focus(&conn) == overlay),
			"the overlay never took keyboard focus"
		);

		// XIconifyWindow: WM_CHANGE_STATE IconicState to the root window.
		let change_state = conn
			.intern_atom(false, b"WM_CHANGE_STATE")
			.unwrap()
			.reply()
			.unwrap()
			.atom;
		conn.send_event(
			false,
			root,
			EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
			ClientMessageEvent::new(32, game, change_state, [3u32, 0, 0, 0, 0]),
		)
		.unwrap();
		conn.flush().unwrap();
		assert!(
			wait_until(|| cardinal(&conn, game, b"WM_STATE") == Some(3)),
			"the iconic request was not acknowledged"
		);

		close(&conn, overlay);
		let returned = wait_until(|| input_focus(&conn) == game);
		assert!(
			returned,
			"keyboard focus did not return: active={:?} game={game:#x} overlay={overlay:#x} opacity={:?}",
			active(&conn),
			cardinal(&conn, overlay, b"_NET_WM_WINDOW_OPACITY")
		);
		assert!(
			wait_until(|| cardinal(&conn, game, b"WM_STATE") == Some(1)),
			"the game stayed iconic after the overlay closed"
		);

		drop(conn);
		shut_down(stop);
	}

	/// Seen on hardware after the Steam overlay closed: Steam's CEF window
	/// focuses itself while the game is the compositor's keyboard focus. The
	/// compositor must take focus back (as gamescope does), or the game is
	/// deactivated and a fullscreen Wine game minimizes again.
	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn keyboard_focus_taken_by_steam_is_reclaimed() {
		const GAME_APP: u32 = 219990;
		const STEAM_APP: u32 = 769;
		let (stop, _handles, launched) = launch();
		let (conn, screen_num) = x11rb::connect(Some(&format!(":{}", launched.ready().xdisplay))).unwrap();
		let root = conn.setup().roots[screen_num].root;
		let baselayer = conn
			.intern_atom(false, b"GAMESCOPECTRL_BASELAYER_APPID")
			.unwrap()
			.reply()
			.unwrap()
			.atom;
		conn.change_property32(
			PropMode::REPLACE,
			root,
			baselayer,
			AtomEnum::CARDINAL,
			&[GAME_APP, STEAM_APP],
		)
		.unwrap();
		let steam = map_fullscreen(&conn, root, &[(b"STEAM_GAME", STEAM_APP)]);
		let game = map_fullscreen(&conn, root, &[(b"STEAM_GAME", GAME_APP)]);
		let input_focus =
			|conn: &x11rb::rust_connection::RustConnection| conn.get_input_focus().unwrap().reply().unwrap().focus;
		assert!(
			wait_until(|| input_focus(&conn) == game),
			"the game never received focus"
		);

		for _ in 0..3 {
			conn.set_input_focus(x11rb::protocol::xproto::InputFocus::NONE, steam, x11rb::CURRENT_TIME)
				.unwrap();
			conn.flush().unwrap();
			assert!(
				wait_until(|| input_focus(&conn) == game),
				"focus stayed on the window that took it: {:#x}",
				input_focus(&conn)
			);
			assert_eq!(cardinal(&conn, root, b"_NET_ACTIVE_WINDOW"), Some(game));
		}

		// Focus dropped to None is taken back too.
		conn.set_input_focus(
			x11rb::protocol::xproto::InputFocus::NONE,
			x11rb::NONE,
			x11rb::CURRENT_TIME,
		)
		.unwrap();
		conn.flush().unwrap();
		assert!(wait_until(|| input_focus(&conn) == game), "focus stayed on None");

		drop(conn);
		shut_down(stop);
	}

	/// A resuming client with another resolution reconfigures the live output.
	/// Windows held at the output size must follow it, or the application keeps
	/// rendering at the previous client's resolution and is scaled into the
	/// stream; a windowed application keeps its own size.
	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn live_output_reconfiguration_resizes_windows_held_at_output_size() {
		let (stop, _handles, mut launched) = launch();
		let (conn, screen_num) = x11rb::connect(Some(&format!(":{}", launched.ready().xdisplay))).unwrap();
		let root = conn.setup().roots[screen_num].root;
		let size = |window| {
			let geometry = conn.get_geometry(window).unwrap().reply().unwrap();
			(geometry.width, geometry.height)
		};
		let big_picture = map_fullscreen(&conn, root, &[(b"STEAM_GAME", 769)]);
		let windowed = map_fullscreen(&conn, root, &[(b"STEAM_GAME", 219990)]);
		conn.configure_window(
			windowed,
			&x11rb::protocol::xproto::ConfigureWindowAux::new()
				.width(640)
				.height(480),
		)
		.unwrap();
		conn.flush().unwrap();
		assert!(wait_until(|| size(windowed) == (640, 480)));
		assert_eq!(size(big_picture), (WIDTH, HEIGHT));

		let effective_hdr = tokio::runtime::Builder::new_current_thread()
			.build()
			.unwrap()
			.block_on(launched.reconfigure(super::OutputMode {
				width: 1920,
				height: 1080,
				refresh_rate: 60,
				hdr: false,
			}))
			.expect("live reconfiguration");
		assert!(!effective_hdr);
		assert!(
			wait_until(|| size(big_picture) == (1920, 1080)),
			"Big Picture kept {:?} after the output became 1920x1080",
			size(big_picture)
		);
		assert_eq!(size(windowed), (640, 480));

		drop(conn);
		shut_down(stop);
	}

	/// A focus target destroyed while the compositor selects its events yields
	/// an asynchronous BadWindow. It must be ignored: Xlib's default handler
	/// exits the process, which took the whole server down on hardware.
	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn x11_errors_on_destroyed_focus_windows_are_not_fatal() {
		let (stop, _handles, launched) = launch();
		let display = launched.ready().xdisplay;
		let (conn, screen_num) = x11rb::connect(Some(&format!(":{display}"))).unwrap();
		let root = conn.setup().roots[screen_num].root;
		let focus = super::x11_focus::X11Focus::open(display).expect("X11 focus connection");
		for _ in 0..5 {
			let window = map_fullscreen(&conn, root, &[]);
			conn.destroy_window(window).unwrap();
			conn.sync().unwrap();
			// Selecting events on, then reading through, a destroyed window.
			focus.watch_keyboard_focus(window);
			focus.flush();
			let _ = focus.get_input_focus();
			let _ = focus.drain_events();
		}
		drop(focus);
		drop(conn);
		shut_down(stop);
	}
}

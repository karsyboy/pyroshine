//! Native Wayland image descriptions are the sole source of surface color state.
//! Descriptions are applied at commit, looked up for the actual captured surface,
//! and carried with its pixels to the encoder. Unsupported encodings fail rather
//! than being mislabeled SDR. scRGB uses the encoder's 80 cd/m² linear scale.

use std::collections::HashMap;
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
	wp_color_management_output_v1, wp_color_management_surface_feedback_v1, wp_color_management_surface_v1,
	wp_color_manager_v1, wp_image_description_creator_icc_v1, wp_image_description_creator_params_v1,
	wp_image_description_info_v1, wp_image_description_v1,
};
use smithay::reexports::wayland_protocols::wp::color_representation::v1::server::{
	wp_color_representation_manager_v1, wp_color_representation_surface_v1,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::session::compositor::frame::{FrameColorSpace, HdrMetadata};
use crate::session::compositor::state::MoonshineCompositor;

fn send_ready(resource: &wp_image_description_v1::WpImageDescriptionV1) {
	static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3);
	let id = ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	if resource.version() >= 2 {
		resource.ready2((id >> 32) as u32, id as u32);
	} else {
		resource.ready(id as u32);
	}
}

fn supported_description(desc: ImageDescription, luminances: Option<(u32, u32, u32)>) -> bool {
	match (desc.primaries, desc.transfer_function) {
		(Primaries::Bt2020, TransferFunction::St2084Pq) => {
			// PQ is absolute. Keep the stream's reference white at 203 nits;
			// arbitrary viewing-condition adaptation is not implemented.
			luminances.is_none_or(|(min, _, reference)| min <= 50 && reference == 203)
		},
		(Primaries::Srgb, TransferFunction::Srgb | TransferFunction::Gamma22 | TransferFunction::ScrgbLinear) => {
			// The native encoder's linear unit is 80 nits. Do not silently
			// accept another scale: it would change highlights at capture.
			luminances.is_none_or(|(min, max, reference)| min <= 2000 && max == 80 && reference == 80)
		},
		_ => false,
	}
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Transfer function as declared by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferFunction {
	Srgb,
	Gamma22,
	St2084Pq,
	/// Linear light with extended range (scRGB). Values may exceed 1.0 to
	/// represent HDR highlights above SDR white. Paired with BT.709 primaries.
	ScrgbLinear,
}

/// Color primaries as declared by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Primaries {
	Srgb,
	Bt2020,
}

/// A resolved image description created from parametric parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageDescription {
	pub transfer_function: TransferFunction,
	pub primaries: Primaries,
	/// Maximum content light level in cd/m² (nits), if declared.
	pub max_cll: Option<u32>,
	/// Maximum frame-average light level in cd/m² (nits), if declared.
	pub max_fall: Option<u32>,
	/// Mastering display luminance (min, max) in 0.0001 cd/m² units, if declared.
	pub mastering_luminance: Option<(u32, u32)>,
	/// Mastering display primaries [(Rx,Ry), (Gx,Gy), (Bx,By)] in 0.00002 units.
	pub mastering_primaries: Option<[(u32, u32); 3]>,
	/// White point (x, y) in 0.00002 units.
	pub white_point: Option<(u32, u32)>,
}

impl ImageDescription {
	pub fn srgb() -> Self {
		Self {
			transfer_function: TransferFunction::Srgb,
			primaries: Primaries::Srgb,
			max_cll: None,
			max_fall: None,
			mastering_luminance: None,
			mastering_primaries: None,
			white_point: None,
		}
	}

	pub fn bt2020_pq() -> Self {
		Self {
			transfer_function: TransferFunction::St2084Pq,
			primaries: Primaries::Bt2020,
			max_cll: None,
			max_fall: None,
			mastering_luminance: None,
			mastering_primaries: None,
			white_point: None,
		}
	}

	pub fn scrgb_linear() -> Self {
		Self {
			transfer_function: TransferFunction::ScrgbLinear,
			primaries: Primaries::Srgb,
			max_cll: None,
			max_fall: None,
			mastering_luminance: None,
			mastering_primaries: None,
			white_point: None,
		}
	}

	pub fn to_frame_color_space(self) -> FrameColorSpace {
		match (self.primaries, self.transfer_function) {
			(Primaries::Bt2020, TransferFunction::St2084Pq) => FrameColorSpace::Bt2020Pq,
			(Primaries::Srgb, TransferFunction::ScrgbLinear) => FrameColorSpace::ScrgbLinear,
			_ => FrameColorSpace::Srgb,
		}
	}
}

/// Builder state while creating a parametric image description.
#[derive(Debug, Default)]
pub(crate) struct CreatorParams {
	luminances: Option<(u32, u32, u32)>,
	transfer_function: Option<TransferFunction>,
	primaries: Option<Primaries>,
	max_cll: Option<u32>,
	max_fall: Option<u32>,
	mastering_luminance: Option<(u32, u32)>,
	/// Mastering display primaries [(Rx,Ry), (Gx,Gy), (Bx,By)] in 0.00002 units.
	mastering_primaries: Option<[(u32, u32); 3]>,
	/// White point (x, y) in 0.00002 units.
	white_point: Option<(u32, u32)>,
}

// ---------------------------------------------------------------------------
// User-data types attached to protocol resources
// ---------------------------------------------------------------------------

/// User data for `wp_color_management_surface_v1`.
pub(crate) struct ColorSurfaceData {
	pub surface: WlSurface,
}

/// User data for `wp_image_description_v1`.
pub(crate) struct ImageDescriptionUserData {
	pub desc: ImageDescription,
	pub valid: bool,
	pub information_allowed: bool,
}

/// User data for `wp_image_description_creator_params_v1`.
pub(crate) struct CreatorParamsUserData {
	pub(crate) params: Mutex<CreatorParams>,
}

/// User data for `wp_color_management_output_v1` (minimal).
pub(crate) struct ColorOutputData {
	output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
}

/// User data for `wp_color_management_surface_feedback_v1`.
///
/// The `_surface` field keeps the `WlSurface` alive for the protocol lifetime.
pub(crate) struct ColorSurfaceFeedbackData {
	pub _surface: WlSurface,
}

/// User data for `wp_image_description_info_v1`.
pub(crate) struct ImageDescriptionInfoData;

/// User data for `wp_image_description_creator_icc_v1`.
pub(crate) struct IccCreatorData;

/// User data for `wp_color_representation_surface_v1`.
pub(crate) struct ColorRepresentationSurfaceData {
	#[allow(dead_code)]
	pub surface: WlSurface,
}

// ---------------------------------------------------------------------------
// Compositor-level color management state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct SurfaceColor {
	description: Option<ImageDescription>,
}
impl smithay::wayland::compositor::Cacheable for SurfaceColor {
	fn commit(&mut self, _: &DisplayHandle) -> Self {
		Self {
			description: self.description,
		}
	}
	fn merge_into(self, destination: &mut Self, _: &DisplayHandle) {
		destination.description = self.description;
	}
}

/// Tracks per-surface color space declarations.
pub(crate) struct ColorManagementState {
	/// Surface references used to resolve render-element IDs. Color itself is
	/// held in Smithay's transaction cache, atomically with its buffer.
	declared: HashMap<smithay::backend::renderer::element::Id, WlSurface>,
	observed: HashMap<smithay::backend::renderer::element::Id, Option<ImageDescription>>,
	surfaces: HashMap<WlSurface, wp_color_management_surface_v1::WpColorManagementSurfaceV1>,
	outputs: Vec<wp_color_management_output_v1::WpColorManagementOutputV1>,
	feedback: Vec<wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1>,
	/// Whether HDR mode was negotiated with the Moonlight client.
	pub hdr: bool,
}

impl ColorManagementState {
	/// Create a new state and register the protocol globals.
	pub fn new(display: &DisplayHandle, hdr: bool) -> Self {
		// Advertise wp_color_manager_v1 (interface version 3).
		display.create_global::<MoonshineCompositor, wp_color_manager_v1::WpColorManagerV1, _>(3, ());
		// Advertise wp_color_representation_manager_v1 (interface version 1).
		display
			.create_global::<MoonshineCompositor, wp_color_representation_manager_v1::WpColorRepresentationManagerV1, _>(
				1,
				(),
			);

		Self {
			declared: HashMap::new(),
			observed: HashMap::new(),
			surfaces: HashMap::new(),
			outputs: Vec::new(),
			feedback: Vec::new(),
			hdr,
		}
	}

	/// Output feedback changes with the negotiated stream, including reconnects.
	pub fn reconfigure(&mut self, hdr: bool) {
		if self.hdr == hdr {
			return;
		}
		self.hdr = hdr;
		self.outputs.retain(Resource::is_alive);
		self.feedback.retain(Resource::is_alive);
		for resource in &self.outputs {
			resource.image_description_changed();
			if let Some(data) = resource.data::<ColorOutputData>()
				&& data.output.version() >= 2
			{
				data.output.done();
			}
		}
		for resource in &self.feedback {
			if resource.version() >= 2 {
				resource.preferred_changed2(0, if hdr { 2 } else { 1 });
			} else {
				resource.preferred_changed(if hdr { 2 } else { 1 });
			}
		}
	}

	/// Set a pending image description for a surface (from `set_image_description`).
	pub fn set_pending(&mut self, surface: &WlSurface, desc: ImageDescription) {
		tracing::trace!(
			surface_id = ?surface.id(),
			color_space = ?desc.to_frame_color_space(),
			"set_pending"
		);
		self.declared.insert(
			smithay::backend::renderer::element::Id::from_wayland_resource(surface),
			surface.clone(),
		);
		smithay::wayland::compositor::with_states(surface, |states| {
			states.cached_state.get::<SurfaceColor>().pending().description = Some(desc);
		});
	}

	/// Reset on the next surface transaction, including synchronized children.
	pub fn unset_pending(&mut self, surface: &WlSurface) {
		smithay::wayland::compositor::with_states(surface, |states| {
			states.cached_state.get::<SurfaceColor>().pending().description = None;
		});
	}

	/// Detect color changes after native buffer transactions have latched.
	/// A transfer-function change needs redraw even without pixel damage.
	pub fn refresh_color_state(&mut self) -> bool {
		let mut changed = false;
		for (id, surface) in &self.declared {
			let description = self.surface_description(surface);
			if self.observed.get(id).copied().flatten() != description {
				self.observed.insert(id.clone(), description);
				tracing::debug!(surface_id = ?surface.id(), ?description, "Native Wayland color state committed");
				changed = true;
			}
		}
		changed
	}

	/// Resolve only the surface whose pixels are being exported. Other live
	/// (including hidden) HDR surfaces cannot describe this DMA-BUF.
	pub fn surface_color_space(&self, surface: &WlSurface) -> FrameColorSpace {
		self.surface_description(surface)
			.map(ImageDescription::to_frame_color_space)
			.unwrap_or(FrameColorSpace::Srgb)
	}

	fn surface_description(&self, surface: &WlSurface) -> Option<ImageDescription> {
		if !surface.is_alive() {
			return None;
		}
		smithay::wayland::compositor::with_states(surface, |states| {
			states.cached_state.get::<SurfaceColor>().current().description
		})
	}

	pub fn surface_hdr_metadata(&self, surface: &WlSurface) -> Option<HdrMetadata> {
		Self::description_hdr_metadata(self.surface_description(surface)?)
	}

	fn description_hdr_metadata(desc: ImageDescription) -> Option<HdrMetadata> {
		if desc.to_frame_color_space() == FrameColorSpace::Srgb
			|| (desc.max_cll.is_none()
				&& desc.max_fall.is_none()
				&& desc.mastering_luminance.is_none()
				&& desc.mastering_primaries.is_none())
		{
			return None;
		}
		// Clamp u32 protocol values to u16 range for the Moonlight HDR metadata.
		// Well-behaved clients stay within range; clamp rather than truncate.
		let sat = |v: u32| -> u16 { u16::try_from(v).unwrap_or(u16::MAX) };
		Some(HdrMetadata {
			display_primaries: desc.mastering_primaries.map_or(
				if desc.primaries == Primaries::Bt2020 {
					[(35400, 14600), (8500, 39850), (6550, 2300)]
				} else {
					[(32000, 16500), (15000, 30000), (7500, 3000)]
				},
				|p| {
					[
						(sat(p[0].0), sat(p[0].1)),
						(sat(p[1].0), sat(p[1].1)),
						(sat(p[2].0), sat(p[2].1)),
					]
				},
			),
			white_point: desc.white_point.map_or((15635, 16450), |(x, y)| (sat(x), sat(y))),
			max_luminance: desc.mastering_luminance.map_or(
				if desc.transfer_function == TransferFunction::St2084Pq {
					100_000_000
				} else {
					// The 80-nit scRGB unit is reference white, not peak
					// mastering luminance. Keep the existing HDR10 fallback
					// when a partial declaration supplies no mastering range.
					HdrMetadata::fallback().max_luminance
				},
				|(_, max)| max,
			),
			min_luminance: desc.mastering_luminance.map_or(0, |(min, _)| min),
			max_cll: sat(desc.max_cll.unwrap_or(0)),
			max_fall: sat(desc.max_fall.unwrap_or(0)),
		})
	}

	/// Resolve HDR only from a rendered element belonging to the primary
	/// application's actual surface tree, including native Vulkan children.
	pub fn hdr_element_surface(
		&self,
		id: &smithay::backend::renderer::element::Id,
		root: &WlSurface,
	) -> Option<WlSurface> {
		let surface = self.declared.get(id)?;
		if self.surface_color_space(surface) == FrameColorSpace::Srgb {
			return None;
		}
		let mut ancestor = Some(surface.clone());
		while let Some(current) = ancestor {
			if &current == root {
				return Some(surface.clone());
			}
			ancestor = smithay::wayland::compositor::get_parent(&current);
		}
		None
	}

	/// Match a draw element to its own surface declaration; undeclared elements
	/// (including cursors and XWayland) use the default SDR encoding.
	pub fn element_encoding(&self, id: &smithay::backend::renderer::element::Id) -> usize {
		self.declared
			.get(id)
			.and_then(|s| self.surface_description(s))
			.map_or(0, |desc| match desc.transfer_function {
				TransferFunction::Srgb => 0,
				TransferFunction::St2084Pq => 1,
				TransferFunction::ScrgbLinear => 2,
				TransferFunction::Gamma22 => 3,
			})
	}

	/// Clean up tracking for a destroyed surface.
	pub fn surface_destroyed(&mut self, surface: &WlSurface) {
		self.declared
			.remove(&smithay::backend::renderer::element::Id::from_wayland_resource(surface));
		self.surfaces.remove(surface);
		self.observed
			.remove(&smithay::backend::renderer::element::Id::from_wayland_resource(surface));
	}
}

// ---------------------------------------------------------------------------
// wp_color_manager_v1 — Global
// ---------------------------------------------------------------------------

impl GlobalDispatch<wp_color_manager_v1::WpColorManagerV1, ()> for MoonshineCompositor {
	fn bind(
		_state: &mut Self,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<wp_color_manager_v1::WpColorManagerV1>,
		_global_data: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		tracing::debug!("wp_color_manager_v1: client bound global");
		let resource = data_init.init(resource, ());

		// Advertise supported capabilities.
		resource.supported_intent(wp_color_manager_v1::RenderIntent::Perceptual);
		resource.supported_feature(wp_color_manager_v1::Feature::Parametric);
		// Arbitrary container primaries are not supported by the encoder.
		resource.supported_feature(wp_color_manager_v1::Feature::SetMasteringDisplayPrimaries);
		resource.supported_feature(wp_color_manager_v1::Feature::ExtendedTargetVolume);
		resource.supported_feature(wp_color_manager_v1::Feature::SetLuminances);
		resource.supported_feature(wp_color_manager_v1::Feature::WindowsScrgb);
		if resource.version() >= 3 {
			resource.supported_feature(wp_color_manager_v1::Feature::WindowsBt2100);
		}
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::Srgb);
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::Gamma22);
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::St2084Pq);
		resource.supported_tf_named(wp_color_manager_v1::TransferFunction::ExtLinear);
		resource.supported_primaries_named(wp_color_manager_v1::Primaries::Srgb);
		resource.supported_primaries_named(wp_color_manager_v1::Primaries::Bt2020);
		resource.done();
	}
}

impl Dispatch<wp_color_manager_v1::WpColorManagerV1, ()> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_manager_v1::WpColorManagerV1,
		request: wp_color_manager_v1::Request,
		_data: &(),
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		tracing::trace!(?request, "wp_color_manager_v1 request");
		match request {
			wp_color_manager_v1::Request::Destroy => {},

			wp_color_manager_v1::Request::GetSurface { id, surface } => {
				// Initialize new IDs even on protocol errors: wayland-server
				// requires object data before returning from a constructor.
				let resource = data_init.init(
					id,
					ColorSurfaceData {
						surface: surface.clone(),
					},
				);
				if let Some(cm) = &mut state.color_management {
					if cm.surfaces.contains_key(&surface) {
						_resource.post_error(
							wp_color_manager_v1::Error::SurfaceExists,
							"Surface already has a color object",
						);
						return;
					}
					cm.surfaces.insert(surface, resource);
				}
			},

			wp_color_manager_v1::Request::GetOutput { id, output } => {
				let resource = data_init.init(id, ColorOutputData { output });
				if let Some(cm) = &mut state.color_management {
					cm.outputs.push(resource);
				}
			},

			wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
				let resource = data_init.init(id, ColorSurfaceFeedbackData { _surface: surface });
				if resource.version() >= 2 {
					resource.preferred_changed2(0, if state.hdr { 2 } else { 1 });
				} else {
					resource.preferred_changed(if state.hdr { 2 } else { 1 });
				}
				if let Some(cm) = &mut state.color_management {
					cm.feedback.push(resource);
				}
			},

			wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
				data_init.init(
					obj,
					CreatorParamsUserData {
						params: Mutex::new(CreatorParams::default()),
					},
				);
			},

			wp_color_manager_v1::Request::CreateIccCreator { obj } => {
				data_init.init(obj, IccCreatorData);
				_resource.post_error(
					wp_color_manager_v1::Error::UnsupportedFeature,
					"ICC profiles are not supported",
				);
			},

			wp_color_manager_v1::Request::CreateWindowsBt2100 { image_description } => {
				// Predefined BT.2020/PQ space (HDR10) from the Windows-compatibility requests.
				let desc = ImageDescription::bt2020_pq();
				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc,
						valid: true,
						information_allowed: false,
					},
				);
				send_ready(&resource);
			},

			wp_color_manager_v1::Request::CreateWindowsScrgb { image_description } => {
				let desc = ImageDescription::scrgb_linear();
				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc,
						valid: true,
						information_allowed: false,
					},
				);
				send_ready(&resource);
			},

			_ => {
				tracing::debug!("Unhandled wp_color_manager_v1 request");
			},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_management_surface_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_management_surface_v1::WpColorManagementSurfaceV1, ColorSurfaceData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_management_surface_v1::WpColorManagementSurfaceV1,
		request: wp_color_management_surface_v1::Request,
		data: &ColorSurfaceData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		if !data.surface.is_alive() {
			if !matches!(request, wp_color_management_surface_v1::Request::Destroy) {
				_resource.post_error(
					wp_color_management_surface_v1::Error::Inert,
					"Surface has been destroyed",
				);
			}
			return;
		}
		match request {
			wp_color_management_surface_v1::Request::Destroy => {
				if let Some(cm) = &mut state.color_management {
					cm.unset_pending(&data.surface);
					cm.surfaces.remove(&data.surface);
				}
			},

			wp_color_management_surface_v1::Request::SetImageDescription {
				image_description,
				render_intent,
			} => {
				if !matches!(
					render_intent.into_result(),
					Ok(wp_color_manager_v1::RenderIntent::Perceptual)
				) {
					_resource.post_error(
						wp_color_management_surface_v1::Error::RenderIntent,
						"Unsupported render intent",
					);
					return;
				}
				if let Some(desc_data) = image_description.data::<ImageDescriptionUserData>() {
					if !desc_data.valid {
						_resource.post_error(
							wp_color_management_surface_v1::Error::ImageDescription,
							"Image description is not ready",
						);
						return;
					}
					tracing::trace!(
						?desc_data.desc,
						"Surface set image description"
					);
					if let Some(cm) = &mut state.color_management {
						cm.set_pending(&data.surface, desc_data.desc);
					}
				}
			},

			wp_color_management_surface_v1::Request::UnsetImageDescription => {
				tracing::debug!("Surface unset image description");
				if let Some(cm) = &mut state.color_management {
					cm.unset_pending(&data.surface);
				}
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_creator_params_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1, CreatorParamsUserData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
		request: wp_image_description_creator_params_v1::Request,
		data: &CreatorParamsUserData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_image_description_creator_params_v1::Request::Create { image_description } => {
				let params = data.params.lock().unwrap();
				let (Some(transfer_function), Some(primaries)) = (params.transfer_function, params.primaries) else {
					data_init.init(
						image_description,
						ImageDescriptionUserData {
							desc: ImageDescription::srgb(),
							valid: false,
							information_allowed: false,
						},
					);
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::IncompleteSet,
						"Transfer function and primaries are required",
					);
					return;
				};
				let desc = ImageDescription {
					transfer_function,
					primaries,
					max_cll: params.max_cll,
					max_fall: params.max_fall,
					mastering_luminance: params.mastering_luminance,
					mastering_primaries: params.mastering_primaries,
					white_point: params.white_point,
				};
				if !supported_description(desc, params.luminances) {
					let resource = data_init.init(
						image_description,
						ImageDescriptionUserData {
							desc,
							valid: false,
							information_allowed: false,
						},
					);
					resource.failed(
						wp_image_description_v1::Cause::Unsupported,
						"Unsupported color encoding or luminance scale".to_string(),
					);
					return;
				}
				tracing::trace!(?desc, "Created parametric image description");

				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc,
						valid: true,
						information_allowed: false,
					},
				);
				// Signal that the image description is ready.
				send_ready(&resource);
			},

			wp_image_description_creator_params_v1::Request::SetTfNamed { tf } => {
				let tf = match tf.into_result() {
					Ok(wp_color_manager_v1::TransferFunction::St2084Pq) => TransferFunction::St2084Pq,
					Ok(wp_color_manager_v1::TransferFunction::Gamma22) => TransferFunction::Gamma22,
					Ok(wp_color_manager_v1::TransferFunction::Srgb) => TransferFunction::Srgb,
					Ok(wp_color_manager_v1::TransferFunction::ExtLinear) => TransferFunction::ScrgbLinear,
					_ => {
						_resource.post_error(
							wp_image_description_creator_params_v1::Error::InvalidTf,
							"Unsupported transfer function",
						);
						return;
					},
				};
				let mut params = data.params.lock().unwrap();
				if params.transfer_function.is_some() {
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::AlreadySet,
						"Parameter already set",
					);
					return;
				}
				params.transfer_function = Some(tf);
			},

			wp_image_description_creator_params_v1::Request::SetPrimariesNamed { primaries } => {
				let p = match primaries.into_result() {
					Ok(wp_color_manager_v1::Primaries::Bt2020) => Primaries::Bt2020,
					Ok(wp_color_manager_v1::Primaries::Srgb) => Primaries::Srgb,
					_ => {
						_resource.post_error(
							wp_image_description_creator_params_v1::Error::InvalidPrimariesNamed,
							"Unsupported primaries",
						);
						return;
					},
				};
				let mut params = data.params.lock().unwrap();
				if params.primaries.is_some() {
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::AlreadySet,
						"Parameter already set",
					);
					return;
				}
				params.primaries = Some(p);
			},

			wp_image_description_creator_params_v1::Request::SetMaxCll { max_cll } => {
				tracing::debug!(max_cll, "Set max content light level");
				let mut params = data.params.lock().unwrap();
				params.max_cll = Some(max_cll);
			},

			wp_image_description_creator_params_v1::Request::SetMaxFall { max_fall } => {
				tracing::debug!(max_fall, "Set max frame-average light level");
				let mut params = data.params.lock().unwrap();
				params.max_fall = Some(max_fall);
			},

			wp_image_description_creator_params_v1::Request::SetMasteringLuminance { min_lum, max_lum } => {
				if u64::from(max_lum) * 10000 <= u64::from(min_lum) {
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::InvalidLuminance,
						"Invalid mastering luminance range",
					);
					return;
				}
				tracing::debug!(min_lum, max_lum, "Set mastering luminance");
				// min_lum is in 0.0001 cd/m² units; max_lum is in 1 cd/m² units.
				// Normalize both to 0.0001 cd/m² units.
				let mut params = data.params.lock().unwrap();
				if params.mastering_luminance.is_some() {
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::AlreadySet,
						"Parameter already set",
					);
					return;
				}
				params.mastering_luminance = Some((min_lum, max_lum.saturating_mul(10000)));
			},

			wp_image_description_creator_params_v1::Request::SetMasteringDisplayPrimaries {
				r_x,
				r_y,
				g_x,
				g_y,
				b_x,
				b_y,
				w_x,
				w_y,
			} => {
				// Protocol sends primaries as 1/1,000,000 chromaticity (i32).
				// Convert to 0.00002 units (divide by 20) to match CTA-861.G format.
				let to_cta = |v: i32| -> u32 { (v.max(0) as u32) / 20 };
				let mut params = data.params.lock().unwrap();
				if params.mastering_primaries.is_some() {
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::AlreadySet,
						"Mastering primaries already set",
					);
					return;
				}
				params.mastering_primaries = Some([
					(to_cta(r_x), to_cta(r_y)),
					(to_cta(g_x), to_cta(g_y)),
					(to_cta(b_x), to_cta(b_y)),
				]);
				params.white_point = Some((to_cta(w_x), to_cta(w_y)));
				tracing::debug!(
					r_x,
					r_y,
					g_x,
					g_y,
					b_x,
					b_y,
					w_x,
					w_y,
					"Set mastering display primaries"
				);
			},

			wp_image_description_creator_params_v1::Request::SetLuminances {
				min_lum,
				max_lum,
				reference_lum,
			} => {
				if u64::from(max_lum) * 10000 <= u64::from(min_lum)
					|| u64::from(reference_lum) * 10000 <= u64::from(min_lum)
				{
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::InvalidLuminance,
						"Invalid luminance range",
					);
					return;
				}
				let mut params = data.params.lock().unwrap();
				if params.luminances.replace((min_lum, max_lum, reference_lum)).is_some() {
					_resource.post_error(
						wp_image_description_creator_params_v1::Error::AlreadySet,
						"Luminances already set",
					);
				}
			},
			wp_image_description_creator_params_v1::Request::SetTfPower { .. }
			| wp_image_description_creator_params_v1::Request::SetPrimaries { .. } => {
				_resource.post_error(
					wp_image_description_creator_params_v1::Error::UnsupportedFeature,
					"Only advertised named encodings are supported",
				);
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_v1::WpImageDescriptionV1, ImageDescriptionUserData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_v1::WpImageDescriptionV1,
		request: wp_image_description_v1::Request,
		data: &ImageDescriptionUserData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_image_description_v1::Request::Destroy => {},

			wp_image_description_v1::Request::GetInformation { information } => {
				let info = data_init.init(information, ImageDescriptionInfoData);
				if !data.information_allowed {
					_resource.post_error(
						wp_image_description_v1::Error::NoInformation,
						"This description does not allow information queries",
					);
					return;
				}
				tracing::debug!(?data.desc, "GetInformation: sending image description info");

				// Send parametric description events.
				match data.desc.primaries {
					Primaries::Srgb => info.primaries_named(wp_color_manager_v1::Primaries::Srgb),
					Primaries::Bt2020 => info.primaries_named(wp_color_manager_v1::Primaries::Bt2020),
				}
				match data.desc.transfer_function {
					TransferFunction::Srgb | TransferFunction::Gamma22 => {
						info.tf_named(if data.desc.transfer_function == TransferFunction::Srgb {
							wp_color_manager_v1::TransferFunction::Srgb
						} else {
							wp_color_manager_v1::TransferFunction::Gamma22
						});
						// sRGB: 0.2–80 cd/m², reference white 80 cd/m².
						info.luminances(2000, 80, 80);
						info.target_luminance(2000, 80);
					},
					TransferFunction::St2084Pq => {
						info.tf_named(wp_color_manager_v1::TransferFunction::St2084Pq);
						// PQ: 0–10000 cd/m², SDR reference white 203 cd/m².
						info.luminances(0, 10000, 203);
						info.target_luminance(0, 10000);
					},
					TransferFunction::ScrgbLinear => {
						info.tf_named(wp_color_manager_v1::TransferFunction::ExtLinear);
						// scRGB: linear with extended range, 1.0 == 80 cd/m² (IEC 61966-2-2).
						info.luminances(0, 80, 80);
						info.target_luminance(0, 10000);
					},
				}

				// done() is a destructor event that removes the child object from
				// the backend's map. Calling it here would panic because the
				// backend tries to set user_data on the deleted object after this
				// handler returns. Defer to after dispatch_clients.
				state.deferred_info_done.push(info);
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_info_v1 — no client requests (events only)
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_info_v1::WpImageDescriptionInfoV1, ImageDescriptionInfoData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_info_v1::WpImageDescriptionInfoV1,
		_request: wp_image_description_info_v1::Request,
		_data: &ImageDescriptionInfoData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		// wp_image_description_info_v1 has no client requests.
	}
}

// ---------------------------------------------------------------------------
// wp_color_management_output_v1 — minimal implementation
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_management_output_v1::WpColorManagementOutputV1, ColorOutputData> for MoonshineCompositor {
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_management_output_v1::WpColorManagementOutputV1,
		request: wp_color_management_output_v1::Request,
		_data: &ColorOutputData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_management_output_v1::Request::Destroy => {
				if let Some(cm) = &mut state.color_management {
					cm.outputs.retain(|r| r != _resource);
				}
			},

			wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
				// Return a BT.2020+PQ description if HDR is active, otherwise sRGB.
				let desc = if state.color_management.as_ref().is_some_and(|cm| cm.hdr) {
					ImageDescription::bt2020_pq()
				} else {
					ImageDescription::srgb()
				};
				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc,
						valid: true,
						information_allowed: true,
					},
				);
				let identity = if desc.to_frame_color_space() == FrameColorSpace::Bt2020Pq {
					2
				} else {
					1
				};
				if resource.version() >= 2 {
					resource.ready2(0, identity);
				} else {
					resource.ready(identity);
				}
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_management_surface_feedback_v1 — minimal implementation
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1, ColorSurfaceFeedbackData>
	for MoonshineCompositor
{
	fn request(
		state: &mut Self,
		_client: &Client,
		_resource: &wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
		request: wp_color_management_surface_feedback_v1::Request,
		data: &ColorSurfaceFeedbackData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_management_surface_feedback_v1::Request::Destroy => {
				if let Some(cm) = &mut state.color_management {
					cm.feedback.retain(|r| r != _resource);
				}
			},

			wp_color_management_surface_feedback_v1::Request::GetPreferred { image_description }
			| wp_color_management_surface_feedback_v1::Request::GetPreferredParametric { image_description } => {
				if !data._surface.is_alive() {
					data_init.init(
						image_description,
						ImageDescriptionUserData {
							desc: ImageDescription::srgb(),
							valid: false,
							information_allowed: false,
						},
					);
					_resource.post_error(
						wp_color_management_surface_feedback_v1::Error::Inert,
						"Surface has been destroyed",
					);
					return;
				}
				let desc = if state.color_management.as_ref().is_some_and(|cm| cm.hdr) {
					ImageDescription::bt2020_pq()
				} else {
					ImageDescription::srgb()
				};
				tracing::debug!(?desc, "GetPreferred: returning image description");
				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc,
						valid: true,
						information_allowed: true,
					},
				);
				let identity = if desc.to_frame_color_space() == FrameColorSpace::Bt2020Pq {
					2
				} else {
					1
				};
				if resource.version() >= 2 {
					resource.ready2(0, identity);
				} else {
					resource.ready(identity);
				}
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_image_description_creator_icc_v1 — stub (not supported)
// ---------------------------------------------------------------------------

impl Dispatch<wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1, IccCreatorData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
		request: wp_image_description_creator_icc_v1::Request,
		_data: &IccCreatorData,
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_image_description_creator_icc_v1::Request::Create { image_description } => {
				// ICC profiles are not supported. Create a default sRGB description
				// and signal failure with the `failed` event.
				let resource = data_init.init(
					image_description,
					ImageDescriptionUserData {
						desc: ImageDescription::srgb(),
						valid: false,
						information_allowed: false,
					},
				);
				resource.failed(
					wp_image_description_v1::Cause::Unsupported,
					"ICC profiles are not supported".to_string(),
				);
			},

			_ => {
				// set_icc_file — accept but ignore.
			},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_representation_manager_v1 — Global
// ---------------------------------------------------------------------------

impl GlobalDispatch<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, ()> for MoonshineCompositor {
	fn bind(
		_state: &mut Self,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<wp_color_representation_manager_v1::WpColorRepresentationManagerV1>,
		_global_data: &(),
		data_init: &mut DataInit<'_, Self>,
	) {
		let resource = data_init.init(resource, ());

		// Advertise supported alpha modes.
		resource.supported_alpha_mode(wp_color_representation_surface_v1::AlphaMode::PremultipliedElectrical);

		// Advertise identity coefficients (RGB) with full range.
		resource.supported_coefficients_and_ranges(
			wp_color_representation_surface_v1::Coefficients::Identity,
			wp_color_representation_surface_v1::Range::Full,
		);

		resource.done();
	}
}

impl Dispatch<wp_color_representation_manager_v1::WpColorRepresentationManagerV1, ()> for MoonshineCompositor {
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
		request: wp_color_representation_manager_v1::Request,
		_data: &(),
		_dhandle: &DisplayHandle,
		data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_representation_manager_v1::Request::Destroy => {},

			wp_color_representation_manager_v1::Request::GetSurface { id, surface } => {
				data_init.init(id, ColorRepresentationSurfaceData { surface });
			},

			_ => {},
		}
	}
}

// ---------------------------------------------------------------------------
// wp_color_representation_surface_v1
// ---------------------------------------------------------------------------

impl Dispatch<wp_color_representation_surface_v1::WpColorRepresentationSurfaceV1, ColorRepresentationSurfaceData>
	for MoonshineCompositor
{
	fn request(
		_state: &mut Self,
		_client: &Client,
		_resource: &wp_color_representation_surface_v1::WpColorRepresentationSurfaceV1,
		request: wp_color_representation_surface_v1::Request,
		_data: &ColorRepresentationSurfaceData,
		_dhandle: &DisplayHandle,
		_data_init: &mut DataInit<'_, Self>,
	) {
		match request {
			wp_color_representation_surface_v1::Request::Destroy => {},

			wp_color_representation_surface_v1::Request::SetAlphaMode { alpha_mode } => {
				tracing::debug!(?alpha_mode, "Surface set alpha mode");
				if !matches!(
					alpha_mode.into_result(),
					Ok(wp_color_representation_surface_v1::AlphaMode::PremultipliedElectrical)
				) {
					_resource.post_error(
						wp_color_representation_surface_v1::Error::AlphaMode,
						"Only premultiplied electrical alpha is supported",
					);
				}
			},

			wp_color_representation_surface_v1::Request::SetCoefficientsAndRange { coefficients, range } => {
				tracing::debug!(?coefficients, ?range, "Surface set coefficients and range");
				if !matches!(
					coefficients.into_result(),
					Ok(wp_color_representation_surface_v1::Coefficients::Identity)
				) || !matches!(range.into_result(), Ok(wp_color_representation_surface_v1::Range::Full))
				{
					_resource.post_error(
						wp_color_representation_surface_v1::Error::Coefficients,
						"Only identity/full-range RGB is supported",
					);
				}
			},

			wp_color_representation_surface_v1::Request::SetChromaLocation { chroma_location } => {
				tracing::debug!(?chroma_location, "Surface set chroma location");
				// Accept but don't act — passthrough.
			},

			_ => {},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
	use std::{os::unix::net::UnixStream, sync::Arc};
	use wayland_client::{
		Connection, QueueHandle,
		protocol::{wl_compositor, wl_registry, wl_subcompositor, wl_subsurface, wl_surface},
	};
	struct Server {
		compositor: CompositorState,
		surfaces: Vec<WlSurface>,
	}
	struct ClientData {
		compositor: CompositorClientState,
	}
	impl smithay::reexports::wayland_server::backend::ClientData for ClientData {}
	impl CompositorHandler for Server {
		fn compositor_state(&mut self) -> &mut CompositorState {
			&mut self.compositor
		}
		fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
			&client.get_data::<ClientData>().unwrap().compositor
		}
		fn new_surface(&mut self, surface: &WlSurface) {
			self.surfaces.push(surface.clone());
		}
		fn commit(&mut self, _: &WlSurface) {}
	}
	smithay::delegate_dispatch2!(Server);
	#[derive(Default)]
	struct Peer {
		compositor_name: u32,
		subcompositor_name: u32,
		color_manager_name: u32,
	}
	impl wayland_client::Dispatch<wl_registry::WlRegistry, ()> for Peer {
		fn event(
			state: &mut Self,
			_: &wl_registry::WlRegistry,
			event: wl_registry::Event,
			_: &(),
			_: &Connection,
			_: &QueueHandle<Self>,
		) {
			if let wl_registry::Event::Global { name, interface, .. } = event {
				match interface.as_str() {
					"wl_compositor" => state.compositor_name = name,
					"wl_subcompositor" => state.subcompositor_name = name,
					"wp_color_manager_v1" => state.color_manager_name = name,
					_ => {},
				}
			}
		}
	}
	wayland_client::delegate_noop!(Peer: ignore wl_compositor::WlCompositor);
	wayland_client::delegate_noop!(Peer: ignore wl_surface::WlSurface);
	wayland_client::delegate_noop!(Peer: ignore wl_subcompositor::WlSubcompositor);
	wayland_client::delegate_noop!(Peer: ignore wl_subsurface::WlSubsurface);
	use wayland_protocols::wp::color_management::v1::client as native;
	wayland_client::delegate_noop!(Peer: ignore native::wp_color_manager_v1::WpColorManagerV1);
	wayland_client::delegate_noop!(Peer: ignore native::wp_color_management_surface_v1::WpColorManagementSurfaceV1);
	wayland_client::delegate_noop!(Peer: ignore native::wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1);
	wayland_client::delegate_noop!(Peer: ignore native::wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1);
	wayland_client::delegate_noop!(Peer: ignore native::wp_image_description_v1::WpImageDescriptionV1);
	wayland_client::delegate_noop!(Peer: ignore native::wp_image_description_info_v1::WpImageDescriptionInfoV1);

	#[test]
	#[ignore = "needs a GPU render node and Xwayland"]
	fn invalid_native_color_constructors_disconnect_only_the_offending_client() {
		use super::super::{Compositor, CompositorConfig, CompositorContext};
		use crate::session::manager::SessionShutdownReason;
		let stop = async_shutdown::ShutdownManager::new();
		let (foreground, _) = tokio::sync::watch::channel(None);
		let (compositor, _handles) = Compositor::new(
			CompositorConfig::default(),
			CompositorContext {
				width: 1280,
				height: 720,
				refresh_rate: 60,
				hdr: false,
				output_scale: 1.0,
				log_stats: false,
			},
			stop.clone(),
			foreground,
		);
		let launched = compositor.launch().unwrap();
		let socket_path = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
			.join(&launched.ready().wayland_display);
		for case in 0..4 {
			let connection = Connection::from_socket(UnixStream::connect(&socket_path).unwrap()).unwrap();
			let mut queue = connection.new_event_queue::<Peer>();
			let qh = queue.handle();
			let registry = connection.display().get_registry(&qh, ());
			let mut peer = Peer::default();
			queue.roundtrip(&mut peer).unwrap();
			let manager: native::wp_color_manager_v1::WpColorManagerV1 =
				registry.bind(peer.color_manager_name, 3, &qh, ());
			let compositor: wl_compositor::WlCompositor = registry.bind(peer.compositor_name, 6, &qh, ());
			let surface = compositor.create_surface(&qh, ());
			match case {
				0 => {
					let _a = manager.get_surface(&surface, &qh, ());
					let _b = manager.get_surface(&surface, &qh, ());
				},
				1 => {
					let creator = manager.create_parametric_creator(&qh, ());
					let _desc = creator.create(&qh, ());
				},
				2 => {
					let _icc = manager.create_icc_creator(&qh, ());
				},
				3 => {
					let desc = manager.create_windows_scrgb(&qh, ());
					queue.roundtrip(&mut peer).unwrap();
					let _info = desc.get_information(&qh, ());
				},
				_ => unreachable!(),
			}
			assert!(
				queue.roundtrip(&mut peer).is_err(),
				"case {case} did not reject invalid protocol use"
			);
		}
		// A fresh, valid client must still be served after every invalid one.
		let connection = Connection::from_socket(UnixStream::connect(socket_path).unwrap()).unwrap();
		let mut queue = connection.new_event_queue::<Peer>();
		let _registry = connection.display().get_registry(&queue.handle(), ());
		queue.roundtrip(&mut Peer::default()).unwrap();
		let _ = stop.trigger_shutdown(SessionShutdownReason::UserStopped);
		tokio::runtime::Builder::new_current_thread()
			.enable_time()
			.build()
			.unwrap()
			.block_on(async {
				tokio::time::timeout(std::time::Duration::from_secs(10), stop.wait_shutdown_complete())
					.await
					.unwrap();
			});
	}

	struct Harness {
		display: smithay::reexports::wayland_server::Display<Server>,
		server: Server,
		connection: Connection,
		a: wl_surface::WlSurface,
		b: wl_surface::WlSurface,
		subcompositor: wl_subcompositor::WlSubcompositor,
		qh: QueueHandle<Peer>,
	}
	impl Harness {
		fn new() -> Self {
			let mut display = smithay::reexports::wayland_server::Display::<Server>::new().unwrap();
			let mut handle = display.handle();
			let compositor = CompositorState::new_v6::<Server>(&handle);
			let mut server = Server {
				compositor,
				surfaces: Vec::new(),
			};
			let (socket, peer) = UnixStream::pair().unwrap();
			handle
				.insert_client(
					socket,
					Arc::new(ClientData {
						compositor: CompositorClientState::default(),
					}),
				)
				.unwrap();
			let connection = Connection::from_socket(peer).unwrap();
			let mut queue = connection.new_event_queue::<Peer>();
			let qh = queue.handle();
			let registry = connection.display().get_registry(&qh, ());
			connection.flush().unwrap();
			display.dispatch_clients(&mut server).unwrap();
			handle.flush_clients().unwrap();
			queue.prepare_read().unwrap().read().unwrap();
			let mut state = Peer::default();
			queue.dispatch_pending(&mut state).unwrap();
			let compositor: wl_compositor::WlCompositor = registry.bind(state.compositor_name, 6, &qh, ());
			let subcompositor = registry.bind(state.subcompositor_name, 1, &qh, ());
			let a = compositor.create_surface(&qh, ());
			let b = compositor.create_surface(&qh, ());
			connection.flush().unwrap();
			display.dispatch_clients(&mut server).unwrap();
			assert_eq!(server.surfaces.len(), 2);
			Self {
				display,
				server,
				connection,
				a,
				b,
				subcompositor,
				qh,
			}
		}
		fn commit(&mut self, index: usize) {
			if index == 0 {
				self.a.commit();
			} else {
				self.b.commit();
			}
			self.dispatch();
		}
		fn dispatch(&mut self) {
			self.connection.flush().unwrap();
			self.display.dispatch_clients(&mut self.server).unwrap();
		}
	}
	fn colors() -> ColorManagementState {
		ColorManagementState {
			declared: HashMap::new(),
			observed: HashMap::new(),
			surfaces: HashMap::new(),
			outputs: Vec::new(),
			feedback: Vec::new(),
			hdr: true,
		}
	}
	#[test]
	fn native_surface_color_lifecycle_and_metadata_isolation() {
		let mut h = Harness::new();
		let a = h.server.surfaces[0].clone();
		let b = h.server.surfaces[1].clone();
		let mut cm = colors();
		assert_eq!(cm.surface_color_space(&a), FrameColorSpace::Srgb);
		let mut pq = ImageDescription::bt2020_pq();
		pq.max_cll = Some(2000);
		pq.max_fall = Some(500);
		pq.mastering_luminance = Some((10, 12_000_000));
		pq.mastering_primaries = Some([(35400, 14600), (8500, 39850), (6550, 2300)]);
		pq.white_point = Some((15635, 16450));
		cm.set_pending(&a, pq);
		assert_eq!(cm.surface_color_space(&a), FrameColorSpace::Srgb);
		h.commit(0);
		assert_eq!(cm.surface_color_space(&a), FrameColorSpace::Bt2020Pq);
		let metadata = cm.surface_hdr_metadata(&a).unwrap();
		assert_eq!(metadata.max_cll, 2000);
		assert_eq!(metadata.max_fall, 500);
		assert_eq!(metadata.max_luminance, 12_000_000);
		assert_eq!(metadata.min_luminance, 10);
		assert_eq!(
			metadata.display_primaries,
			[(35400, 14600), (8500, 39850), (6550, 2300)]
		);
		assert_eq!(metadata.white_point, (15635, 16450));
		assert_eq!(cm.surface_color_space(&b), FrameColorSpace::Srgb);
		assert!(cm.surface_hdr_metadata(&b).is_none());
		cm.set_pending(&b, ImageDescription::scrgb_linear());
		h.commit(1);
		assert_eq!(cm.surface_color_space(&b), FrameColorSpace::ScrgbLinear);
		assert!(cm.surface_hdr_metadata(&b).is_none());
		cm.set_pending(&a, ImageDescription::srgb());
		h.commit(0);
		assert_eq!(cm.surface_color_space(&a), FrameColorSpace::Srgb);
		assert!(cm.surface_hdr_metadata(&a).is_none());
		cm.set_pending(&a, pq);
		h.commit(0);
		assert_eq!(cm.surface_color_space(&a), FrameColorSpace::Bt2020Pq);
		cm.unset_pending(&a);
		h.commit(0);
		assert_eq!(cm.surface_color_space(&a), FrameColorSpace::Srgb);
		cm.set_pending(&a, pq);
		h.commit(0);
		cm.surface_destroyed(&a);
		h.a.destroy();
		h.dispatch();
		assert!(
			cm.declared
				.keys()
				.all(|id| id != &smithay::backend::renderer::element::Id::from_wayland_resource(&a))
		);
		assert_eq!(cm.surface_color_space(&b), FrameColorSpace::ScrgbLinear);
	}
	#[test]
	fn synchronized_subsurface_color_follows_its_parent_transaction() {
		let mut h = Harness::new();
		let child = h.server.surfaces[1].clone();
		let _subsurface = h.subcompositor.get_subsurface(&h.b, &h.a, &h.qh, ());
		h.dispatch();
		let mut cm = colors();
		cm.set_pending(&child, ImageDescription::scrgb_linear());
		h.commit(1);
		assert_eq!(cm.surface_color_space(&child), FrameColorSpace::Srgb);
		h.commit(0);
		assert_eq!(cm.surface_color_space(&child), FrameColorSpace::ScrgbLinear);
		let id = smithay::backend::renderer::element::Id::from_wayland_resource(&child);
		assert_eq!(cm.hdr_element_surface(&id, &h.server.surfaces[0]), Some(child.clone()));
		assert!(cm.hdr_element_surface(&id, &child).is_some());
		cm.unset_pending(&child);
		h.commit(1);
		assert_eq!(cm.surface_color_space(&child), FrameColorSpace::ScrgbLinear);
		h.commit(0);
		assert_eq!(cm.surface_color_space(&child), FrameColorSpace::Srgb);
	}

	#[test]
	fn partial_scrgb_metadata_keeps_reference_white_distinct_from_mastering_peak() {
		let desc = ImageDescription {
			max_cll: Some(2000),
			max_fall: Some(500),
			..ImageDescription::scrgb_linear()
		};
		let metadata = ColorManagementState::description_hdr_metadata(desc).unwrap();
		assert_eq!(metadata.max_cll, 2000);
		assert_eq!(metadata.max_fall, 500);
		assert_eq!(metadata.max_luminance, HdrMetadata::fallback().max_luminance);
		assert_ne!(metadata.max_luminance, 80 * 10000);
	}

	#[test]
	fn supported_encodings_and_luminance_scales() {
		assert!(supported_description(ImageDescription::srgb(), None));
		assert!(supported_description(
			ImageDescription::bt2020_pq(),
			Some((50, 10000, 203))
		));
		assert!(supported_description(
			ImageDescription::scrgb_linear(),
			Some((0, 80, 80))
		));
		assert!(!supported_description(
			ImageDescription::scrgb_linear(),
			Some((0, 1000, 203))
		));
		assert!(!supported_description(
			ImageDescription {
				primaries: Primaries::Bt2020,
				..ImageDescription::srgb()
			},
			None
		));
	}
}

//! Capture/encode identity contract. Select and independently verify the capture GPU;
//! never infer identity from enumeration order, vendor IDs, or device names.
use ash::vk;
use ash::vk::TaggedStructure;
use pixelforge::{DeviceSelector, VideoContext, VideoContextBuilder};
use std::ffi::CStr;
use std::fs::File;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureIdentity {
	render: (i64, i64),
	pci: Option<(u32, u32, u32, u32)>,
}

impl CaptureIdentity {
	fn from_file(file: &File) -> Result<Self, String> {
		let metadata = file
			.metadata()
			.map_err(|e| format!("Cannot stat capture DRM device: {e}"))?;
		if !metadata.file_type().is_char_device() {
			return Err("Capture render node must be a DRM character device".into());
		}
		let render = (libc::major(metadata.rdev()) as i64, libc::minor(metadata.rdev()) as i64);
		// Resolve by device number, so /dev/dri/by-path and custom symlinks work.
		let pci = std::fs::canonicalize(format!("/sys/dev/char/{}:{}/device", render.0, render.1))
			.ok()
			.and_then(|p| p.file_name()?.to_str().and_then(parse_pci));
		Ok(Self { render, pci })
	}

	fn matches(&self, drm: Option<(i64, i64)>, pci: Option<(u32, u32, u32, u32)>) -> bool {
		// DRM is authoritative if advertised. PCI is a fallback for drivers
		// lacking VK_EXT_physical_device_drm, never a vendor/device-ID guess.
		match drm {
			Some(render) => self.render == render,
			None => self.pci.is_some() && self.pci == pci,
		}
	}
}

fn parse_pci(address: &str) -> Option<(u32, u32, u32, u32)> {
	let fields: Vec<_> = address.split([':', '.']).collect();
	if fields.len() != 4 {
		return None;
	}
	Some((
		u32::from_str_radix(fields[0], 16).ok()?,
		u32::from_str_radix(fields[1], 16).ok()?,
		u32::from_str_radix(fields[2], 16).ok()?,
		u32::from_str_radix(fields[3], 16).ok()?,
	))
}

/// Build and verify against the *opened* DRM node used for GBM/EGL. All
/// encoder/import contexts in a session clone this verified context.
///
/// The context does not require Vulkan Video encode: a capture GPU without it
/// (Intel Xe2 and newer, where ANV exposes no encode, or any driver without
/// the extensions) still captures, converts and runs PyroWave, which encodes
/// with compute. Conventional profiles are then absent from the probe, so
/// they are never advertised; [`encode_status`] says why.
pub(crate) fn capture_context(file: &File) -> Result<VideoContext, String> {
	let capture = CaptureIdentity::from_file(file)?;
	let context = VideoContextBuilder::new()
		.app_name("Pyroshine")
		.device_selector(DeviceSelector {
			drm_render_node: capture.render,
			pci_bus: capture.pci,
		})
		.allow_without_encode(true)
		.build()
		.map_err(|e| {
			format!(
				"Failed to initialize Vulkan on capture DRM {:?} (PCI {:?}): {e}",
				capture.render, capture.pci
			)
		})?;
	let extensions = unsafe {
		context
			.instance()
			.enumerate_device_extension_properties(context.physical_device())
	}
	.map_err(|e| format!("Cannot query Vulkan device identity extensions: {e}"))?;
	let has = |name: &CStr| {
		extensions
			.iter()
			.any(|ext| unsafe { CStr::from_ptr(ext.extension_name.as_ptr()) } == name)
	};
	let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
	let mut pci = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
	let mut ids = vk::PhysicalDeviceIDProperties::default();
	let mut props = vk::PhysicalDeviceProperties2::default().push(&mut ids);
	if has(ash::ext::physical_device_drm::NAME) {
		props = props.push(&mut drm);
	}
	if has(ash::ext::pci_bus_info::NAME) {
		props = props.push(&mut pci);
	}
	// SAFETY: supported property structures and the live context remain valid.
	unsafe {
		context
			.instance()
			.get_physical_device_properties2(context.physical_device(), &mut props);
	}
	let name = unsafe { CStr::from_ptr(props.properties.device_name.as_ptr()) }.to_string_lossy();
	let drm_id = (drm.has_render != 0).then_some((drm.render_major, drm.render_minor));
	let pci_id =
		has(ash::ext::pci_bus_info::NAME).then_some((pci.pci_domain, pci.pci_bus, pci.pci_device, pci.pci_function));
	if !capture.matches(drm_id, pci_id) {
		return Err(format!(
			"Capture DRM {}:{} (PCI {:?}) does not match or cannot be verified against Vulkan '{name}' (DRM {drm_id:?}, PCI {pci_id:?}). Cross-device capture/encoding is not validated. Ensure compositor.gpu or MOONSHINE_RENDER_NODE identifies a render node whose Vulkan driver exposes DRM or PCI identity. No cross-GPU fallback is attempted.",
			capture.render.0, capture.render.1, capture.pci
		));
	}
	tracing::info!(capture_drm = ?capture.render, pci = ?capture.pci, device_uuid = ?ids.device_uuid, gpu = %name,
		vulkan_video_encode = %encode_status(&context),
		"Verified capture/encode/import GPU identity (same device)");
	Ok(context)
}

/// The DRM render node of `context`'s physical device, for APIs that open
/// the GPU by node (VA-API). `None` when the driver does not report DRM
/// identity (`VK_EXT_physical_device_drm`).
pub(crate) fn render_node(context: &VideoContext) -> Option<std::path::PathBuf> {
	let has_drm = unsafe {
		context
			.instance()
			.enumerate_device_extension_properties(context.physical_device())
	}
	.ok()?
	.iter()
	.any(|ext| unsafe { CStr::from_ptr(ext.extension_name.as_ptr()) } == ash::ext::physical_device_drm::NAME);
	if !has_drm {
		return None;
	}
	let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
	let mut props = vk::PhysicalDeviceProperties2::default().push(&mut drm);
	// SAFETY: the extension is supported and the structures live for the call.
	unsafe {
		context
			.instance()
			.get_physical_device_properties2(context.physical_device(), &mut props);
	}
	if drm.has_render == 0 {
		return None;
	}
	// Resolve the node by device number, not by assuming /dev/dri naming.
	let dir = format!("/sys/dev/char/{}:{}", drm.render_major, drm.render_minor);
	let name = std::fs::read_link(dir).ok()?.file_name()?.to_owned();
	Some(std::path::Path::new("/dev/dri").join(name))
}

const ENCODE_CODECS: [(pixelforge::Codec, &str); 3] = [
	(pixelforge::Codec::H264, "H.264"),
	(pixelforge::Codec::H265, "HEVC"),
	(pixelforge::Codec::AV1, "AV1"),
];

/// One line on the context's Vulkan Video encode support: the codecs it can
/// encode, or why none (identical reasons are reported once).
pub(crate) fn encode_status(context: &VideoContext) -> String {
	let supported: Vec<_> = ENCODE_CODECS
		.iter()
		.filter(|(codec, _)| context.supports_encode(*codec))
		.map(|(_, label)| *label)
		.collect();
	let mut unavailable: Vec<(String, Vec<&str>)> = Vec::new();
	for (codec, label) in ENCODE_CODECS {
		let Some(reason) = context.encode_unsupported_reason(codec) else {
			continue;
		};
		match unavailable.iter_mut().find(|(r, _)| r == reason) {
			Some((_, labels)) => labels.push(label),
			None => unavailable.push((reason.to_string(), vec![label])),
		}
	}
	let unavailable = unavailable
		.iter()
		.map(|(reason, labels)| format!("{}: {reason}", labels.join(", ")))
		.collect::<Vec<_>>()
		.join("; ");
	match (supported.is_empty(), unavailable.is_empty()) {
		(true, _) => format!("unavailable ({unavailable})"),
		(false, true) => supported.join(", "),
		(false, false) => format!("{} (not {unavailable})", supported.join(", ")),
	}
}

/// Vulkan format that samples a single-plane DRM fourcc with its channels in
/// RGBA order: the compositor formats both encoder families read directly.
///
/// DRM codes name channels from the most significant bits; Vulkan packed
/// formats likewise, byte formats from the lowest address. X-variants share
/// the layout of their A-counterparts (the alpha is never read).
pub(crate) fn sampled_vk_format(fourcc: u32) -> Option<vk::Format> {
	match fourcc {
		0x3432_4241 | 0x3432_4258 => Some(vk::Format::R8G8B8A8_UNORM), // ABGR/XBGR8888
		0x3432_5241 | 0x3432_5258 => Some(vk::Format::B8G8R8A8_UNORM), // ARGB/XRGB8888
		0x3033_4241 | 0x3033_4258 => Some(vk::Format::A2B10G10R10_UNORM_PACK32), // ABGR/XBGR2101010
		0x3033_5241 | 0x3033_5258 => Some(vk::Format::A2R10G10B10_UNORM_PACK32), // ARGB/XRGB2101010
		0x4834_4241 | 0x4834_4258 => Some(vk::Format::R16G16B16A16_SFLOAT), // ABGR/XBGR16161616F
		_ => None,
	}
}

/// Every fourcc [`sampled_vk_format`] maps.
const SAMPLED_FOURCCS: [u32; 10] = [
	0x3432_4241,
	0x3432_4258,
	0x3432_5241,
	0x3432_5258,
	0x3033_4241,
	0x3033_4258,
	0x3033_5241,
	0x3033_5258,
	0x4834_4241,
	0x4834_4258,
];

/// Image usage both encoder families request for an imported source.
const IMPORT_USAGE: vk::ImageUsageFlags =
	vk::ImageUsageFlags::from_raw(vk::ImageUsageFlags::SAMPLED.as_raw() | vk::ImageUsageFlags::TRANSFER_SRC.as_raw());

/// Single-plane (fourcc, DRM modifier) pairs the capture device imports as a
/// DMA-BUF image with the encoders' usage.
///
/// A GL renderer can render to modifiers the same GPU's Vulkan driver cannot
/// import: NVIDIA's EGL also offers its compressed block-linear kinds
/// (`0x0300000000e080xx`) and the implicit `DRM_FORMAT_MOD_INVALID`, which
/// `vkGetPhysicalDeviceImageFormatProperties2` rejects. PyroWave checks that
/// before importing and fails on every frame; creating the image anyway (as
/// NVIDIA tolerates) is invalid usage. Capture buffers, direct exports and
/// DMA-BUF overlay layers therefore use only pairs listed here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportableModifiers(Vec<(u32, u64)>);

impl ImportableModifiers {
	/// Query every sampled format's modifiers on `context`'s physical device.
	pub(crate) fn query(context: &VideoContext) -> Self {
		let instance = context.instance();
		let gpu = context.physical_device();
		let mut pairs = Vec::new();
		for fourcc in SAMPLED_FOURCCS {
			let Some(format) = sampled_vk_format(fourcc) else {
				continue;
			};
			let mut count = vk::DrmFormatModifierPropertiesListEXT::default();
			let mut properties = vk::FormatProperties2::default().push(&mut count);
			// SAFETY: plain physical-device query.
			unsafe { instance.get_physical_device_format_properties2(gpu, format, &mut properties) };
			let mut modifiers =
				vec![vk::DrmFormatModifierPropertiesEXT::default(); count.drm_format_modifier_count as usize];
			let mut list =
				vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut modifiers);
			let mut properties = vk::FormatProperties2::default().push(&mut list);
			// SAFETY: as above, with an array of the reported size.
			unsafe { instance.get_physical_device_format_properties2(gpu, format, &mut properties) };
			let required = vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_SRC;
			for modifier in modifiers.iter().filter(|m| {
				m.drm_format_modifier_plane_count == 1 && m.drm_format_modifier_tiling_features.contains(required)
			}) {
				let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
					.drm_format_modifier(modifier.drm_format_modifier)
					.sharing_mode(vk::SharingMode::EXCLUSIVE);
				let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
					.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
				let info = vk::PhysicalDeviceImageFormatInfo2::default()
					.format(format)
					.ty(vk::ImageType::TYPE_2D)
					.tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
					.usage(IMPORT_USAGE)
					.push(&mut external_info)
					.push(&mut modifier_info);
				let mut external = vk::ExternalImageFormatProperties::default();
				let mut result = vk::ImageFormatProperties2::default().push(&mut external);
				// SAFETY: valid input and output chains that live for the call.
				let supported =
					unsafe { instance.get_physical_device_image_format_properties2(gpu, &info, &mut result) };
				if supported.is_ok()
					&& external
						.external_memory_properties
						.external_memory_features
						.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
				{
					pairs.push((fourcc, modifier.drm_format_modifier));
				}
			}
		}
		Self(pairs)
	}

	pub(crate) fn contains(&self, fourcc: u32, modifier: u64) -> bool {
		self.0.contains(&(fourcc, modifier))
	}

	pub(crate) fn len(&self) -> usize {
		self.0.len()
	}

	#[cfg(test)]
	pub(crate) fn from_pairs(pairs: Vec<(u32, u64)>) -> Self {
		Self(pairs)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	#[ignore = "needs a Vulkan Video GPU: MOONSHINE_TEST_GPU=1"]
	fn capture_device_selection_on_gpu() {
		assert!(
			std::env::var_os("MOONSHINE_TEST_GPU").is_some(),
			"set MOONSHINE_TEST_GPU=1"
		);
		let path = std::env::var("MOONSHINE_TEST_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
		let file = File::open(path).unwrap();
		// capture_context independently queries and verifies the selected GPU.
		let context = capture_context(&file).unwrap();
		assert!(
			[pixelforge::Codec::H264, pixelforge::Codec::H265, pixelforge::Codec::AV1]
				.into_iter()
				.any(|codec| context.supports_encode(codec))
		);
		// An impossible DRM identity cannot silently select the usable adapter.
		let result = VideoContextBuilder::new()
			.device_selector(DeviceSelector {
				drm_render_node: (i64::MAX, i64::MAX),
				pci_bus: None,
			})
			.build();
		let Err(pixelforge::PixelForgeError::NoSuitableDevice(message)) = result else {
			panic!("unknown capture identity must be rejected");
		};
		assert!(message.contains("no other GPU is eligible"));
	}

	#[test]
	#[ignore = "needs a GPU without Vulkan Video encode: MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE=<node>"]
	fn capture_device_without_encode_support_never_falls_back_on_gpu() {
		assert!(
			std::env::var_os("MOONSHINE_TEST_GPU").is_some(),
			"set MOONSHINE_TEST_GPU=1"
		);
		let path = std::env::var("MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE")
			.expect("set MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE");
		let file = File::open(path).unwrap();
		let capture = CaptureIdentity::from_file(&file).unwrap();
		let result = VideoContextBuilder::new()
			.device_selector(DeviceSelector {
				drm_render_node: capture.render,
				pci_bus: capture.pci,
			})
			.build();
		let Err(pixelforge::PixelForgeError::NoSuitableDevice(message)) = result else {
			panic!("the requested non-encoding GPU must not fall back to another adapter");
		};
		assert!(message.contains("no other GPU is eligible"));
		// The capture context accepts it compute-only: verified as the same
		// device (capture_context checks DRM/PCI identity), encoding nothing.
		let context = capture_context(&file).expect("compute-only capture context");
		for (codec, _) in ENCODE_CODECS {
			assert!(!context.supports_encode(codec));
			assert!(context.encode_unsupported_reason(codec).is_some());
		}
		assert!(encode_status(&context).starts_with("unavailable ("));
	}

	#[test]
	fn drm_identity_overrides_enumeration_and_pci_guessing() {
		let capture = CaptureIdentity {
			render: (226, 129),
			pci: Some((0, 3, 0, 0)),
		};
		assert!(!capture.matches(Some((226, 128)), capture.pci));
		assert!(capture.matches(Some((226, 129)), None));
		assert!(capture.matches(None, Some((0, 3, 0, 0))));
		assert!(!capture.matches(None, Some((0, 4, 0, 0))));
		assert!(!capture.matches(None, None));
		assert!(
			!CaptureIdentity {
				render: (226, 129),
				pci: None
			}
			.matches(None, None)
		);
	}
	#[test]
	fn pci_address_includes_domain_and_function() {
		assert_eq!(parse_pci("0001:0a:02.3"), Some((1, 10, 2, 3)));
		assert_eq!(parse_pci("not-pci"), None);
		assert_eq!(parse_pci("0000:03:00.g"), None);
	}
	#[test]
	fn every_sampled_fourcc_maps_and_others_are_rejected() {
		for fourcc in SAMPLED_FOURCCS {
			assert!(sampled_vk_format(fourcc).is_some(), "0x{fourcc:08x}");
		}
		// NV12 and RGB565 are never sampled as RGB.
		assert_eq!(sampled_vk_format(0x3231_564e), None);
		assert_eq!(sampled_vk_format(0x3631_4752), None);
	}

	#[test]
	fn importable_modifiers_match_format_and_modifier_exactly() {
		let importable = ImportableModifiers::from_pairs(vec![(0x3432_4241, 0x0300_0000_0060_6010)]);
		assert!(importable.contains(0x3432_4241, 0x0300_0000_0060_6010));
		// NVIDIA's EGL-only compressed kind and the implicit modifier.
		assert!(!importable.contains(0x3432_4241, 0x0300_0000_00e0_8014));
		assert!(!importable.contains(0x3432_4241, 0x00ff_ffff_ffff_ffff));
		assert!(!importable.contains(0x3432_5258, 0x0300_0000_0060_6010));
	}

	#[test]
	fn regular_files_cannot_masquerade_as_render_nodes() {
		let file = tempfile::tempfile().unwrap();
		assert!(CaptureIdentity::from_file(&file).is_err());
	}
}

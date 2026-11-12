//! Conventional (H.264/HEVC/AV1) encoder backends behind one interface.
//!
//! The encoding loop converts each captured frame into the input target the
//! encoder hands out and submits it; everything after the encoded packet
//! (padding removal, HDR metadata, packetization, FEC, encryption, pacing,
//! epochs) is shared. Vulkan Video (Pixelforge) is the preferred backend;
//! VA-API and NVENC encode the profiles Vulkan Video cannot on a GPU, or every
//! profile when selected explicitly (`stream.video.encoder`).
//!
//! A stream's backend is chosen once, before the stream starts, from the
//! startup probe's per-profile result ([`EncoderTable`]); it never changes
//! during a stream epoch.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use pixelforge::{ColorDescription, Frame, InputSlot, PixelForgeError, Source, StreamColor};

use super::convert::{ConvertFormat, Target};
use crate::session::stream::video::{BitDepth, ChromaFormat, VideoCodec};

/// Which encoder implementations may encode conventional profiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPreference {
	/// Vulkan Video for every profile it encodes on the capture GPU; VA-API,
	/// then NVENC, for the profiles it cannot.
	#[default]
	Auto,
	/// Vulkan Video only.
	Vulkan,
	/// VA-API only.
	Vaapi,
	/// NVENC only.
	Nvenc,
}

impl EncoderPreference {
	/// Backends to try for a profile, in order.
	pub(crate) fn candidates(self) -> &'static [EncoderBackend] {
		match self {
			Self::Auto => &[EncoderBackend::Vulkan, EncoderBackend::Vaapi, EncoderBackend::Nvenc],
			Self::Vulkan => &[EncoderBackend::Vulkan],
			Self::Vaapi => &[EncoderBackend::Vaapi],
			Self::Nvenc => &[EncoderBackend::Nvenc],
		}
	}
}

/// A conventional encoder implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EncoderBackend {
	Vulkan,
	Vaapi,
	Nvenc,
}

impl EncoderBackend {
	pub(crate) fn label(self) -> &'static str {
		match self {
			Self::Vulkan => "Vulkan Video",
			Self::Vaapi => "VA-API",
			Self::Nvenc => "NVENC",
		}
	}

	/// The converter output this backend encodes a format from, or why the
	/// combination is not implemented.
	pub(crate) fn convert_format(self, chroma: ChromaFormat, depth: BitDepth) -> Result<ConvertFormat, &'static str> {
		Ok(match (self, chroma, depth) {
			(_, ChromaFormat::Yuv420, BitDepth::Eight) => ConvertFormat::Nv12,
			(_, ChromaFormat::Yuv420, BitDepth::Ten) => ConvertFormat::P010,
			(Self::Vulkan, ChromaFormat::Yuv444, BitDepth::Eight) => ConvertFormat::Yuv444,
			(Self::Vulkan, ChromaFormat::Yuv444, BitDepth::Ten) => ConvertFormat::Yuv444P10,
			(Self::Vaapi | Self::Nvenc, ChromaFormat::Yuv444, BitDepth::Eight) => ConvertFormat::Xyuv,
			(Self::Vaapi, ChromaFormat::Yuv444, BitDepth::Ten) => ConvertFormat::Y410,
			// NVENC takes 10-bit 4:4:4 only as three 16-bit planes, which the
			// converter does not produce.
			(Self::Nvenc, ChromaFormat::Yuv444, BitDepth::Ten) => {
				return Err("NVENC 4:4:4 10-bit input is not implemented");
			},
		})
	}
}

/// The backend chosen at startup for each conventional profile bit
/// (`healthcheck::CODEC_PROFILES`); empty when no probe ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncoderTable(Vec<(u32, EncoderBackend)>);

impl EncoderTable {
	pub fn insert(&mut self, bit: u32, backend: EncoderBackend) {
		self.0.retain(|(b, _)| *b != bit);
		self.0.push((bit, backend));
	}

	pub fn get(&self, bit: u32) -> Option<EncoderBackend> {
		self.0.iter().find(|(b, _)| *b == bit).map(|(_, backend)| *backend)
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

/// Frame extent of a native backend's startup probe: a small stream size
/// above every encoder's minimum (AMD VCN 5 encodes HEVC from 384 columns).
pub(crate) const PROBE_EXTENT: (u32, u32) = (1280, 720);

/// What a native (VA-API, NVENC) encoder encodes: one stream epoch's format.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeFormat {
	pub codec: VideoCodec,
	pub chroma: ChromaFormat,
	pub bit_depth: BitDepth,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	/// Constant bitrate in bits per second.
	pub bitrate: u32,
	pub color: ColorDescription,
}

/// An encoded frame of any backend, in Annex B (H.264/HEVC) or low-overhead
/// OBU (AV1) form.
#[derive(Debug)]
pub(crate) struct Packet {
	pub data: Vec<u8>,
	pub is_key_frame: bool,
}

/// A backend failure, with device loss kept apart from its wording.
#[derive(Debug)]
pub(crate) enum EncodeError {
	Vulkan(PixelForgeError),
	Native { message: String, device_lost: bool },
}

impl EncodeError {
	pub(crate) fn native(message: impl Into<String>) -> Self {
		Self::Native {
			message: message.into(),
			device_lost: false,
		}
	}

	pub(crate) fn device_lost(&self) -> bool {
		match self {
			Self::Vulkan(error) => matches!(
				error,
				PixelForgeError::DeviceLost
					| PixelForgeError::Vulkan {
						result: ash::vk::Result::ERROR_DEVICE_LOST,
						..
					}
			),
			Self::Native { device_lost, .. } => *device_lost,
		}
	}
}

impl std::fmt::Display for EncodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Vulkan(error) => error.fmt(f),
			Self::Native { message, .. } => f.write_str(message),
		}
	}
}

impl From<PixelForgeError> for EncodeError {
	fn from(error: PixelForgeError) -> Self {
		Self::Vulkan(error)
	}
}

/// Resolves with a submitted frame's packet once the encoder finished it.
pub(crate) enum PacketFuture {
	Vulkan(pixelforge::EncodeFuture),
	Native(tokio::sync::oneshot::Receiver<Result<Packet, EncodeError>>),
}

impl Future for PacketFuture {
	type Output = Result<Packet, EncodeError>;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		match self.get_mut() {
			Self::Vulkan(future) => Pin::new(future).poll(cx).map(|result| {
				result
					.map(|packet| Packet {
						is_key_frame: packet.is_key_frame(),
						data: packet.data,
					})
					.map_err(EncodeError::Vulkan)
			}),
			Self::Native(receiver) => Pin::new(receiver).poll(cx).map(|result| {
				result.unwrap_or_else(|_| {
					Err(EncodeError::native(
						"encode cancelled: encoder shut down before the frame was read back",
					))
				})
			}),
		}
	}
}

/// A Pixelforge encoder and the input slot handed out for the next frame.
///
/// Pixelforge hands out its input image as an [`InputSlot`] that is valid
/// until the encoder's next submission. The conventional loop converts into
/// the slot's image and then submits it, so the slot is kept from
/// `input_target` to `encode`; asking again before a submission returns the
/// same slot.
pub(crate) struct VulkanEncoder {
	encoder: pixelforge::Encoder,
	slot: Option<InputSlot>,
}

impl VulkanEncoder {
	pub(crate) fn new(encoder: pixelforge::Encoder) -> Self {
		Self { encoder, slot: None }
	}

	/// The current slot's image, waiting until its previous encode is done.
	fn input_target(&mut self) -> Result<Target, EncodeError> {
		let slot = match &mut self.slot {
			Some(slot) => slot,
			slot @ None => slot.insert(self.encoder.input_slot().map_err(EncodeError::Vulkan)?),
		};
		Ok(Target::encoder_input(slot.image()))
	}

	/// Encode the slot `target` names, which the caller converted into.
	fn encode(&mut self, target: Target) -> Result<PacketFuture, EncodeError> {
		let Target::Image { image, .. } = target else {
			return Err(EncodeError::native("Vulkan Video encodes from its own input images"));
		};
		let slot = self
			.slot
			.take()
			.ok_or_else(|| EncodeError::native("no Vulkan Video input slot was taken for this frame"))?;
		if slot.image() != image {
			return Err(EncodeError::native("the frame was converted into another input slot"));
		}
		// The conversion completed on the CPU before this call, so the encode
		// waits for nothing on the GPU.
		self.encoder
			.submit(Frame::input(slot))
			.map(PacketFuture::Vulkan)
			.map_err(EncodeError::Vulkan)
	}
}

impl std::ops::Deref for VulkanEncoder {
	type Target = pixelforge::Encoder;

	fn deref(&self) -> &pixelforge::Encoder {
		&self.encoder
	}
}

impl std::ops::DerefMut for VulkanEncoder {
	fn deref_mut(&mut self) -> &mut pixelforge::Encoder {
		&mut self.encoder
	}
}

/// A conventional encoder of one stream epoch.
pub(crate) enum ConventionalEncoder {
	Vulkan(VulkanEncoder),
	Vaapi(super::vaapi::VaapiEncoder),
	Nvenc(super::nvenc::NvencEncoder),
}

/// Live rates stay within the negotiated startup budget: allocated output storage,
/// codec level and receiver capacity remain valid without a new stream epoch.
pub(super) fn bounded_bitrate(requested: u32, ceiling: u32) -> Result<u32, EncodeError> {
	if requested == 0 || ceiling == 0 {
		return Err(EncodeError::native("live bitrate must be non-zero"));
	}
	Ok(requested.min(ceiling))
}

impl ConventionalEncoder {
	pub(crate) fn next_display_order(&self) -> u64 {
		match self {
			Self::Vulkan(e) => e.next_display_order(),
			Self::Vaapi(e) => e.next_display_order(),
			Self::Nvenc(e) => e.next_display_order(),
		}
	}

	pub(crate) fn set_bitrate(&mut self, requested: u32) -> Result<u32, EncodeError> {
		match self {
			Self::Vulkan(encoder) => encoder.set_bitrate(requested).map_err(EncodeError::Vulkan),
			Self::Vaapi(encoder) => encoder.set_bitrate(requested),
			Self::Nvenc(encoder) => encoder.set_bitrate(requested),
		}
	}

	/// Force the next submitted frame to be an IDR (key) frame.
	pub(crate) fn request_idr(&mut self) {
		match self {
			Self::Vulkan(encoder) => encoder.request_idr(),
			Self::Vaapi(encoder) => encoder.request_idr(),
			Self::Nvenc(encoder) => encoder.request_idr(),
		}
	}

	/// Stop predicting from frames at or after `display_order`, the first
	/// frame the client lost. Backends without reference invalidation encode
	/// an IDR instead.
	pub(crate) fn invalidate_reference_frames(&mut self, display_order: u64) {
		match self {
			Self::Vulkan(encoder) => encoder.invalidate_reference_frames(display_order),
			Self::Vaapi(encoder) => encoder.invalidate_reference_frames(display_order),
			Self::Nvenc(encoder) => encoder.invalidate_reference_frames(display_order),
		}
	}

	/// Change the signalled color description; takes effect with a key frame.
	pub(crate) fn set_color_description(&mut self, description: ColorDescription) -> Result<(), EncodeError> {
		match self {
			// The source stays YUV the conversion writes; a changed colour
			// makes the next frame a key frame with the new headers.
			Self::Vulkan(encoder) => encoder
				.reconfigure(Source::Yuv, StreamColor::Other(description))
				.map_err(EncodeError::Vulkan),
			Self::Vaapi(encoder) => encoder.set_color_description(description),
			Self::Nvenc(encoder) => encoder.set_color_description(description),
		}
	}

	/// Wait for every submitted frame (end-of-epoch barrier).
	pub(crate) fn flush(&mut self) -> Result<(), EncodeError> {
		match self {
			Self::Vulkan(encoder) => encoder.flush().map_err(EncodeError::Vulkan),
			Self::Vaapi(encoder) => encoder.flush(),
			Self::Nvenc(encoder) => encoder.flush(),
		}
	}
}

/// The encoder-input contract of the conventional loop. Every backend owns a
/// small ring of input targets: `input_target` is the current one (waiting
/// until its previous encode finished) and every `encode` submits it and
/// advances to the next. Each captured frame is converted into
/// `input_target` and encoded from it immediately.
pub(crate) trait EncoderInput {
	type Target: Copy;
	type Future;
	type Error;
	fn input_target(&mut self) -> Result<Self::Target, Self::Error>;
	fn encode(&mut self, target: Self::Target) -> Result<Self::Future, Self::Error>;
}

impl EncoderInput for ConventionalEncoder {
	type Target = Target;
	type Future = PacketFuture;
	type Error = EncodeError;

	fn input_target(&mut self) -> Result<Target, EncodeError> {
		match self {
			Self::Vulkan(encoder) => encoder.input_target(),
			Self::Vaapi(encoder) => encoder.input_target(),
			Self::Nvenc(encoder) => encoder.input_target(),
		}
	}

	fn encode(&mut self, target: Target) -> Result<PacketFuture, EncodeError> {
		match self {
			Self::Vulkan(encoder) => encoder.encode(target),
			Self::Vaapi(encoder) => encoder.encode(target),
			Self::Nvenc(encoder) => encoder.encode(target),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn auto_prefers_vulkan_and_explicit_choices_are_strict() {
		assert_eq!(EncoderPreference::default(), EncoderPreference::Auto);
		assert_eq!(EncoderPreference::Auto.candidates()[0], EncoderBackend::Vulkan);
		assert_eq!(EncoderPreference::Vaapi.candidates(), &[EncoderBackend::Vaapi]);
		assert_eq!(EncoderPreference::Nvenc.candidates(), &[EncoderBackend::Nvenc]);
	}

	#[test]
	fn each_backend_encodes_444_from_its_own_layout() {
		use BitDepth::{Eight, Ten};
		use ChromaFormat::{Yuv420, Yuv444};
		for backend in [EncoderBackend::Vulkan, EncoderBackend::Vaapi, EncoderBackend::Nvenc] {
			assert_eq!(backend.convert_format(Yuv420, Eight), Ok(ConvertFormat::Nv12));
			assert_eq!(backend.convert_format(Yuv420, Ten), Ok(ConvertFormat::P010));
		}
		assert_eq!(
			EncoderBackend::Vulkan.convert_format(Yuv444, Eight),
			Ok(ConvertFormat::Yuv444)
		);
		assert_eq!(
			EncoderBackend::Vaapi.convert_format(Yuv444, Ten),
			Ok(ConvertFormat::Y410)
		);
		assert_eq!(
			EncoderBackend::Nvenc.convert_format(Yuv444, Eight),
			Ok(ConvertFormat::Xyuv)
		);
		assert!(EncoderBackend::Nvenc.convert_format(Yuv444, Ten).is_err());
	}

	#[test]
	fn the_table_keeps_one_backend_per_profile() {
		let mut table = EncoderTable::default();
		assert!(table.is_empty());
		table.insert(1, EncoderBackend::Vulkan);
		table.insert(1, EncoderBackend::Vaapi);
		table.insert(2, EncoderBackend::Nvenc);
		assert_eq!(table.get(1), Some(EncoderBackend::Vaapi));
		assert_eq!(table.get(2), Some(EncoderBackend::Nvenc));
		assert_eq!(table.get(3), None);
	}
}

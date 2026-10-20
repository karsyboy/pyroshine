use super::*;

#[test]
fn profiles_map_each_format_to_its_surface_layout() {
	let check = |codec, chroma, depth, rt, fourcc, convert| {
		let p = profile(codec, chroma, depth).unwrap();
		assert_eq!((p.rt_format, p.fourcc, p.convert), (rt, fourcc, convert));
	};
	use BitDepth::{Eight, Ten};
	use ChromaFormat::{Yuv420, Yuv444};
	check(
		VideoCodec::H264,
		Yuv420,
		Eight,
		VA_RT_FORMAT_YUV420,
		VA_FOURCC_NV12,
		ConvertFormat::Nv12,
	);
	check(
		VideoCodec::Hevc,
		Yuv420,
		Ten,
		VA_RT_FORMAT_YUV420_10,
		VA_FOURCC_P010,
		ConvertFormat::P010,
	);
	check(
		VideoCodec::Hevc,
		Yuv444,
		Eight,
		VA_RT_FORMAT_YUV444,
		VA_FOURCC_XYUV,
		ConvertFormat::Xyuv,
	);
	check(
		VideoCodec::Hevc,
		Yuv444,
		Ten,
		VA_RT_FORMAT_YUV444_10,
		VA_FOURCC_Y410,
		ConvertFormat::Y410,
	);
	check(
		VideoCodec::Av1,
		Yuv420,
		Ten,
		VA_RT_FORMAT_YUV420_10,
		VA_FOURCC_P010,
		ConvertFormat::P010,
	);
	assert!(profile(VideoCodec::Av1, Yuv444, Eight).is_err());
	assert!(profile(VideoCodec::H264, Yuv444, Eight).is_err());
}

#[test]
fn surface_layers_match_the_converter_planes() {
	let layers = layer_formats(ConvertFormat::Nv12, (1920, 1088));
	assert_eq!(
		layers,
		[(vk::Format::R8_UNORM, 1920, 1088), (vk::Format::R8G8_UNORM, 960, 544)]
	);
	assert_eq!(layer_formats(ConvertFormat::Y410, (16, 16)).len(), 1);
	assert!(layer_formats(ConvertFormat::Yuv444, (16, 16)).is_empty());
}

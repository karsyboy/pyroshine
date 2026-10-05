//! Color conversion in the scene's existing texture draw, with no extra pass.
use super::frame::FrameColorSpace;
use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTexProgram};

pub(super) struct ColorShaders([[Option<GlesTexProgram>; 4]; 2]);
impl ColorShaders {
	pub fn new(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
		let mut programs = std::array::from_fn(|_| std::array::from_fn(|_| None));
		for (target, row) in programs.iter_mut().enumerate() {
			for (source, slot) in row.iter_mut().enumerate() {
				if source == target {
					continue;
				}
				let shader = include_str!("color.frag").replace(
					"//_DEFINES_",
					&format!("//_DEFINES_\n#define SOURCE {source}\n#define TARGET {target}"),
				);
				*slot = Some(renderer.compile_custom_texture_shader(shader, &[])?);
			}
		}
		Ok(Self(programs))
	}
	pub fn program(&self, source: usize, target: FrameColorSpace) -> Option<GlesTexProgram> {
		self.0[usize::from(target == FrameColorSpace::Bt2020Pq)][source].clone()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use smithay::{
		backend::{
			allocator::Fourcc,
			egl::{EGLContext, EGLDisplay},
			renderer::{Bind, ExportMem, Frame, ImportMem, Offscreen, Renderer, gles::GlesRenderbuffer},
		},
		utils::{Rectangle, Transform},
	};

	/// Readback is confined to this test; production draws stay on the GPU.
	#[test]
	#[ignore = "needs an EGL-capable GPU render node"]
	fn native_color_draw_preserves_pq_and_scales_linear_light() {
		let path = super::super::find_render_node(&None).unwrap();
		let fd = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
		let device = smithay::backend::allocator::gbm::GbmDevice::new(fd).unwrap();
		// SAFETY: the EGL display owns its GBM device and outlives the renderer.
		let display = unsafe { EGLDisplay::new(device) }.unwrap();
		let context = EGLContext::new(&display).unwrap();
		// SAFETY: the test retains the display and operates on one thread.
		let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
		let shaders = ColorShaders::new(&mut renderer).unwrap();
		let mut target: GlesRenderbuffer = renderer.create_buffer(Fourcc::Abgr8888, (1, 1).into()).unwrap();
		let mut draw = |bytes: &[u8], format, source, color| {
			let texture = renderer.import_memory(bytes, format, (1, 1).into(), false).unwrap();
			let mut fb = renderer.bind(&mut target).unwrap();
			let mut frame = renderer.render(&mut fb, (1, 1).into(), Transform::Normal).unwrap();
			if let Some(program) = shaders.program(source, color) {
				frame.override_default_tex_program(program, Vec::new());
			}
			let rect = Rectangle::from_size((1, 1).into());
			frame.clear([0.0, 0.0, 0.0, 0.0].into(), &[rect]).unwrap();
			frame
				.render_texture_at(&texture, (0, 0).into(), 1, 1.0, Transform::Normal, &[rect], &[], 1.0)
				.unwrap();
			let fence = frame.finish().unwrap();
			renderer.wait(&fence).unwrap();
			let mapping = renderer
				.copy_framebuffer(&fb, Rectangle::from_size((1, 1).into()), Fourcc::Abgr8888)
				.unwrap();
			renderer.map_texture(&mapping).unwrap()[..4].to_vec()
		};
		let assert_rgb = |actual: Vec<u8>, expected: u8| {
			for value in &actual[..3] {
				assert!(value.abs_diff(expected) <= 1, "pixel {actual:?}, expected {expected}");
			}
		};
		// PQ input uses the ordinary texture draw, without another OETF.
		assert_rgb(
			draw(&[191, 191, 191, 255], Fourcc::Abgr8888, 1, FrameColorSpace::Bt2020Pq),
			191,
		);
		// ST 2084: 80 nits = 0.48585677; linear scRGB 1.0 is exactly 80 nits.
		let linear_white = [0x00, 0x3c, 0x00, 0x3c, 0x00, 0x3c, 0x00, 0x3c];
		assert_rgb(
			draw(&linear_white, Fourcc::Abgr16161616f, 2, FrameColorSpace::Bt2020Pq),
			124,
		);
		// A BT.709 red primary maps to all three BT.2020 channels. These
		// ST 2084 code values catch a transposed or omitted gamut matrix.
		let red = draw(
			&[0x00, 0x3c, 0, 0, 0, 0, 0x00, 0x3c],
			Fourcc::Abgr16161616f,
			2,
			FrameColorSpace::Bt2020Pq,
		);
		for (actual, expected) in red[..3].iter().zip([112u8, 65, 42]) {
			assert!(actual.abs_diff(expected) <= 1, "red primary {red:?}");
		}
		// SDR reference white is anchored to the HDR output's 203-nit white.
		assert_rgb(
			draw(&[255, 255, 255, 255], Fourcc::Abgr8888, 0, FrameColorSpace::Bt2020Pq),
			148,
		);
		// The shader unpremultiplies before transfer conversion, then restores alpha.
		assert_rgb(
			draw(&[128, 128, 128, 128], Fourcc::Abgr8888, 0, FrameColorSpace::Bt2020Pq),
			74,
		);
		// The SDR target clips HDR highlights rather than treating PQ as sRGB.
		assert_rgb(
			draw(&[255, 255, 255, 255], Fourcc::Abgr8888, 1, FrameColorSpace::Srgb),
			255,
		);
	}
}

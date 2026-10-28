//! Zero-copy DMA-BUF import into Vulkan images for video encoding.
//!
//! [`DmaBufImporter`] imports a captured frame (and its DMA-BUF overlay
//! layers) for one conversion; the [`DmaBufImport`] is dropped once the
//! conversion's fence has signaled. Imports are deliberately not cached: an
//! import that stays alive keeps the client's buffer in this device's
//! submissions, whose implicit synchronization then makes later work (the
//! video encode among it) wait for whatever the client is rendering into that
//! buffer. Behind a GPU-bound game that held every frame for the game's next
//! one (see `PIPELINE_OPTIMIZATION.md`). Importing costs about 15 µs of CPU per
//! frame plus mapping the buffer at submission.
//!
//! The module also identifies DMA-BUFs for callers that do cache imports (the
//! PyroWave encoder): [`same_buffer`] checks whether two fds refer to one
//! buffer. [`KcmpProbe`] picks the method once per process:
//! - `kcmp(2)` (preferred). It checks whether two fds share one open file
//!   description, the kernel's `struct file`. All DMA-BUF fds in a session are
//!   dups of the exporter's single `struct file`, so this means "same buffer".
//! - Inode comparison (fallback). `fstat(2)` returns the device and inode
//!   number of the file behind an fd. Since Linux 5.3, each DMA-BUF has its own
//!   inode on the kernel's dmabuf pseudo-filesystem, so equal numbers mean the
//!   same buffer. Pyroshine uses this method when a seccomp filter blocks
//!   `kcmp(2)` (Podman's default profile, Flatpak) or the kernel lacks it.
//!
//! The inode method has one limitation. The kernel numbers these inodes with a
//! 32-bit counter (`get_next_ino`), shared with pipes, sockets and other
//! pseudo-files. After about 4 billion new inodes the counter wraps, and a new
//! buffer could get the inode number of a cached one; the caller's own expiry
//! and layout checks keep the risk negligible by accident. A local process can
//! force a collision on purpose, which yields only a stale frame from a buffer
//! the session already provided; `kcmp(2)` cannot be fooled this way, which is
//! why it is preferred.
//!
//! The startup healthcheck reports which method is in use. It warns, but does
//! not refuse startup, when `kcmp(2)` is unavailable.

use ash::vk;
use pixelforge::VideoContext;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::io::{BorrowedFd, IntoRawFd};
use std::sync::OnceLock;
use tracing::debug;

/// Own partially-created resources until the import has completely succeeded.
/// Every setup/driver error must release the image and any allocated memory.
struct ImportResources<'a> {
	device: &'a ash::Device,
	image: vk::Image,
	memory: vk::DeviceMemory,
}

impl ImportResources<'_> {
	fn into_handles(mut self) -> (vk::Image, vk::DeviceMemory) {
		let handles = (self.image, self.memory);
		self.image = vk::Image::null();
		self.memory = vk::DeviceMemory::null();
		handles
	}
}

impl Drop for ImportResources<'_> {
	fn drop(&mut self) {
		unsafe {
			if self.image != vk::Image::null() {
				self.device.destroy_image(self.image, None);
			}
			if self.memory != vk::DeviceMemory::null() {
				self.device.free_memory(self.memory, None);
			}
		}
	}
}

/// A single DMA-BUF plane descriptor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DmaBufPlane {
	pub fd: RawFd,
	/// Offset within the DMA-BUF to the start of this plane.
	pub offset: u32,
	/// Row stride in bytes.
	pub stride: u32,
	/// DRM format modifier.
	pub modifier: u64,
}

/// A DMA-BUF imported into the encoders' device for one conversion. Dropping
/// it frees the image and the imported memory, so it must outlive every GPU
/// read of it.
pub(crate) struct DmaBufImport {
	image: vk::Image,
	memory: vk::DeviceMemory,
	width: u32,
	height: u32,
	/// Own context clone so `Drop` can free resources after the importer is gone.
	context: VideoContext,
}

/// How [`same_buffer`] checks that two fds refer to the same DMA-BUF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareMethod {
	/// Compare open file descriptions with `kcmp(2)`.
	Kcmp,
	/// Compare device and inode numbers from `fstat(2)`.
	Inode,
}

/// Result of the one-time `kcmp(2)` probe.
///
/// [`same_buffer`] and the startup healthcheck read the same result, so the
/// healthcheck reports the method its callers really use.
#[derive(Debug)]
pub(crate) enum KcmpProbe {
	/// `kcmp(2)` works.
	Available,
	/// `kcmp(2)` failed with this error.
	Unavailable(io::Error),
	/// The probe could not open `/dev/null` to test `kcmp(2)`.
	NoProbeFile(io::Error),
}

impl KcmpProbe {
	/// Probe `kcmp(2)` on first call and return the stored result after that.
	pub(crate) fn get() -> &'static Self {
		static PROBE: OnceLock<KcmpProbe> = OnceLock::new();
		PROBE.get_or_init(|| match File::open("/dev/null") {
			Ok(file) => match kcmp_file(file.as_fd(), file.as_fd()) {
				Ok(_) => Self::Available,
				Err(e) => Self::Unavailable(e),
			},
			Err(e) => Self::NoProbeFile(e),
		})
	}

	/// The comparison method this result selects. Any probe failure selects
	/// the inode method, so there is always a working method.
	pub(crate) fn method(&self) -> CompareMethod {
		match self {
			Self::Available => CompareMethod::Kcmp,
			Self::Unavailable(_) | Self::NoProbeFile(_) => CompareMethod::Inode,
		}
	}
}

/// Check with `kcmp(2)` whether two fds share one open file description.
fn kcmp_file(a: BorrowedFd<'_>, b: BorrowedFd<'_>) -> io::Result<bool> {
	/// `KCMP_FILE` — not exposed by `libc` or glibc.
	const KCMP_FILE: libc::c_int = 0;

	// The kernel reads the fds as `unsigned long`. A `BorrowedFd` is never negative.
	let [a, b] = [a, b].map(|fd| libc::c_ulong::from(fd.as_raw_fd().unsigned_abs()));

	// SAFETY: syscall with scalar arguments; fds are only compared, not accessed.
	let result = unsafe { libc::syscall(libc::SYS_kcmp, libc::getpid(), libc::getpid(), KCMP_FILE, a, b) };
	if result < 0 {
		return Err(io::Error::last_os_error());
	}

	// 0 = identical, 1/2 = ordering between distinct files.
	Ok(result == 0)
}

/// Check whether two open fds refer to the same DMA-BUF, with the method
/// [`KcmpProbe`] selected. An error counts as different buffers.
///
/// For callers that keep their own `dup` of a buffer, such as the PyroWave
/// import cache.
pub(crate) fn same_buffer(a: BorrowedFd<'_>, b: BorrowedFd<'_>) -> bool {
	match KcmpProbe::get().method() {
		CompareMethod::Kcmp => kcmp_file(a, b).unwrap_or(false),
		CompareMethod::Inode => matches!((BufferId::of(a), BufferId::of(b)), (Ok(a), Ok(b)) if a == b),
	}
}

/// Identity of a DMA-BUF: the device and inode number of its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferId {
	dev: libc::dev_t,
	ino: libc::ino_t,
}

impl BufferId {
	/// Read the identity of the file behind `fd` with `fstat(2)`.
	fn of(fd: BorrowedFd<'_>) -> io::Result<Self> {
		let mut stat = MaybeUninit::<libc::stat>::uninit();
		// SAFETY: `fd` is open, and `stat` has room for a `struct stat`.
		if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
			return Err(io::Error::last_os_error());
		}
		// SAFETY: `fstat` succeeded, so it filled in `stat`.
		let stat = unsafe { stat.assume_init() };
		Ok(Self {
			dev: stat.st_dev,
			ino: stat.st_ino,
		})
	}
}

impl DmaBufImport {
	/// Wrap a test-owned image (not a DMA-BUF) so GPU fixture tests can drive
	/// consumers of imports. The import owns and frees `image` and `memory`.
	#[cfg(test)]
	pub(crate) fn for_test(
		context: VideoContext,
		image: vk::Image,
		memory: vk::DeviceMemory,
		width: u32,
		height: u32,
	) -> Self {
		Self {
			image,
			memory,
			width,
			height,
			context,
		}
	}

	/// The imported Vulkan image.
	pub(crate) fn image(&self) -> vk::Image {
		self.image
	}

	/// Width and height of the imported image.
	pub(crate) fn extent(&self) -> (u32, u32) {
		(self.width, self.height)
	}
}

impl Drop for DmaBufImport {
	fn drop(&mut self) {
		// SAFETY: the owner drops the import only after every GPU read of it.
		unsafe {
			let device = self.context.device();
			device.destroy_image(self.image, None);
			device.free_memory(self.memory, None);
		}
	}
}

/// Imports DMA-BUF fds into Vulkan images, one import per conversion (see the
/// module documentation for why nothing is cached).
///
/// Every import starts in `UNDEFINED` layout; the consumer (`InputConverter`)
/// acquires it from the foreign queue family in the same submission that
/// reads it, so an import costs no extra GPU submission.
pub(crate) struct DmaBufImporter {
	context: VideoContext,
	external_memory_fd: ash::khr::external_memory_fd::Device,
}

impl DmaBufImporter {
	/// Create a new DMA-BUF importer.
	pub fn new(context: VideoContext) -> Self {
		let external_memory_fd = ash::khr::external_memory_fd::Device::load(context.instance(), context.device());
		Self {
			context,
			external_memory_fd,
		}
	}

	/// Import the DMA-BUF of `planes` (borrowed: Vulkan receives a duplicate
	/// of the first plane's fd).
	pub fn import(
		&self,
		width: u32,
		height: u32,
		format: vk::Format,
		planes: &[DmaBufPlane],
	) -> Result<DmaBufImport, String> {
		let (image, memory) = self.import_internal(width, height, format, planes)?;
		Ok(DmaBufImport {
			image,
			memory,
			width,
			height,
			context: self.context.clone(),
		})
	}

	/// Raw Vulkan DMA-BUF import. Returns `(VkImage, VkDeviceMemory)` in `UNDEFINED` layout.
	fn import_internal(
		&self,
		width: u32,
		height: u32,
		format: vk::Format,
		planes: &[DmaBufPlane],
	) -> Result<(vk::Image, vk::DeviceMemory), String> {
		if planes.is_empty() {
			return Err("At least one DMA-BUF plane is required".to_string());
		}

		let device = self.context.device();

		// Build DRM format modifier plane layouts for all planes.
		// AMD modifiers (e.g. tiled/DCC) may require multiple planes;
		// the layout count must match the modifier's expected plane count.
		let plane_layouts: Vec<vk::SubresourceLayout> = planes
			.iter()
			.map(|p| {
				vk::SubresourceLayout::default()
					.offset(p.offset as u64)
					.row_pitch(p.stride as u64)
			})
			.collect();

		let modifier = planes[0].modifier;
		let mut drm_format_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
			.drm_format_modifier(modifier)
			.plane_layouts(&plane_layouts);

		let mut external_memory_info =
			vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
		external_memory_info.p_next =
			&mut drm_format_modifier_info as *mut vk::ImageDrmFormatModifierExplicitCreateInfoEXT as *mut _;

		let mut image_create_info = vk::ImageCreateInfo::default()
			.image_type(vk::ImageType::TYPE_2D)
			.format(format)
			.extent(vk::Extent3D {
				width,
				height,
				depth: 1,
			})
			.mip_levels(1)
			.array_layers(1)
			.samples(vk::SampleCountFlags::TYPE_1)
			.tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
			.usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::SAMPLED)
			.sharing_mode(vk::SharingMode::EXCLUSIVE)
			.initial_layout(vk::ImageLayout::UNDEFINED);
		image_create_info.p_next = &mut external_memory_info as *mut vk::ExternalMemoryImageCreateInfo as *mut _;

		let image = unsafe { device.create_image(&image_create_info, None) }
			.map_err(|e| format!("DMA-BUF image creation: {e}"))?;
		let mut resources = ImportResources {
			device,
			image,
			memory: vk::DeviceMemory::null(),
		};

		// Memory requirements.
		let mem_requirements = unsafe { device.get_image_memory_requirements(image) };

		// FD memory properties.
		let mut memory_fd_properties = vk::MemoryFdPropertiesKHR::default();
		unsafe {
			self.external_memory_fd.get_memory_fd_properties(
				vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
				planes[0].fd,
				&mut memory_fd_properties,
			)
		}
		.map_err(|e| format!("Failed to get memory FD properties: {e}"))?;

		// Duplicate the FD — vkAllocateMemory consumes it.
		let fd = unsafe { BorrowedFd::borrow_raw(planes[0].fd) }
			.try_clone_to_owned()
			.map_err(|e| format!("Failed to duplicate DMA-BUF FD: {e}"))?;

		let mut import_memory_fd_info = vk::ImportMemoryFdInfoKHR::default()
			.handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
			.fd(fd.as_raw_fd());

		let memory_type_bits = mem_requirements.memory_type_bits & memory_fd_properties.memory_type_bits;

		debug!(
			"Memory allocation: size={}, image_type_bits={:#x}, fd_type_bits={:#x}, combined={:#x}",
			mem_requirements.size,
			mem_requirements.memory_type_bits,
			memory_fd_properties.memory_type_bits,
			memory_type_bits
		);

		let memory_type_index = self
			.context
			.find_memory_type(memory_type_bits, vk::MemoryPropertyFlags::empty())
			.ok_or_else(|| "No suitable memory type for DMA-BUF import".to_string())?;

		// Dedicated allocation (required by many drivers for external memory).
		let mut dedicated_alloc_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
		import_memory_fd_info.p_next = &mut dedicated_alloc_info as *mut vk::MemoryDedicatedAllocateInfo as *mut _;

		let mut alloc_info = vk::MemoryAllocateInfo::default()
			.allocation_size(mem_requirements.size)
			.memory_type_index(memory_type_index);
		alloc_info.p_next = &mut import_memory_fd_info as *mut vk::ImportMemoryFdInfoKHR as *mut _;

		let memory =
			unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| format!("DMA-BUF memory import: {e}"))?;
		// Vulkan takes fd ownership only after a successful allocation. Until
		// then OwnedFd closes it on every error, including memory-type lookup.
		let _ = fd.into_raw_fd();
		resources.memory = memory;

		if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
			return Err(format!("DMA-BUF memory bind: {e}"));
		}

		Ok(resources.into_handles())
	}
}

#[cfg(test)]
mod tests {
	use super::{BufferId, CompareMethod, DmaBufPlane, ImportResources, KcmpProbe, kcmp_file, same_buffer};
	use ash::vk;
	use std::io;
	use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

	#[test]
	fn incomplete_imports_release_resources_but_success_transfers_ownership() {
		use ash::vk::Handle;
		use std::cell::RefCell;
		thread_local! { static RELEASED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) }; }
		unsafe extern "system" fn destroy(_: vk::Device, _: vk::Image, _: *const vk::AllocationCallbacks) {
			RELEASED.with_borrow_mut(|released| released.push("image"));
		}
		unsafe extern "system" fn free(_: vk::Device, _: vk::DeviceMemory, _: *const vk::AllocationCallbacks) {
			RELEASED.with_borrow_mut(|released| released.push("memory"));
		}
		// Fake Vulkan dispatch exercises ownership without requiring a GPU.
		let device = unsafe {
			ash::Device::load_with(
				|name| match name.to_bytes() {
					b"vkDestroyImage" => destroy as *const () as *const std::ffi::c_void,
					b"vkFreeMemory" => free as *const () as *const std::ffi::c_void,
					_ => std::ptr::null(),
				},
				vk::Device::null(),
			)
		};
		let image = vk::Image::from_raw(1);
		let memory = vk::DeviceMemory::from_raw(2);
		// Properties/dup/type/allocation failures have an image but no memory.
		for _ in 0..4 {
			drop(ImportResources {
				device: &device,
				image,
				memory: vk::DeviceMemory::null(),
			});
		}
		// Bind failure releases the image before its backing allocation.
		drop(ImportResources {
			device: &device,
			image,
			memory,
		});
		let handles = ImportResources {
			device: &device,
			image,
			memory,
		}
		.into_handles();
		assert_eq!(handles, (image, memory));
		RELEASED.with_borrow(|released| assert_eq!(released, &["image", "image", "image", "image", "image", "memory"]));
	}

	/// Create an anonymous in-memory file. Like a DMA-BUF, each memfd has its
	/// own inode on a kernel pseudo-filesystem and its own open file description.
	fn memfd() -> OwnedFd {
		// SAFETY: the name is a valid C string, and no flags are set.
		let fd = unsafe { libc::memfd_create(c"moonshine-test".as_ptr(), 0) };
		assert!(fd >= 0, "memfd_create failed: {}", io::Error::last_os_error());
		// SAFETY: `fd` is a new fd that nothing else owns.
		unsafe { OwnedFd::from_raw_fd(fd) }
	}

	#[test]
	fn identity_methods_distinguish_buffers() {
		let buffer = memfd();
		// Same buffer: shares the open file description and the inode.
		let dup = buffer.try_clone().unwrap();
		// Different buffer: its own open file description and inode.
		let other = memfd();

		// Some sandboxes block kcmp(2), e.g. Podman's default seccomp profile.
		// Test it only where the probe found it.
		let kcmp_available = KcmpProbe::get().method() == CompareMethod::Kcmp;
		if !kcmp_available {
			eprintln!(
				"kcmp(2) unavailable ({:?}); testing the inode method only",
				KcmpProbe::get()
			);
		}
		let methods = [CompareMethod::Kcmp, CompareMethod::Inode]
			.into_iter()
			.filter(|&method| method == CompareMethod::Inode || kcmp_available);

		for method in methods {
			let same = |a: BorrowedFd<'_>, b: BorrowedFd<'_>| match method {
				CompareMethod::Kcmp => kcmp_file(a, b).unwrap(),
				CompareMethod::Inode => BufferId::of(a).unwrap() == BufferId::of(b).unwrap(),
			};
			assert!(same(buffer.as_fd(), dup.as_fd()), "{method:?}: dup must match");
			assert!(
				!same(buffer.as_fd(), other.as_fd()),
				"{method:?}: other buffer must not match"
			);
		}

		// The probed method, as the PyroWave import cache uses it.
		assert!(same_buffer(buffer.as_fd(), dup.as_fd()));
		assert!(!same_buffer(buffer.as_fd(), other.as_fd()));
	}

	/// STAB-004 on real hardware (opt-in with `MOONSHINE_TEST_GPU=1` and
	/// `--ignored`; ordinary CI has no GPU). A frame exported from a GBM pool whose buffer and
	/// allocator were dropped — the compositor exiting before the encoder
	/// imports — still imports into Vulkan, and so does a new buffer that
	/// receives the same descriptor number after the frame is gone.
	#[test]
	#[ignore = "needs a GPU render node: MOONSHINE_TEST_GPU=1"]
	fn gpu_import_after_source_owner_teardown_and_fd_reuse() {
		use super::DmaBufImporter;
		use crate::session::capture::frame::{ExportedFrame, FrameColorSpace};
		use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
		use smithay::backend::allocator::{Allocator, Fourcc, Modifier};
		use std::sync::Arc;
		use std::sync::atomic::AtomicBool;

		assert!(
			std::env::var_os("MOONSHINE_TEST_GPU").is_some(),
			"set MOONSHINE_TEST_GPU=1 to run the GPU import test"
		);
		const SIZE: u32 = 256;
		let allocator = || {
			let node = crate::healthcheck::find_render_node(&None).unwrap();
			let file = std::fs::OpenOptions::new().read(true).write(true).open(node).unwrap();
			GbmAllocator::new(GbmDevice::new(file).unwrap(), GbmBufferFlags::RENDERING)
		};
		let export = |allocator: &mut GbmAllocator<std::fs::File>| {
			let buffer = allocator
				.create_buffer(SIZE, SIZE, Fourcc::Xrgb8888, &[Modifier::Linear])
				.unwrap();
			let dmabuf = smithay::backend::allocator::dmabuf::AsDmabuf::export(&buffer).unwrap();
			let frame = ExportedFrame::from_dmabuf(
				&dmabuf,
				0,
				Arc::new(AtomicBool::new(false)),
				FrameColorSpace::Srgb,
				None,
			);
			// The compositor's pool slot and GBM buffer go away.
			drop((dmabuf, buffer));
			frame
		};
		let planes = |frame: &ExportedFrame| -> Vec<DmaBufPlane> {
			frame
				.planes()
				.map(|plane| DmaBufPlane {
					fd: plane.fd.as_raw_fd(),
					offset: plane.offset,
					stride: plane.stride,
					modifier: frame.modifier,
				})
				.collect()
		};
		let format = vk::Format::B8G8R8A8_UNORM;
		let node = crate::healthcheck::find_render_node(&None).unwrap();
		let file = std::fs::OpenOptions::new().read(true).write(true).open(node).unwrap();
		let verified = crate::gpu::capture_context(&file).unwrap();
		let (sender, receiver) = crate::session::capture::admission::capture_channel();
		sender.set_context(verified.clone());
		let context = receiver.context().unwrap();
		assert_eq!(context.physical_device(), verified.physical_device());
		assert_eq!(context.device().handle(), verified.device().handle());
		drop((sender, file, verified));
		let importer = DmaBufImporter::new(context.clone());

		let mut first_allocator = allocator();
		let first = export(&mut first_allocator);
		// The whole compositor (GBM device included) exits before import.
		drop(first_allocator);
		let first_planes = planes(&first);
		let import = importer
			.import(SIZE, SIZE, format, &first_planes)
			.expect("a frame must import after its compositor owner is gone");
		// Every import is its own image: nothing is cached between frames.
		let again = importer.import(SIZE, SIZE, format, &first_planes).unwrap();
		assert_ne!(import.image(), again.image());
		let reused_number = first_planes[0].fd;

		// A replacement compositor exists before the old frame is released, so
		// the next export is handed the freed descriptor number.
		let mut second_allocator = allocator();
		drop(first);
		let second = export(&mut second_allocator);
		let mut second_planes = planes(&second);
		// How many descriptors an export opens and closes differs by driver
		// (on Intel's xe the frame lands one number higher), so place a
		// duplicate of the new buffer on the freed number when needed.
		let _duplicate = (second_planes[0].fd != reused_number).then(|| {
			// SAFETY: F_GETFD only probes the number; dup2 targets it only
			// when it is free, and the OwnedFd takes ownership of the result.
			unsafe {
				assert_eq!(
					libc::fcntl(reused_number, libc::F_GETFD),
					-1,
					"test setup: the freed descriptor number was taken"
				);
				assert_eq!(libc::dup2(second_planes[0].fd, reused_number), reused_number);
				<std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(reused_number)
			}
		});
		second_planes[0].fd = reused_number;
		let second_import = importer
			.import(SIZE, SIZE, format, &second_planes)
			.expect("a new buffer on a freed descriptor number must import");
		assert_ne!(import.image(), second_import.image());
		// SAFETY: plain idle wait on the test's own device.
		unsafe { context.device().device_wait_idle() }.unwrap();
		drop((import, again, second_import, importer));
	}
}

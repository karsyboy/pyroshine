//! Small decoder-visible short-term DPB. Trust is invalidated through the
//! submitted head, including queued pictures, and is rebuilt only from a safe
//! parent or IDR. Surface reuse excludes every retained reconstruction.
pub(super) const MAX_REFS: usize = 4;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reference {
	pub surface: u32,
	pub order: u64,
	pub codec_order: u64,
	trusted: bool,
}
#[derive(Debug)]
pub(super) struct Dpb {
	entries: [Option<Reference>; MAX_REFS],
	capacity: usize,
}
impl Dpb {
	pub fn new(capacity: usize) -> Self {
		assert!((1..=MAX_REFS).contains(&capacity));
		Self {
			entries: [None; MAX_REFS],
			capacity,
		}
	}
	pub fn clear(&mut self) {
		self.entries.fill(None);
	}
	pub fn invalidate(&mut self, first: u64) {
		for reference in self.entries.iter_mut().flatten() {
			if reference.order >= first {
				reference.trusted = false;
			}
		}
	}
	pub fn anchor(&self) -> Option<Reference> {
		self.entries.iter().flatten().rev().find(|r| r.trusted).copied()
	}
	pub fn contains(&self, surface: u32) -> bool {
		self.entries.iter().flatten().any(|r| r.surface == surface)
	}
	pub fn references(&self) -> [Option<(u32, u64)>; MAX_REFS] {
		self.entries.map(|r| r.map(|r| (r.surface, r.codec_order)))
	}
	pub fn push(&mut self, surface: u32, order: u64, codec_order: u64) {
		self.entries[..self.capacity].rotate_left(1);
		self.entries[self.capacity - 1] = Some(Reference {
			surface,
			order,
			codec_order,
			trusted: true,
		});
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn queued_descendants_are_tainted_and_recovery_itself_can_be_lost() {
		let mut dpb = Dpb::new(4);
		for n in 0..8 {
			dpb.push(n as u32, n, n);
		}
		dpb.invalidate(6);
		assert_eq!(dpb.anchor().unwrap().order, 5);
		dpb.invalidate(7);
		assert_eq!(dpb.anchor().unwrap().order, 5);
		dpb.push(8, 8, 8);
		assert_eq!(dpb.anchor().unwrap().order, 8);
		dpb.invalidate(6);
		assert_eq!(dpb.anchor().unwrap().order, 5);
		dpb.push(9, 9, 9);
		dpb.invalidate(6);
		assert_eq!(dpb.anchor(), None);
		dpb.clear();
		dpb.push(10, 10, 0);
		assert_eq!(dpb.anchor().unwrap().codec_order, 0);
	}
	#[test]
	fn retention_surface_reuse_and_codec_order_are_distinct_from_capture_indices() {
		let mut dpb = Dpb::new(2);
		dpb.push(10, 100, 255);
		dpb.push(11, 101, 256);
		assert!(dpb.contains(10));
		assert!(!dpb.contains(12));
		dpb.invalidate(101);
		assert_eq!(dpb.anchor().unwrap().codec_order, 255);
		dpb.push(12, 102, 257);
		assert!(!dpb.contains(10));
		dpb.invalidate(100);
		assert_eq!(dpb.anchor(), None);
	}
}

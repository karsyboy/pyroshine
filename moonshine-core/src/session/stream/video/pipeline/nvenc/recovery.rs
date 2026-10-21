//! Through-head invalidation policy. An explicit preset DPB depth bounds
//! anchors; an automatic (zero) depth is unknown and never treated as 16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Plan {
	Idr,
	Covered,
	Invalidate { first: u64, end: u64 },
}
#[derive(Debug, Default)]
pub(super) struct Recovery {
	retained: Option<u32>,
	epoch_start: u64,
	invalidated: Option<(u64, u64)>,
}
impl Recovery {
	pub fn new(retained: u32) -> Self {
		Self {
			retained: (retained > 0).then_some(retained),
			..Self::default()
		}
	}
	pub fn reset(&mut self, first: u64) {
		self.epoch_start = first;
		self.invalidated = None;
	}
	pub fn plan(&self, first: u64, next: u64) -> Plan {
		if first <= self.epoch_start || first >= next || next - first > super::MAX_INVALIDATIONS {
			return Plan::Idr;
		}
		// Covered feedback still needs a new recovery picture; only the native
		// calls are coalesced, and only when no new dependent picture exists.
		if self
			.invalidated
			.is_some_and(|(start, end)| start <= first && end == next)
		{
			return Plan::Covered;
		}
		let Some(depth) = self.retained else {
			// Zero means the SDK manages retention. Preserve its existing native
			// invalidation/anchor selection, without inventing a DPB depth from
			// the call-cost bound. SDK rejection still forces IDR at the caller.
			return Plan::Invalidate { first, end: next };
		};
		let oldest = next.saturating_sub(u64::from(depth)).max(self.epoch_start);
		if oldest >= first {
			return Plan::Idr;
		}
		if self
			.invalidated
			.is_some_and(|(start, end)| start <= oldest && end >= first)
		{
			return Plan::Idr;
		}
		Plan::Invalidate { first, end: next }
	}
	pub fn accepted(&mut self, first: u64, end: u64) {
		self.invalidated = Some(match self.invalidated {
			Some((start, previous_end)) if previous_end >= first && start <= end => {
				(start.min(first), previous_end.max(end))
			},
			_ => (first, end),
		});
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn actual_retention_and_epochs_bound_anchors() {
		assert_eq!(Recovery::new(0).plan(9, 10), Plan::Invalidate { first: 9, end: 10 });
		assert_eq!(Recovery::new(1).plan(9, 10), Plan::Idr);
		let mut r = Recovery::new(4);
		r.reset(100);
		assert_eq!(r.plan(99, 105), Plan::Idr);
		assert_eq!(r.plan(100, 105), Plan::Idr);
		let mut automatic = Recovery::new(0);
		automatic.reset(100);
		assert_eq!(
			automatic.plan(100, 105),
			Plan::Idr,
			"a lost epoch IDR leaves no decoder-safe parent even with automatic SDK retention"
		);
		assert_eq!(r.plan(105, 105), Plan::Idr);
		assert_eq!(r.plan(106, 105), Plan::Idr);
		assert_eq!(r.plan(101, 105), Plan::Idr);
		assert_eq!(r.plan(102, 105), Plan::Invalidate { first: 102, end: 105 });
	}
	#[test]
	fn covered_overlap_failures_and_new_dependencies_keep_recovery_live() {
		let mut r = Recovery::new(5);
		assert_eq!(r.plan(8, 10), Plan::Invalidate { first: 8, end: 10 });
		// Failed SDK calls never enter the successful interval ledger.
		assert_eq!(r.plan(8, 10), Plan::Invalidate { first: 8, end: 10 });
		r.accepted(8, 10);
		assert_eq!(r.plan(9, 10), Plan::Covered);
		assert_eq!(r.plan(8, 11), Plan::Invalidate { first: 8, end: 11 });
		assert_eq!(r.plan(9, 14), Plan::Idr); // no trusted retained anchor
		r.reset(14);
		assert_eq!(r.plan(8, 15), Plan::Idr);
		assert_eq!(r.plan(15, 17), Plan::Invalidate { first: 15, end: 17 });
	}
}

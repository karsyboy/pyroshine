//! WSI presentation bindings: which Vulkan swapchain surface presents which
//! X11 window.
//!
//! `override_window_content` maps an X11 window to a swapchain `wl_surface`
//! (gamescope's per-window `override_surface`). Every live game owns its own
//! binding, so a second game's swapchain never replaces the first game's
//! presentation; the renderer asks which surface presents a given window.
//!
//! A binding lives until its owning swapchain object releases it (see
//! `gamescope_swapchain::release_override`, which protects newer owners of
//! the same surface), its surface dies, or its X11 window is destroyed. Unmap
//! keeps the binding: a window that is withdrawn and mapped again (Wine
//! minimize/restore) presents the same swapchain afterwards.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::IsAlive;

/// One swapchain surface presenting one X11 window (or a standalone native
/// surface when `reported_window` is 0).
#[derive(Debug)]
struct Binding<S> {
	surface: S,
	/// The window the WSI layer reported: the swapchain window, often a
	/// Wine/DXVK child of the WM-visible toplevel.
	reported_window: u32,
	/// The rendered toplevel resolved from `reported_window`; equal to it
	/// until that window or one of its ancestors is in the scene.
	render_window: u32,
	/// Registration order; the newest binding wins when two resolve to the
	/// same window (e.g. a recreated child window before the old swapchain
	/// is destroyed).
	sequence: u64,
}

/// Live WSI presentation bindings, keyed by swapchain surface.
///
/// The set is bounded by the live swapchain surfaces that announced an
/// override: every removal path (owner release, surface death, X11 window
/// destruction) is deterministic.
#[derive(Debug)]
pub(crate) struct WsiBindings<S = WlSurface> {
	bindings: Vec<Binding<S>>,
	next_sequence: u64,
}

impl<S> Default for WsiBindings<S> {
	fn default() -> Self {
		Self {
			bindings: Vec::new(),
			next_sequence: 0,
		}
	}
}

impl<S: Clone + PartialEq + IsAlive> WsiBindings<S> {
	/// Bind `surface` to the X11 window the WSI reported.
	///
	/// A surface presents one window: re-announcing it retargets its binding.
	/// An X11 window has one replacement (gamescope semantics): a different
	/// surface reporting the same window displaces the older binding. Returns
	/// the displaced surfaces so their per-surface presentation state (HDR)
	/// can be cleared; they are never the newly bound surface.
	pub fn bind(&mut self, surface: S, reported_window: u32) -> Vec<S> {
		let mut displaced = Vec::new();
		self.bindings.retain(|binding| {
			if binding.surface == surface {
				return false;
			}
			let replaced = reported_window != 0 && binding.reported_window == reported_window;
			if replaced {
				displaced.push(binding.surface.clone());
			}
			!replaced
		});
		self.next_sequence += 1;
		self.bindings.push(Binding {
			surface,
			reported_window,
			render_window: reported_window,
			sequence: self.next_sequence,
		});
		displaced
	}

	/// Remove the binding presented by `surface`. Returns whether one existed.
	pub fn release(&mut self, surface: &S) -> bool {
		let before = self.bindings.len();
		self.bindings.retain(|binding| &binding.surface != surface);
		self.bindings.len() != before
	}

	/// Remove every binding targeting a destroyed X11 window. X11 ids are
	/// reused, so a stale binding could otherwise present on an unrelated
	/// future window. Returns the removed surfaces.
	pub fn remove_window(&mut self, window: u32) -> Vec<S> {
		if window == 0 {
			return Vec::new();
		}
		let mut removed = Vec::new();
		self.bindings.retain(|binding| {
			let targets = binding.reported_window == window || binding.render_window == window;
			if targets {
				removed.push(binding.surface.clone());
			}
			!targets
		});
		removed
	}

	/// Drop bindings whose surface died without an owner release (client
	/// disconnect). Returns whether anything was removed.
	pub fn prune_dead(&mut self) -> bool {
		let before = self.bindings.len();
		self.bindings.retain(|binding| binding.surface.alive());
		self.bindings.len() != before
	}

	/// Resolve each binding's rendered toplevel.
	///
	/// A binding already pointing at a rendered window is kept without any X11
	/// query. Otherwise the first window of the reported window's ancestor
	/// chain (the window itself first) that is rendered wins; with none
	/// rendered yet the reported window is kept so a later map resolves it.
	/// Returns `(reported, resolved)` for every binding that changed.
	pub fn resolve(
		&mut self,
		is_rendered: impl Fn(u32) -> bool,
		ancestor_chain: impl Fn(u32) -> Vec<u32>,
	) -> Vec<(u32, u32)> {
		let mut changed = Vec::new();
		for binding in &mut self.bindings {
			if binding.reported_window == 0 || is_rendered(binding.render_window) || !binding.surface.alive() {
				continue;
			}
			let resolved = ancestor_chain(binding.reported_window)
				.into_iter()
				.find(|id| is_rendered(*id))
				.unwrap_or(binding.reported_window);
			if resolved != binding.render_window {
				binding.render_window = resolved;
				changed.push((binding.reported_window, resolved));
			}
		}
		changed
	}

	/// The live surface presenting X11 window `window`, if any.
	pub fn surface_for_window(&self, window: u32) -> Option<&S> {
		if window == 0 {
			return None;
		}
		self.bindings
			.iter()
			.filter(|binding| binding.render_window == window && binding.surface.alive())
			.max_by_key(|binding| binding.sequence)
			.map(|binding| &binding.surface)
	}

	/// The newest live standalone surface: a native swapchain without an X11
	/// window, rendered at the output origin.
	pub fn standalone(&self) -> Option<&S> {
		self.bindings
			.iter()
			.filter(|binding| binding.reported_window == 0 && binding.surface.alive())
			.max_by_key(|binding| binding.sequence)
			.map(|binding| &binding.surface)
	}

	/// Whether `surface` presents some window.
	pub fn contains(&self, surface: &S) -> bool {
		self.bindings.iter().any(|binding| &binding.surface == surface)
	}

	/// Every live bound surface, in no particular order.
	pub fn live_surfaces(&self) -> impl Iterator<Item = &S> {
		self.bindings
			.iter()
			.map(|binding| &binding.surface)
			.filter(|surface| surface.alive())
	}

	/// Whether any live binding is presented: a standalone surface, or one
	/// whose window is rendered.
	pub fn any_presented(&self, is_rendered: impl Fn(u32) -> bool) -> bool {
		self.bindings.iter().any(|binding| {
			binding.surface.alive() && (binding.reported_window == 0 || is_rendered(binding.render_window))
		})
	}

	#[cfg(test)]
	fn is_empty(&self) -> bool {
		self.bindings.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::Cell;
	use std::rc::Rc;

	/// A swapchain surface with an externally controlled lifetime.
	#[derive(Clone, Debug)]
	struct Surface(&'static str, Rc<Cell<bool>>);

	impl Surface {
		fn new(name: &'static str) -> Self {
			Self(name, Rc::new(Cell::new(true)))
		}
		fn destroy(&self) {
			self.1.set(false);
		}
	}
	impl PartialEq for Surface {
		fn eq(&self, other: &Self) -> bool {
			self.0 == other.0
		}
	}
	impl IsAlive for Surface {
		fn alive(&self) -> bool {
			self.1.get()
		}
	}

	const VALHEIM: u32 = 0x40_0001;
	const SHOCK: u32 = 0x60_0001;
	const SHOCK_CHILD: u32 = 0x60_0005;

	fn name(surface: Option<&Surface>) -> Option<&'static str> {
		surface.map(|s| s.0)
	}

	#[test]
	fn second_game_does_not_replace_first_games_presentation() {
		let mut wsi = WsiBindings::default();
		let a = Surface::new("valheim");
		let b = Surface::new("shock");
		assert!(wsi.bind(a.clone(), VALHEIM).is_empty());
		assert!(wsi.bind(b.clone(), SHOCK).is_empty());

		// Both remain represented; switching selects each game's own surface.
		for _ in 0..10 {
			assert_eq!(name(wsi.surface_for_window(VALHEIM)), Some("valheim"));
			assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("shock"));
		}
		assert_eq!(wsi.live_surfaces().count(), 2);
	}

	#[test]
	fn swapchain_recreation_lifecycle_keeps_independent_bindings() {
		let mut wsi = WsiBindings::default();
		let a = Surface::new("a");
		let b = Surface::new("b");
		wsi.bind(a.clone(), VALHEIM);
		wsi.bind(b.clone(), SHOCK);

		// Recreating A on the same Vulkan surface re-announces the binding;
		// the owner guard (OverrideOwner) keeps the old chain's destroy from
		// reaching `release`, so A stays bound.
		assert!(wsi.bind(a.clone(), VALHEIM).is_empty());
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), Some("a"));

		// A2 on a new surface for the same window displaces A, not B.
		let a2 = Surface::new("a2");
		let displaced = wsi.bind(a2.clone(), VALHEIM);
		assert_eq!(displaced, vec![a.clone()]);
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), Some("a2"));
		assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("b"));

		// The old owner's late release cannot remove the replacement.
		assert!(!wsi.release(&a));
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), Some("a2"));

		// Closing B leaves A2 valid.
		assert!(wsi.release(&b));
		assert_eq!(name(wsi.surface_for_window(SHOCK)), None);
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), Some("a2"));
	}

	#[test]
	fn destroyed_selected_binding_falls_back_deterministically() {
		let mut wsi = WsiBindings::default();
		// A recreated child window under the same toplevel: both bindings
		// resolve to one rendered window, the newest wins.
		let old = Surface::new("old");
		let new = Surface::new("new");
		wsi.bind(old.clone(), SHOCK);
		wsi.bind(new.clone(), SHOCK_CHILD);
		let rendered = |id| id == SHOCK;
		wsi.resolve(rendered, |id| {
			if id == SHOCK_CHILD {
				vec![SHOCK_CHILD, SHOCK]
			} else {
				vec![id]
			}
		});
		assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("new"));

		// The selected surface dies: the older live binding presents again.
		new.destroy();
		assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("old"));
		assert!(wsi.prune_dead());
		assert!(!wsi.prune_dead(), "pruning is idempotent");

		// With no binding left the window presents its own X11 content.
		old.destroy();
		assert_eq!(name(wsi.surface_for_window(SHOCK)), None);
		assert!(wsi.prune_dead());
		assert!(wsi.is_empty(), "dead bindings are not cached");
	}

	#[test]
	fn unresolved_window_resolves_after_its_toplevel_maps() {
		let mut wsi = WsiBindings::default();
		let surface = Surface::new("dxvk");
		// The swapchain is created on a child window before the toplevel maps.
		wsi.bind(surface.clone(), SHOCK_CHILD);
		let chain = |id| {
			if id == SHOCK_CHILD {
				vec![SHOCK_CHILD, SHOCK]
			} else {
				vec![id]
			}
		};
		assert!(wsi.resolve(|_| false, chain).is_empty());
		assert_eq!(name(wsi.surface_for_window(SHOCK)), None);
		assert!(!wsi.any_presented(|_| false));

		let mapped = |id| id == SHOCK;
		assert_eq!(wsi.resolve(mapped, chain), vec![(SHOCK_CHILD, SHOCK)]);
		assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("dxvk"));
		assert!(wsi.any_presented(mapped));
		// A resolved binding is kept without querying X11 again.
		assert!(
			wsi.resolve(mapped, |_| unreachable!("no X11 query for a rendered binding"))
				.is_empty()
		);
	}

	#[test]
	fn unmap_keeps_binding_but_window_destruction_removes_it() {
		let mut wsi = WsiBindings::default();
		let a = Surface::new("a");
		let b = Surface::new("b");
		wsi.bind(a.clone(), VALHEIM);
		wsi.bind(b.clone(), SHOCK);

		// Unmapped (e.g. Wine withdraw before restore): no longer presented,
		// but the binding survives for the remap.
		assert!(!wsi.any_presented(|id| id == 0));
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), Some("a"));

		// Destroying the window removes only its binding, even while the
		// Vulkan surface is still alive (the X11 id may be reused).
		assert_eq!(wsi.remove_window(VALHEIM), vec![a.clone()]);
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), None);
		assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("b"));
		assert!(wsi.remove_window(VALHEIM).is_empty(), "removal is idempotent");
		assert!(wsi.remove_window(0).is_empty());
	}

	#[test]
	fn surface_retarget_and_standalone_bindings() {
		let mut wsi = WsiBindings::default();
		let surface = Surface::new("s");
		wsi.bind(surface.clone(), VALHEIM);
		// Retargeting moves the one binding of this surface.
		assert!(wsi.bind(surface.clone(), SHOCK).is_empty());
		assert_eq!(name(wsi.surface_for_window(VALHEIM)), None);
		assert_eq!(name(wsi.surface_for_window(SHOCK)), Some("s"));
		assert!(wsi.contains(&surface));

		let native_old = Surface::new("native-old");
		let native_new = Surface::new("native-new");
		wsi.bind(native_old.clone(), 0);
		assert!(
			wsi.bind(native_new.clone(), 0).is_empty(),
			"standalone surfaces never displace each other"
		);
		assert_eq!(name(wsi.standalone()), Some("native-new"));
		assert_eq!(name(wsi.surface_for_window(0)), None);
		wsi.release(&native_new);
		assert_eq!(name(wsi.standalone()), Some("native-old"));
		assert!(wsi.any_presented(|_| false));
	}
}

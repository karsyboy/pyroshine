//! Reporting follows primary compositor focus, independently of input targets.

use moonshine_management::dto::ForegroundApplication;
use smithay::desktop::Window;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tokio::sync::watch;

use super::state::MoonshineCompositor;

fn display_name<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<ForegroundApplication> {
	names
		.into_iter()
		.map(str::trim)
		.find(|name| !name.is_empty())
		.map(|title| ForegroundApplication {
			title: title.to_owned(),
		})
}

fn window_application(window: &Window) -> Option<ForegroundApplication> {
	if let Some(x11) = window.x11_surface() {
		return display_name([x11.title().as_str(), x11.class().as_str(), x11.instance().as_str()]);
	}
	let toplevel = window.toplevel()?;
	// XDG titles/app IDs are role attributes, not double-buffered state.
	with_states(toplevel.wl_surface(), |states| {
		let attributes = states.data_map.get::<XdgToplevelSurfaceData>()?.lock().unwrap();
		display_name([
			attributes.title.as_deref().unwrap_or(""),
			attributes.app_id.as_deref().unwrap_or(""),
		])
	})
}

pub(super) fn publish(
	sender: &watch::Sender<Option<ForegroundApplication>>,
	application: Option<ForegroundApplication>,
) {
	sender.send_if_modified(|current| {
		if *current == application {
			return false;
		}
		*current = application;
		true
	});
}

impl MoonshineCompositor {
	/// Called on focus selection and metadata events, never from rendering.
	pub(super) fn refresh_foreground_application(&self) {
		let application = self
			.focused_window
			.as_ref()
			.filter(|window| {
				self.space.elements().any(|mapped| mapped == *window)
					&& self.window_metadata.get(*window).is_some_and(|meta| {
						!meta.excluded_from_primary_focus() && meta.map_state_viewable && meta.opacity > 0
					})
			})
			.and_then(window_application);
		publish(&self.foreground_tx, application);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn titles_precede_identity_and_empty_metadata_is_normal() {
		assert_eq!(
			display_name([" Grim Dawn ", "steam_app_219990", "wine"]).unwrap().title,
			"Grim Dawn"
		);
		assert_eq!(display_name([" ", "Wine", "wine"]).unwrap().title, "Wine");
		assert_eq!(display_name(["", "", "instance"]).unwrap().title, "instance");
		assert_eq!(
			display_name(["Wayland Game", "org.example.Game"]).unwrap().title,
			"Wayland Game"
		);
		assert_eq!(
			display_name(["", "org.example.Game"]).unwrap().title,
			"org.example.Game"
		);
		assert_eq!(display_name(["", "\t"]), None);
	}

	#[test]
	fn changes_are_retained_and_duplicates_do_not_notify() {
		let (sender, mut receiver) = watch::channel(None);
		let game = display_name(["Grim Dawn"]);
		publish(&sender, game.clone());
		assert!(receiver.has_changed().unwrap());
		assert_eq!(*receiver.borrow_and_update(), game);
		publish(&sender, game.clone());
		assert!(!receiver.has_changed().unwrap());
		// Reporting does not depend on whether a management/UI receiver exists.
		drop(receiver);
		publish(&sender, display_name(["Steam"]));
		let mut receiver = sender.subscribe();
		assert_eq!(receiver.borrow_and_update().as_ref().unwrap().title, "Steam");
		publish(&sender, None);
		assert!(receiver.has_changed().unwrap());
		assert_eq!(*receiver.borrow_and_update(), None);
		publish(&sender, None);
		assert!(!receiver.has_changed().unwrap());
	}
}

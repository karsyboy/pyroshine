//! The application library over the management interface: the catalog view
//! for `GetLibrary` and library events, and its errors.

use std::time::Duration;

use moonshine_management::ManagementError;
use moonshine_management::dto::{LibraryApplication, LibrarySnapshot, LibrarySource};

use super::hub::EventKind;
use super::{Management, json};
use crate::app_scanner::service::LibraryError;
use crate::app_scanner::{CatalogEntry, EntryState, LibraryView, Origin, SourceHealth};

/// Changes closer together than this are sent as one signal.
const COALESCE: Duration = Duration::from_millis(250);

impl From<LibraryError> for ManagementError {
	fn from(error: LibraryError) -> Self {
		match error {
			LibraryError::NotFound(message) => Self::NotFound(message),
			LibraryError::Invalid(message) => Self::Invalid(message),
			LibraryError::Unavailable(message) | LibraryError::Failed(message) => Self::Failed(message),
		}
	}
}

fn title_of(view: &LibraryView, key: &str) -> String {
	view.catalog
		.find_key(key)
		.map_or_else(|| key.to_string(), |entry| entry.application.title.clone())
}

fn application(view: &LibraryView, entry: &CatalogEntry) -> LibraryApplication {
	let (source, source_label, scanner, native_id) = match &entry.origin {
		Origin::Manual { .. } => ("manual".to_string(), "Manual".to_string(), None, None),
		Origin::Discovered {
			scanner,
			kind,
			native_id,
			..
		} => (
			kind.id().to_string(),
			kind.label().to_string(),
			Some(*scanner as u32),
			Some(native_id.clone()),
		),
	};
	let (state, state_detail) = match &entry.state {
		EntryState::Available => ("available", None),
		EntryState::Stale { .. } => (
			"stale",
			Some("Its library is not reachable; offered from the last successful scan.".to_string()),
		),
		EntryState::Hidden => ("hidden", Some("Hidden from clients.".to_string())),
		EntryState::Shadowed { by } => (
			"shadowed",
			Some(format!("Replaced by the manual application '{}'.", title_of(view, by))),
		),
		EntryState::Duplicate { of } if *of == entry.key => (
			"duplicate",
			Some("Another scanner of the same launcher already offers it.".to_string()),
		),
		EntryState::Duplicate { of } => (
			"duplicate",
			Some(format!(
				"A shortcut to '{}', which is offered instead.",
				title_of(view, of)
			)),
		),
	};
	LibraryApplication {
		key: entry.key.clone(),
		id: entry.id,
		title: entry.application.title.clone(),
		manual: entry.is_manual(),
		can_hide: !entry.is_manual()
			&& matches!(
				entry.state,
				EntryState::Available | EntryState::Stale { .. } | EntryState::Hidden
			),
		source,
		source_label,
		scanner,
		native_id,
		state: state.into(),
		offered: entry.state.offered(),
		state_detail,
		artwork: entry.artwork_origin.map(|origin| origin.id().to_string()),
		command: entry.application.command.clone(),
	}
}

/// The management view of the library.
pub(crate) fn snapshot(view: &LibraryView) -> LibrarySnapshot {
	LibrarySnapshot {
		generation: view.catalog.generation,
		scanning: view.scanning,
		auto_refresh: view.auto_refresh,
		poll_interval_secs: view.poll_interval_secs,
		last_refresh_ms: view.last_refresh_ms,
		notice: view.notice.clone(),
		sources: view
			.sources
			.iter()
			.map(|source| LibrarySource {
				scanner: source.scanner as u32,
				kind: source.kind.id().into(),
				label: source.kind.label().into(),
				root: source.root.clone(),
				status: match source.health {
					SourceHealth::Pending => "pending",
					SourceHealth::Ok => "ok",
					SourceHealth::Partial => "partial",
					SourceHealth::Failed => "failed",
				}
				.into(),
				message: source.message.clone(),
				hint: source.hint.clone(),
				applications: source.entries as u32,
				stale_applications: source.stale_entries as u32,
				last_scan_ms: source.last_scan_ms,
				duration_ms: source.duration_ms,
				watched: source.watched,
			})
			.collect(),
		applications: view
			.catalog
			.entries()
			.iter()
			.map(|entry| application(view, entry))
			.collect(),
	}
}

impl Management {
	pub(super) fn library_snapshot(&self) -> LibrarySnapshot {
		snapshot(&self.library.view())
	}
}

/// Publish library changes while someone watches; a viewer reads the
/// complete snapshot when it starts, so nothing is lost while nobody does.
pub(super) async fn follow_library(management: &Management) {
	let mut views = management.library.subscribe();
	let mut observed = management.hub.observed();
	loop {
		tokio::select! {
			changed = views.changed() => if changed.is_err() { return },
			changed = observed.changed() => {
				if changed.is_err() { return }
				continue;
			},
		}
		if !*observed.borrow() {
			continue;
		}
		tokio::time::sleep(COALESCE).await;
		let view = views.borrow_and_update().clone();
		management.hub.publish(EventKind::Library, json(&snapshot(&view)));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::app_scanner::Library;
	use crate::session::application::ApplicationConfig;

	#[test]
	fn manual_entries_are_reported_and_cannot_be_hidden() {
		let library = Library::fixed(&[ApplicationConfig {
			title: "Steam".into(),
			command: vec!["steam".into()],
			..Default::default()
		}]);
		let snapshot = snapshot(&library.view());
		assert!(!snapshot.scanning);
		let steam = &snapshot.applications[0];
		assert_eq!(
			(
				steam.key.as_str(),
				steam.source.as_str(),
				steam.manual,
				steam.can_hide,
				steam.offered
			),
			("manual:0", "manual", true, false, true)
		);
		assert_eq!(steam.id, crate::app_scanner::legacy_id("Steam"));
	}
}

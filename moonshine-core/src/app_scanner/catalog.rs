//! The application catalog: manual entries and discovered applications
//! reconciled into one immutable snapshot.
//!
//! A snapshot is published as a whole and never modified: request handlers
//! read the one current when they start, so `/applist`, `/appasset` and
//! `/launch` always see a consistent catalog. Launching copies the entry's
//! [`ApplicationConfig`] into the session, so a running session never
//! depends on the catalog again: refreshing, hiding or removing its entry
//! cannot stop, restart or reconfigure it.
//!
//! Reconciliation rules, in order:
//!
//! 1. Manual `[[application]]` entries come first and keep the ID earlier
//!    releases gave them ([`legacy_id`] of the title).
//! 2. A discovered identity (`steam:620`) found twice (two scanners of the
//!    same launcher) is offered once; the later one is a duplicate.
//! 3. A launcher shortcut (a desktop entry running `steam://rungameid/620`)
//!    is a duplicate of the application it starts when that is in the catalog.
//! 4. A discovered application with the title of a manual entry is shadowed
//!    by it: the manual entry is the user's override (command, box art).
//! 5. Remaining entries get stable IDs (`IdentityStore::assign`); the
//!    user's visibility choice hides some of them.
//! 6. Equal titles from different launchers are told apart by a suffix
//!    (`Hades (Steam)`, `Hades (Heroic)`), since Moonlight shows only titles.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::identity::{IdentityStore, derived_id, legacy_id, title_key};
use super::{ArtworkOrigin, Discovered, ScannerKind};
use crate::session::application::ApplicationConfig;

/// Where an entry comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
	/// The `index`th `[[application]]` of the configuration.
	Manual { index: usize },
	/// Found by the `scanner`th `[[application_scanner]]`.
	Discovered {
		scanner: usize,
		kind: ScannerKind,
		/// Launcher-native identity (Steam app ID, Lutris slug, ...).
		native_id: String,
		/// The unit (library folder, file, directory) it was found in.
		unit: String,
	},
}

/// Whether and why an entry is offered to clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryState {
	Available,
	/// Its library could not be read recently; offered from the last
	/// successful scan.
	Stale {
		since_ms: u64,
	},
	/// Hidden by the user.
	Hidden,
	/// A manual entry with the same title replaces it.
	Shadowed {
		by: String,
	},
	/// Another entry (`of`) offers the same application.
	Duplicate {
		of: String,
	},
}

impl EntryState {
	/// Whether clients see the entry in `/applist` and may launch it.
	pub fn offered(&self) -> bool {
		matches!(self, Self::Available | Self::Stale { .. })
	}
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogEntry {
	/// Stable identity: `manual:<index>` or `<launcher>:<native id>`.
	pub key: String,
	/// Moonlight application ID. Zero for entries never offered.
	pub id: i32,
	pub origin: Origin,
	/// What a launch runs. The title is the one clients are shown.
	pub application: ApplicationConfig,
	/// Box art candidates, best first (`application.boxart` leads).
	pub artwork: Vec<PathBuf>,
	pub artwork_origin: Option<ArtworkOrigin>,
	pub state: EntryState,
}

impl CatalogEntry {
	pub fn is_manual(&self) -> bool {
		matches!(self.origin, Origin::Manual { .. })
	}
}

/// One published catalog.
#[derive(Debug, Default)]
pub struct CatalogSnapshot {
	/// Increments with every change of the entries.
	pub generation: u64,
	entries: Vec<CatalogEntry>,
	offered: HashMap<i32, usize>,
}

impl PartialEq for CatalogSnapshot {
	fn eq(&self, other: &Self) -> bool {
		self.entries == other.entries
	}
}

impl CatalogSnapshot {
	pub fn new(generation: u64, entries: Vec<CatalogEntry>) -> Self {
		let mut offered = HashMap::new();
		for (index, entry) in entries.iter().enumerate() {
			if entry.state.offered() {
				offered.entry(entry.id).or_insert(index);
			}
		}
		Self {
			generation,
			entries,
			offered,
		}
	}

	/// A catalog of manual entries only.
	pub fn from_applications(applications: &[ApplicationConfig]) -> Self {
		let mut identity = IdentityStore::in_memory();
		Self::new(0, reconcile(applications, &[], &mut identity, &|_| None, 0))
	}

	/// Every entry, including those not offered (for the management view).
	pub fn entries(&self) -> &[CatalogEntry] {
		&self.entries
	}

	/// Entries clients see, in catalog order.
	pub fn offered(&self) -> impl Iterator<Item = &CatalogEntry> {
		self.entries.iter().filter(|entry| entry.state.offered())
	}

	/// The offered entry with Moonlight ID `id`.
	pub fn find(&self, id: i32) -> Option<&CatalogEntry> {
		self.offered.get(&id).map(|&index| &self.entries[index])
	}

	pub fn find_key(&self, key: &str) -> Option<&CatalogEntry> {
		self.entries.iter().find(|entry| entry.key == key)
	}

	/// The launcher identities (`steam:892970`) each offered entry starts,
	/// keyed by entry key, in catalog order: a discovered entry's own key, and
	/// for a manual entry the keys of the discovered entries it shadows, since
	/// it is the user's override of that game. Manual keys are positions in
	/// the configuration, not identities, and are never reported.
	///
	/// This lets a client find "the entry for Steam app 892970" without
	/// matching titles, which can be suffixed (`Hades (Steam)`) or overridden.
	pub fn launcher_identities(&self) -> HashMap<&str, Vec<&str>> {
		let mut identities: HashMap<&str, Vec<&str>> = HashMap::new();
		for entry in &self.entries {
			match (&entry.origin, &entry.state) {
				(Origin::Discovered { .. }, state) if state.offered() => {
					identities
						.entry(entry.key.as_str())
						.or_default()
						.push(entry.key.as_str());
				},
				(Origin::Discovered { .. }, EntryState::Shadowed { by })
					if self.find_key(by).is_some_and(|owner| owner.state.offered()) =>
				{
					identities.entry(by.as_str()).or_default().push(entry.key.as_str());
				},
				_ => {},
			}
		}
		identities
	}
}

/// One unit's current entries.
pub(crate) struct UnitInput<'a> {
	pub key: &'a str,
	pub entries: &'a [Discovered],
	/// Set while the unit is offered from an earlier scan.
	pub stale_since: Option<u64>,
}

/// One scanner's current entries.
pub(crate) struct SourceInput<'a> {
	pub scanner: usize,
	pub kind: ScannerKind,
	pub units: Vec<UnitInput<'a>>,
}

/// Build the catalog entries (see the module documentation for the rules).
/// `title_icon` finds fallback box art by title for entries without any.
pub(crate) fn reconcile(
	manual: &[ApplicationConfig],
	sources: &[SourceInput<'_>],
	identity: &mut IdentityStore,
	title_icon: &dyn Fn(&str) -> Option<PathBuf>,
	now_ms: u64,
) -> Vec<CatalogEntry> {
	let mut entries = Vec::new();
	let mut reserved = HashSet::new();
	let mut manual_titles: HashMap<String, String> = HashMap::new();

	for (index, application) in manual.iter().enumerate() {
		let key = format!("manual:{index}");
		let mut id = legacy_id(&application.title);
		let mut attempt = 0;
		while reserved.contains(&id) {
			// A repeated title used to repeat the ID, which made the second
			// entry unreachable; it now gets an ID of its own.
			id = derived_id(&format!("manual:{}:{index}", application.title), attempt);
			attempt += 1;
		}
		reserved.insert(id);
		manual_titles
			.entry(title_key(&application.title))
			.or_insert_with(|| key.clone());
		let (artwork, artwork_origin) = match &application.boxart {
			Some(boxart) => (vec![boxart.clone()], Some(ArtworkOrigin::Manual)),
			None => match title_icon(&application.title) {
				Some(icon) => (vec![icon], Some(ArtworkOrigin::IconTheme)),
				None => (Vec::new(), None),
			},
		};
		let mut application = application.clone();
		application.boxart = artwork.first().cloned();
		entries.push(CatalogEntry {
			key,
			id,
			origin: Origin::Manual { index },
			application,
			artwork,
			artwork_origin,
			state: EntryState::Available,
		});
	}

	// Discovered entries in source and unit order, sorted within a unit so
	// the catalog (and who gets a contested ID first) does not depend on
	// directory order.
	let mut discovered: Vec<(CatalogEntry, &Discovered)> = Vec::new();
	for source in sources {
		for unit in &source.units {
			let mut items: Vec<&Discovered> = unit.entries.iter().collect();
			items.sort_by(|a, b| {
				title_key(&a.application.title)
					.cmp(&title_key(&b.application.title))
					.then_with(|| a.native_id.cmp(&b.native_id))
			});
			for item in items {
				let mut artwork: Vec<PathBuf> = item
					.application
					.boxart
					.iter()
					.chain(item.artwork_fallbacks.iter())
					.cloned()
					.collect();
				let mut artwork_origin = item.artwork_origin;
				if artwork.is_empty()
					&& let Some(icon) = title_icon(&item.application.title)
				{
					artwork.push(icon);
					artwork_origin = Some(ArtworkOrigin::IconTheme);
				}
				let mut application = item.application.clone();
				application.boxart = artwork.first().cloned();
				discovered.push((
					CatalogEntry {
						key: format!("{}:{}", source.kind.id(), item.native_id),
						id: 0,
						origin: Origin::Discovered {
							scanner: source.scanner,
							kind: source.kind,
							native_id: item.native_id.clone(),
							unit: unit.key.to_string(),
						},
						application,
						artwork,
						artwork_origin,
						state: match unit.stale_since {
							Some(since_ms) => EntryState::Stale { since_ms },
							None => EntryState::Available,
						},
					},
					item,
				));
			}
		}
	}

	// Same identity twice: the first one stands.
	let mut owners: HashMap<String, String> = HashMap::new();
	for (entry, item) in &mut discovered {
		if owners.contains_key(&entry.key) {
			entry.state = EntryState::Duplicate { of: entry.key.clone() };
			continue;
		}
		owners.insert(entry.key.clone(), entry.key.clone());
		let kind = entry.key.split(':').next().unwrap_or_default().to_string();
		for alias in &item.aliases {
			let alias = if alias.contains(':') {
				alias.clone()
			} else {
				format!("{kind}:{alias}")
			};
			owners.entry(alias).or_insert_with(|| entry.key.clone());
		}
	}
	for (entry, item) in &mut discovered {
		if !entry.state.offered() {
			continue;
		}
		if let Some(target) = item.launches.as_ref().and_then(|target| owners.get(target))
			&& *target != entry.key
		{
			entry.state = EntryState::Duplicate { of: target.clone() };
			continue;
		}
		if let Some(by) = manual_titles.get(&title_key(&entry.application.title)) {
			entry.state = EntryState::Shadowed { by: by.clone() };
		}
	}

	let present: Vec<(String, String)> = discovered
		.iter()
		.filter(|(entry, _)| entry.state.offered())
		.map(|(entry, _)| (entry.key.clone(), entry.application.title.clone()))
		.collect();
	let ids = identity.assign(&present, &reserved, now_ms);
	for (entry, _) in &mut discovered {
		match ids.get(&entry.key) {
			Some(id) => {
				entry.id = *id;
				if identity.is_hidden(&entry.key) {
					entry.state = EntryState::Hidden;
				}
			},
			None => entry.id = identity.record(&entry.key).map_or(0, |record| record.id),
		}
	}

	// Tell equal titles apart (offered discovered entries only).
	let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
	for (index, (entry, _)) in discovered.iter().enumerate() {
		if entry.state.offered() {
			by_title
				.entry(title_key(&entry.application.title))
				.or_default()
				.push(index);
		}
	}
	for indices in by_title.into_values().filter(|indices| indices.len() > 1) {
		let kinds: Vec<ScannerKind> = indices
			.iter()
			.map(|&index| match &discovered[index].0.origin {
				Origin::Discovered { kind, .. } => *kind,
				Origin::Manual { .. } => unreachable!("only discovered entries"),
			})
			.collect();
		for (position, &index) in indices.iter().enumerate() {
			let kind = kinds[position];
			let same_kind = kinds.iter().filter(|other| **other == kind).count() > 1;
			let entry = &mut discovered[index].0;
			let suffix = match (&entry.origin, same_kind) {
				(Origin::Discovered { native_id, .. }, true) => format!("{} {native_id}", kind.short_label()),
				_ => kind.short_label().to_string(),
			};
			entry.application.title = format!("{} ({suffix})", entry.application.title);
		}
	}

	entries.extend(discovered.into_iter().map(|(entry, _)| entry));
	entries
}

#[cfg(test)]
mod tests {
	use super::*;

	fn app(title: &str, command: &str) -> ApplicationConfig {
		ApplicationConfig {
			title: title.into(),
			command: vec![command.into()],
			..Default::default()
		}
	}

	fn found(native_id: &str, title: &str) -> Discovered {
		Discovered::new(native_id, app(title, &format!("run {native_id}")))
	}

	fn catalog(
		manual: &[ApplicationConfig],
		sources: &[(ScannerKind, Vec<Discovered>)],
		identity: &mut IdentityStore,
	) -> CatalogSnapshot {
		let inputs: Vec<SourceInput> = sources
			.iter()
			.enumerate()
			.map(|(scanner, (kind, entries))| SourceInput {
				scanner,
				kind: *kind,
				units: vec![UnitInput {
					key: "unit",
					entries,
					stale_since: None,
				}],
			})
			.collect();
		CatalogSnapshot::new(1, reconcile(manual, &inputs, identity, &|_| None, 1))
	}

	#[test]
	fn manual_entries_keep_their_ids_and_come_first() {
		let mut identity = IdentityStore::in_memory();
		let snapshot = catalog(
			&[app("Steam", "steam"), app("Desktop", "plasma")],
			&[(ScannerKind::Steam, vec![found("620", "Portal 2")])],
			&mut identity,
		);
		let offered: Vec<_> = snapshot
			.offered()
			.map(|entry| (entry.application.title.as_str(), entry.id))
			.collect();
		assert_eq!(
			offered,
			vec![
				("Steam", legacy_id("Steam")),
				("Desktop", legacy_id("Desktop")),
				("Portal 2", legacy_id("Portal 2"))
			]
		);
		assert_eq!(snapshot.find(legacy_id("Portal 2")).unwrap().key, "steam:620");
	}

	#[test]
	fn a_manual_entry_overrides_a_discovered_one_with_the_same_title() {
		let mut identity = IdentityStore::in_memory();
		let mut custom = app("Portal 2", "custom-portal");
		custom.boxart = Some("/art/portal.png".into());
		let snapshot = catalog(
			&[custom],
			&[(ScannerKind::Steam, vec![found("620", "portal 2 ")])],
			&mut identity,
		);
		let offered: Vec<_> = snapshot.offered().collect();
		assert_eq!(offered.len(), 1);
		assert_eq!(offered[0].application.command, vec!["custom-portal"]);
		assert_eq!(offered[0].artwork_origin, Some(ArtworkOrigin::Manual));
		assert_eq!(
			snapshot.find_key("steam:620").unwrap().state,
			EntryState::Shadowed { by: "manual:0".into() }
		);
	}

	#[test]
	fn launcher_shortcuts_and_repeated_identities_are_offered_once() {
		let mut identity = IdentityStore::in_memory();
		let mut shortcut = found("Portal 2.desktop", "Portal 2");
		shortcut.launches = Some("steam:620".into());
		let mut lutris_shortcut = found("quake.desktop", "Quake");
		lutris_shortcut.launches = Some("lutris:id:7".into());
		let mut quake = found("quake", "Quake");
		quake.aliases.push("lutris:id:7".into());
		let mut orphan = found("hl.desktop", "Half-Life");
		orphan.launches = Some("steam:70".into());
		let snapshot = catalog(
			&[],
			&[
				(ScannerKind::Steam, vec![found("620", "Portal 2")]),
				(ScannerKind::Lutris, vec![quake]),
				(ScannerKind::Desktop, vec![shortcut, lutris_shortcut, orphan]),
				(ScannerKind::Steam, vec![found("620", "Portal 2")]),
			],
			&mut identity,
		);
		let offered: Vec<_> = snapshot.offered().map(|entry| entry.key.as_str()).collect();
		assert_eq!(offered, vec!["steam:620", "lutris:quake", "desktop:hl.desktop"]);
		assert_eq!(
			snapshot.find_key("desktop:quake.desktop").unwrap().state,
			EntryState::Duplicate {
				of: "lutris:quake".into()
			}
		);
		assert_eq!(snapshot.entries().len(), 6);
	}

	#[test]
	fn a_bottles_desktop_shortcut_is_offered_once() {
		let mut identity = IdentityStore::in_memory();
		let mut hades = found("abc-123", "Hades");
		hades
			.aliases
			.push(super::super::bottles::program_alias("Gaming", "Hades"));
		let mut shortcut = found("Gaming--Hades.desktop", "Hades");
		shortcut.launches = super::super::launcher_target(&[
			"bottles-cli".into(),
			"run".into(),
			"-p".into(),
			"Hades".into(),
			"-b".into(),
			"Gaming".into(),
		]);
		let snapshot = catalog(
			&[],
			&[
				(ScannerKind::Bottles, vec![hades]),
				(ScannerKind::Desktop, vec![shortcut]),
			],
			&mut identity,
		);
		let offered: Vec<_> = snapshot.offered().map(|entry| entry.key.as_str()).collect();
		assert_eq!(offered, vec!["bottles:abc-123"]);
		assert_eq!(
			snapshot.find_key("desktop:Gaming--Hades.desktop").unwrap().state,
			EntryState::Duplicate {
				of: "bottles:abc-123".into()
			}
		);
	}

	#[test]
	fn equal_titles_from_different_launchers_are_told_apart_with_distinct_ids() {
		let mut identity = IdentityStore::in_memory();
		let snapshot = catalog(
			&[],
			&[
				(
					ScannerKind::Steam,
					vec![
						found("1145360", "Hades"),
						found("1", "Soundtrack"),
						found("2", "Soundtrack"),
					],
				),
				(ScannerKind::Heroic, vec![found("legendary/Min", "Hades")]),
			],
			&mut identity,
		);
		let mut titles: Vec<_> = snapshot
			.offered()
			.map(|entry| entry.application.title.clone())
			.collect();
		titles.sort();
		assert_eq!(
			titles,
			vec![
				"Hades (Heroic)",
				"Hades (Steam)",
				"Soundtrack (Steam 1)",
				"Soundtrack (Steam 2)"
			]
		);
		let ids: HashSet<i32> = snapshot.offered().map(|entry| entry.id).collect();
		assert_eq!(ids.len(), 4);
		assert_eq!(snapshot.find_key("steam:1145360").unwrap().id, legacy_id("Hades"));
	}

	#[test]
	fn hidden_entries_keep_their_id_but_are_not_offered() {
		let mut identity = IdentityStore::in_memory();
		let first = catalog(
			&[],
			&[(ScannerKind::Steam, vec![found("620", "Portal 2")])],
			&mut identity,
		);
		let id = first.find_key("steam:620").unwrap().id;
		identity.set_hidden("steam:620", true);
		let hidden = catalog(
			&[],
			&[(ScannerKind::Steam, vec![found("620", "Portal 2")])],
			&mut identity,
		);
		let entry = hidden.find_key("steam:620").unwrap();
		assert_eq!((entry.id, &entry.state), (id, &EntryState::Hidden));
		assert!(hidden.find(id).is_none());
		assert_eq!(hidden.offered().count(), 0);
	}

	/// Repeated refreshes with the same, a renamed or a reordered library
	/// keep every ID.
	#[test]
	fn ids_are_stable_across_refreshes_renames_and_reordering() {
		let mut identity = IdentityStore::in_memory();
		let ids = |snapshot: &CatalogSnapshot| -> HashMap<String, i32> {
			snapshot.offered().map(|entry| (entry.key.clone(), entry.id)).collect()
		};
		let library = vec![
			found("620", "Portal 2"),
			found("400", "Portal"),
			found("70", "Half-Life"),
		];
		let first = ids(&catalog(&[], &[(ScannerKind::Steam, library.clone())], &mut identity));
		for _ in 0..3 {
			assert_eq!(
				ids(&catalog(&[], &[(ScannerKind::Steam, library.clone())], &mut identity)),
				first
			);
		}
		let mut changed = library.clone();
		changed.reverse();
		changed[0].application.title = "Half-Life: Source".into();
		assert_eq!(
			ids(&catalog(&[], &[(ScannerKind::Steam, changed)], &mut identity)),
			first
		);
	}

	#[test]
	fn stale_units_are_still_offered_and_title_icons_fill_missing_art() {
		let mut identity = IdentityStore::in_memory();
		let entries = vec![found("620", "Portal 2")];
		let inputs = [SourceInput {
			scanner: 0,
			kind: ScannerKind::Steam,
			units: vec![UnitInput {
				key: "/mnt/games",
				entries: &entries,
				stale_since: Some(5),
			}],
		}];
		let icon = |title: &str| (title == "Portal 2").then(|| PathBuf::from("/icons/portal-2.png"));
		let snapshot = CatalogSnapshot::new(1, reconcile(&[], &inputs, &mut identity, &icon, 1));
		let entry = snapshot.offered().next().unwrap();
		assert_eq!(entry.state, EntryState::Stale { since_ms: 5 });
		assert_eq!(entry.application.boxart, Some(PathBuf::from("/icons/portal-2.png")));
		assert_eq!(entry.artwork_origin, Some(ArtworkOrigin::IconTheme));
	}

	#[test]
	fn repeated_manual_titles_become_reachable() {
		let snapshot = CatalogSnapshot::from_applications(&[app("Game", "a"), app("Game", "b")]);
		let ids: Vec<i32> = snapshot.offered().map(|entry| entry.id).collect();
		assert_eq!(ids[0], legacy_id("Game"));
		assert_ne!(ids[1], ids[0]);
		assert_eq!(snapshot.find(ids[1]).unwrap().application.command, vec!["b"]);
	}

	/// Clients find a game by launcher identity, never by title: equal
	/// titles stay apart, a manual override answers for the game it shadows,
	/// and hidden or duplicate entries report nothing.
	#[test]
	fn launcher_identities_follow_offered_entries_and_overrides() {
		let mut identity = IdentityStore::in_memory();
		let mut shortcut = found("Valheim.desktop", "Valheim");
		shortcut.launches = Some("steam:892970".into());
		let snapshot = catalog(
			&[app("Portal 2", "custom-portal"), app("Desktop", "plasma")],
			&[
				(
					ScannerKind::Steam,
					vec![
						found("892970", "Valheim"),
						found("620", "Portal 2"),
						found("1145360", "Hades"),
						found("1145361", "Hades"),
					],
				),
				(ScannerKind::Heroic, vec![found("legendary/Hades", "Hades")]),
				(ScannerKind::Desktop, vec![shortcut]),
			],
			&mut identity,
		);
		let identities = snapshot.launcher_identities();
		let of = |key: &str| identities.get(key).cloned().unwrap_or_default();

		assert_eq!(of("steam:892970"), vec!["steam:892970"]);
		// The manual "Portal 2" is what launching Steam app 620 starts.
		assert_eq!(of("manual:0"), vec!["steam:620"]);
		assert_eq!(of("steam:620"), Vec::<&str>::new());
		assert_eq!(of("manual:1"), Vec::<&str>::new());
		assert_eq!(of("desktop:Valheim.desktop"), Vec::<&str>::new());
		// Same title, distinct identities and IDs.
		assert_eq!(of("steam:1145360"), vec!["steam:1145360"]);
		assert_eq!(of("steam:1145361"), vec!["steam:1145361"]);
		assert_eq!(of("heroic:legendary/Hades"), vec!["heroic:legendary/Hades"]);
		let id = |key: &str| snapshot.find_key(key).unwrap().id;
		assert_ne!(id("steam:1145360"), id("steam:1145361"));
	}

	#[test]
	fn hidden_entries_report_no_launcher_identity() {
		let mut identity = IdentityStore::in_memory();
		let sources = [(ScannerKind::Steam, vec![found("892970", "Valheim")])];
		catalog(&[], &sources, &mut identity);
		identity.set_hidden("steam:892970", true);
		let snapshot = catalog(&[], &sources, &mut identity);
		assert_eq!(snapshot.find_key("steam:892970").unwrap().state, EntryState::Hidden);
		assert!(snapshot.launcher_identities().is_empty());
	}
}

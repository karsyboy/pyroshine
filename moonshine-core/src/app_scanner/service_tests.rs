//! Library worker behavior against real launcher metadata in temporary
//! directories: automatic refresh, failure retention, persistence and bounds.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::app_scanner::heroic::HeroicApplicationScannerConfig;
use crate::app_scanner::steam::tests::{config as steam_config, install, steam_root};
use crate::app_scanner::{EntryState, legacy_id};

const WAIT: Duration = Duration::from_secs(10);

fn fast() -> Timing {
	Timing {
		quiet: Duration::from_millis(100),
		max_delay: Duration::from_millis(500),
		min_interval: Duration::ZERO,
		retry: vec![Duration::from_millis(200); 5],
	}
}

fn paths(directory: &Path) -> LibraryPaths {
	LibraryPaths {
		identity: Some(directory.join("state/library.toml")),
		retained: Some(directory.join("cache/library-cache.json")),
		artwork: None,
	}
}

fn manual() -> Vec<ApplicationConfig> {
	vec![ApplicationConfig {
		title: "Steam".into(),
		command: vec!["steam".into(), "steam://open/bigpicture".into()],
		..Default::default()
	}]
}

fn start(directory: &Path, scanners: Vec<ApplicationScannerConfig>, options: LibraryConfig) -> Library {
	Library::start_with(manual(), scanners, options, paths(directory), fast())
}

fn steam(root: &Path) -> ApplicationScannerConfig {
	ApplicationScannerConfig::Steam(steam_config(root))
}

/// Titles offered to clients, sorted.
fn offered(library: &Library) -> Vec<String> {
	let mut titles: Vec<String> = library
		.catalog()
		.offered()
		.map(|entry| entry.application.title.clone())
		.collect();
	titles.sort();
	titles
}

async fn until(library: &Library, what: &str, condition: impl Fn(&LibraryView) -> bool) -> Arc<LibraryView> {
	let mut views = library.subscribe();
	match tokio::time::timeout(WAIT, views.wait_for(|view| condition(view))).await {
		Ok(Ok(view)) => view.clone(),
		_ => panic!("timed out waiting for {what}; last view: {:#?}", library.view()),
	}
}

async fn until_offered(library: &Library, titles: &[&str]) {
	let mut expected: Vec<String> = titles.iter().map(|title| title.to_string()).collect();
	expected.sort();
	until(library, &format!("{expected:?}"), |view| {
		let mut titles: Vec<String> = view
			.catalog
			.offered()
			.map(|entry| entry.application.title.clone())
			.collect();
		titles.sort();
		titles == expected && !view.scanning
	})
	.await;
}

fn id_of(library: &Library, title: &str) -> i32 {
	library
		.catalog()
		.offered()
		.find(|entry| entry.application.title == title)
		.unwrap_or_else(|| panic!("{title} offered"))
		.id
}

#[tokio::test]
async fn installing_and_removing_games_updates_the_catalog_without_a_restart() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	steam_root(&root, &[&root]);
	install(&root, 620, "Portal 2");

	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	assert_eq!(offered(&library), vec!["Portal 2", "Steam"]);
	let portal2 = id_of(&library, "Portal 2");
	assert_eq!(portal2, legacy_id("Portal 2"), "upgrades keep the IDs clients know");
	let generation = library.catalog().generation;

	install(&root, 400, "Portal");
	until_offered(&library, &["Steam", "Portal 2", "Portal"]).await;
	assert!(library.catalog().generation > generation);
	assert_eq!(id_of(&library, "Portal 2"), portal2);

	std::fs::remove_file(root.join("steamapps/appmanifest_620.acf")).unwrap();
	until_offered(&library, &["Steam", "Portal"]).await;
	assert_eq!(
		id_of(&library, "Steam"),
		legacy_id("Steam"),
		"manual entries are untouched"
	);

	// Reinstalling restores the same ID.
	install(&root, 620, "Portal 2");
	until_offered(&library, &["Steam", "Portal 2", "Portal"]).await;
	assert_eq!(id_of(&library, "Portal 2"), portal2);
}

#[tokio::test]
async fn renamed_games_keep_their_id() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	steam_root(&root, &[&root]);
	install(&root, 620, "Portal 2");
	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	let id = id_of(&library, "Portal 2");

	install(&root, 620, "Portal 2: Definitive Edition");
	until_offered(&library, &["Steam", "Portal 2: Definitive Edition"]).await;
	assert_eq!(id_of(&library, "Portal 2: Definitive Edition"), id);
}

#[tokio::test]
async fn library_folders_added_and_removed_in_steam_are_followed() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	let second = directory.path().join("SteamLibrary");
	steam_root(&root, &[&root]);
	install(&root, 620, "Portal 2");
	std::fs::create_dir_all(second.join("steamapps")).unwrap();
	install(&second, 70, "Half-Life");

	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	assert_eq!(offered(&library), vec!["Portal 2", "Steam"]);

	steam_root(&root, &[&root, &second]);
	until_offered(&library, &["Steam", "Portal 2", "Half-Life"]).await;
	let half_life = id_of(&library, "Half-Life");

	// A game installed into the new library is noticed too (it is watched).
	install(&second, 220, "Half-Life 2");
	until_offered(&library, &["Steam", "Portal 2", "Half-Life", "Half-Life 2"]).await;

	steam_root(&root, &[&root]);
	until_offered(&library, &["Steam", "Portal 2"]).await;
	steam_root(&root, &[&root, &second]);
	until_offered(&library, &["Steam", "Portal 2", "Half-Life", "Half-Life 2"]).await;
	assert_eq!(id_of(&library, "Half-Life"), half_life);
}

/// A library Steam still lists but that cannot be read (an unmounted drive)
/// keeps its games, marked stale, until it is back.
#[tokio::test]
async fn temporarily_inaccessible_storage_keeps_the_last_known_games() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	let drive = directory.path().join("drive");
	steam_root(&root, &[&root, &drive]);
	install(&root, 620, "Portal 2");
	install(&drive, 70, "Half-Life");
	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	let half_life = id_of(&library, "Half-Life");

	let unmounted = directory.path().join("drive-unmounted");
	std::fs::rename(&drive, &unmounted).unwrap();
	library.rescan().unwrap();
	let view = until(&library, "a partial scan", |view| {
		view.sources[0].health == SourceHealth::Partial && !view.scanning
	})
	.await;
	assert_eq!(view.sources[0].stale_entries, 1);
	assert!(
		view.sources[0]
			.hint
			.as_deref()
			.unwrap()
			.contains("last successful scan")
	);
	assert_eq!(offered(&library), vec!["Half-Life", "Portal 2", "Steam"]);
	let entry = library.catalog().find(half_life).unwrap().state.clone();
	assert!(matches!(entry, EntryState::Stale { .. }), "{entry:?}");

	std::fs::rename(&unmounted, &drive).unwrap();
	library.rescan().unwrap();
	until(&library, "recovery", |view| {
		view.sources[0].health == SourceHealth::Ok && !view.scanning
	})
	.await;
	assert_eq!(library.catalog().find(half_life).unwrap().state, EntryState::Available);
}

/// A half-written library list fails the scan; the catalog stays as it was,
/// and the next write (or retry) brings the change in.
#[tokio::test]
async fn corrupt_metadata_never_erases_the_catalog_and_is_retried() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	steam_root(&root, &[&root]);
	install(&root, 620, "Portal 2");
	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);

	let vdf = root.join("steamapps/libraryfolders.vdf");
	let complete = std::fs::read_to_string(&vdf).unwrap();
	std::fs::write(&vdf, &complete[..complete.len() / 2]).unwrap();
	let view = until(&library, "a failed scan", |view| {
		view.sources[0].health == SourceHealth::Failed && !view.scanning
	})
	.await;
	assert!(
		view.sources[0]
			.message
			.as_deref()
			.unwrap()
			.contains("library folder list")
	);
	assert_eq!(offered(&library), vec!["Portal 2", "Steam"]);

	// Steam finishes writing (the retry, not only the event, picks it up).
	install(&root, 400, "Portal");
	std::fs::write(&vdf, complete).unwrap();
	until_offered(&library, &["Steam", "Portal 2", "Portal"]).await;
	assert_eq!(library.view().sources[0].health, SourceHealth::Ok);
}

#[tokio::test]
async fn half_written_heroic_libraries_keep_their_games() {
	let directory = tempfile::tempdir().unwrap();
	let heroic = directory.path().join("heroic");
	std::fs::create_dir_all(heroic.join("store_cache")).unwrap();
	let library_file = heroic.join("store_cache/legendary_library.json");
	let game = |title: &str| {
		format!(
			r#"{{"library": [{{"app_name": "id", "title": "{title}", "runner": "legendary", "is_installed": true}}]}}"#
		)
	};
	std::fs::write(&library_file, game("Epic Game")).unwrap();
	let scanner = ApplicationScannerConfig::Heroic(HeroicApplicationScannerConfig {
		config_dir: heroic.clone(),
		command: vec![
			"heroic".into(),
			"heroic://launch?appName={app_name}&runner={runner}".into(),
		],
		pre_command: vec![],
		post_command: vec![],
		stdout: None,
		stderr: None,
		launch_timeout_secs: 2,
		frame_limit: Default::default(),
	});
	let library = start(directory.path(), vec![scanner], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	let id = id_of(&library, "Epic Game");

	std::fs::write(&library_file, r#"{"library": [{"app_name": "id", "ti"#).unwrap();
	until(&library, "a partial scan", |view| {
		view.sources[0].health == SourceHealth::Failed && !view.scanning
	})
	.await;
	assert_eq!(offered(&library), vec!["Epic Game", "Steam"]);

	std::fs::write(&library_file, game("Epic Game: Renamed")).unwrap();
	until_offered(&library, &["Steam", "Epic Game: Renamed"]).await;
	assert_eq!(id_of(&library, "Epic Game: Renamed"), id);
}

/// Restarting (here: dropping the library while a rescan is queued) keeps
/// IDs, and a library unreadable at the next start is offered from the
/// retained results of the previous run.
#[tokio::test]
async fn a_restart_keeps_ids_and_retained_games() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	let drive = directory.path().join("drive");
	steam_root(&root, &[&root, &drive]);
	install(&root, 1, "Hades");
	install(&drive, 2, "Hades");
	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	let ids: Vec<(String, i32)> = library
		.catalog()
		.offered()
		.map(|entry| (entry.key.clone(), entry.id))
		.collect();
	library.rescan().unwrap();
	drop(library);

	std::fs::rename(&drive, directory.path().join("gone")).unwrap();
	let restarted = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(restarted.ready(WAIT).await);
	let again: Vec<(String, i32)> = restarted
		.catalog()
		.offered()
		.map(|entry| (entry.key.clone(), entry.id))
		.collect();
	assert_eq!(again, ids);
	assert!(matches!(
		restarted.catalog().find_key("steam:2").unwrap().state,
		EntryState::Stale { .. }
	));

	// A changed scanner configuration does not reuse retained results.
	let mut changed = steam_config(&root);
	changed.command.push("-silent".into());
	let reconfigured = start(
		directory.path(),
		vec![ApplicationScannerConfig::Steam(changed)],
		LibraryConfig::default(),
	);
	assert!(reconfigured.ready(WAIT).await);
	assert!(reconfigured.catalog().find_key("steam:2").is_none());
	assert_eq!(reconfigured.catalog().find_key("steam:1").unwrap().id, ids[1].1);
}

#[tokio::test]
async fn visibility_choices_apply_immediately_and_persist() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	steam_root(&root, &[&root]);
	install(&root, 620, "Portal 2");
	install(&root, 400, "Portal");
	let library = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	let id = id_of(&library, "Portal");

	library.set_hidden("steam:400", true).await.unwrap();
	assert_eq!(offered(&library), vec!["Portal 2", "Steam"]);
	assert_eq!(library.catalog().find_key("steam:400").unwrap().id, id);
	assert_eq!(
		library.set_hidden("manual:0", true).await,
		Err(LibraryError::Invalid(
			"Manual applications are shown as configured; remove them in Settings instead.".into()
		))
	);
	assert!(matches!(
		library.set_hidden("steam:999", true).await,
		Err(LibraryError::NotFound(_))
	));
	drop(library);

	let restarted = start(directory.path(), vec![steam(&root)], LibraryConfig::default());
	assert!(restarted.ready(WAIT).await);
	assert_eq!(offered(&restarted), vec!["Portal 2", "Steam"]);
	restarted.set_hidden("steam:400", false).await.unwrap();
	assert_eq!(id_of(&restarted, "Portal"), id);
}

/// Unrelated activity next to watched metadata does not scan; a burst of
/// relevant changes is scanned as one.
#[tokio::test]
async fn unrelated_activity_is_ignored_and_bursts_are_debounced() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	steam_root(&root, &[&root]);
	let timing = Timing {
		quiet: Duration::from_millis(300),
		max_delay: Duration::from_secs(5),
		min_interval: Duration::ZERO,
		retry: vec![],
	};
	let library = Library::start_with(
		manual(),
		vec![steam(&root)],
		LibraryConfig::default(),
		paths(directory.path()),
		timing,
	);
	assert!(library.ready(WAIT).await);
	let scans = library.view().sources[0].scans;

	for index in 0..200 {
		std::fs::write(root.join(format!("steamapps/download_{index}.tmp")), "x").unwrap();
	}
	std::fs::create_dir_all(root.join("steamapps/shadercache/1")).unwrap();
	tokio::time::sleep(Duration::from_secs(1)).await;
	assert_eq!(library.view().sources[0].scans, scans, "no scan for unrelated files");

	for app_id in 1..=30 {
		install(&root, app_id, &format!("Game {app_id}"));
	}
	until(&library, "the burst", |view| {
		view.catalog.offered().count() == 31 && !view.scanning
	})
	.await;
	let burst_scans = library.view().sources[0].scans - scans;
	assert!(burst_scans <= 2, "{burst_scans} scans for one burst");
}

#[tokio::test]
async fn without_auto_refresh_only_a_rescan_updates_the_catalog() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	steam_root(&root, &[&root]);
	let options = LibraryConfig {
		auto_refresh: false,
		..LibraryConfig::default()
	};
	let library = start(directory.path(), vec![steam(&root)], options);
	assert!(library.ready(WAIT).await);
	install(&root, 620, "Portal 2");
	tokio::time::sleep(Duration::from_millis(800)).await;
	assert_eq!(offered(&library), vec!["Steam"]);
	assert!(!library.view().sources[0].watched);
	library.rescan().unwrap();
	until_offered(&library, &["Steam", "Portal 2"]).await;
}

/// The periodic check finds changes file notifications cannot report (here:
/// a library created after the last scan under a path that did not exist).
#[tokio::test]
async fn the_periodic_check_finds_libraries_that_appear_later() {
	let directory = tempfile::tempdir().unwrap();
	let root = directory.path().join("Steam");
	let options = LibraryConfig {
		auto_refresh: true,
		poll_interval_secs: 1,
	};
	let library = start(directory.path(), vec![steam(&root)], options);
	assert!(library.ready(WAIT).await);
	assert_eq!(library.view().sources[0].health, SourceHealth::Failed);
	assert_eq!(offered(&library), vec!["Steam"]);

	steam_root(&root, &[&root]);
	install(&root, 620, "Portal 2");
	until_offered(&library, &["Steam", "Portal 2"]).await;
}

#[tokio::test]
async fn sources_without_scanners_publish_the_manual_catalog() {
	let directory = tempfile::tempdir().unwrap();
	let library = start(directory.path(), Vec::new(), LibraryConfig::default());
	assert!(library.ready(WAIT).await);
	assert_eq!(offered(&library), vec!["Steam"]);
	assert!(!library.view().scanning);
}

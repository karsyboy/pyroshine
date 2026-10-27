//! Run the application library against a configuration and report what it
//! finds and what it costs, without a server and without touching the live
//! library state (IDs, visibility choices, caches go to a temporary
//! directory unless `--state` is given).
//!
//! ```sh
//! cargo run --release -p moonshine-tools --bin moonshine-library -- ~/.config/moonshine/config.toml --follow 120
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use moonshine_core::app_scanner::service::LibraryPaths;
use moonshine_core::app_scanner::{Library, SourceHealth};
use moonshine_core::config::Config;

#[derive(Parser, Debug)]
#[command(
	name = "moonshine-library",
	about = "Scan application libraries and report catalog, timing and resource use"
)]
struct Args {
	/// Configuration file whose applications and scanners are used.
	config: PathBuf,
	/// Keep running for this many seconds and report every catalog change
	/// (install or remove a game meanwhile to see it followed).
	#[arg(long, default_value_t = 0)]
	follow: u64,
	/// Directory for the identity file and caches (default: a fresh
	/// temporary directory, so the live state is never modified).
	#[arg(long)]
	state: Option<PathBuf>,
	/// Print every offered application.
	#[arg(long)]
	list: bool,
}

/// Resident memory of this process, in KiB.
fn rss_kib() -> u64 {
	std::fs::read_to_string("/proc/self/status")
		.ok()
		.and_then(|status| {
			status
				.lines()
				.find_map(|line| line.strip_prefix("VmRSS:"))
				.and_then(|value| value.split_whitespace().next()?.parse().ok())
		})
		.unwrap_or(0)
}

/// CPU time of the library thread, in milliseconds (from schedstat, in ns).
fn library_thread_cpu_ms() -> Option<u64> {
	for task in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
		let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
		if comm.trim() != "library" {
			continue;
		}
		let schedstat = std::fs::read_to_string(task.path().join("schedstat")).ok()?;
		let nanoseconds: u64 = schedstat.split_whitespace().next()?.parse().ok()?;
		return Some(nanoseconds / 1_000_000);
	}
	None
}

fn inotify_watches() -> usize {
	let Ok(fds) = std::fs::read_dir("/proc/self/fdinfo") else {
		return 0;
	};
	fds.flatten()
		.filter_map(|fd| std::fs::read_to_string(fd.path()).ok())
		.map(|info| info.lines().filter(|line| line.starts_with("inotify wd:")).count())
		.sum()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args = Args::parse();
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| "warn".into()))
		.with_writer(std::io::stderr)
		.init();
	let config = Config::read_from_file(&args.config).map_err(|()| "cannot read the configuration")?;
	let temporary = args.state.is_none();
	let state = args
		.state
		.clone()
		.unwrap_or_else(|| std::env::temp_dir().join(format!("moonshine-library-{}", std::process::id())));
	let paths = LibraryPaths {
		identity: Some(state.join("library.toml")),
		retained: Some(state.join("library-cache.json")),
		artwork: Some(state.join("boxart")),
	};

	let rss_before = rss_kib();
	let started = Instant::now();
	let library = Library::start(
		config.applications.clone(),
		config.application_scanners.clone(),
		config.library.clone(),
		paths,
	);
	if !library.ready(Duration::from_secs(120)).await {
		return Err("the initial scan did not finish within 120 s".into());
	}
	let elapsed = started.elapsed();
	let view = library.view();

	println!("Initial scan: {} ms wall", elapsed.as_millis());
	if let Some(cpu) = library_thread_cpu_ms() {
		println!("Library thread CPU: {cpu} ms");
	}
	println!(
		"Resident memory: {} KiB before, {} KiB after (+{} KiB)",
		rss_before,
		rss_kib(),
		rss_kib().saturating_sub(rss_before)
	);
	println!("inotify watches: {}", inotify_watches());
	for source in &view.sources {
		println!(
			"  [{}] {:<16} {:>5} entries ({} stale) {:>5} ms  {:?}{}  {}",
			source.scanner,
			source.kind.label(),
			source.entries,
			source.stale_entries,
			source.duration_ms.unwrap_or(0),
			source.health,
			if source.watched { ", watched" } else { ", periodic" },
			source.root
		);
		if source.health != SourceHealth::Ok
			&& let Some(message) = &source.message
		{
			println!("      {message}");
			if let Some(hint) = &source.hint {
				println!("      hint: {hint}");
			}
		}
	}
	let catalog = library.catalog();
	println!(
		"Catalog generation {}: {} offered, {} entries in total",
		catalog.generation,
		catalog.offered().count(),
		catalog.entries().len()
	);
	let with_art = catalog.offered().filter(|entry| !entry.artwork.is_empty()).count();
	println!(
		"Box art: {with_art} of {} offered applications",
		catalog.offered().count()
	);
	if args.list {
		for entry in catalog.entries() {
			println!(
				"  {:>10}  {:<40} {:<24} {:?} {}",
				entry.id,
				entry.application.title,
				entry.key,
				entry.state,
				entry
					.artwork
					.first()
					.map_or_else(|| "-".to_string(), |path| path.display().to_string())
			);
		}
	}

	if args.follow > 0 {
		println!("Following changes for {} s...", args.follow);
		let mut views = library.subscribe();
		let deadline = tokio::time::Instant::now() + Duration::from_secs(args.follow);
		let mut generation = catalog.generation;
		let mut known: std::collections::BTreeMap<i32, String> = catalog
			.offered()
			.map(|entry| (entry.id, entry.application.title.clone()))
			.collect();
		while tokio::time::timeout_at(deadline, views.changed())
			.await
			.is_ok_and(|changed| changed.is_ok())
		{
			let view = views.borrow_and_update().clone();
			if view.catalog.generation == generation {
				continue;
			}
			generation = view.catalog.generation;
			let now: std::collections::BTreeMap<i32, String> = view
				.catalog
				.offered()
				.map(|entry| (entry.id, entry.application.title.clone()))
				.collect();
			for (id, title) in now.iter().filter(|(id, _)| !known.contains_key(id)) {
				println!("  + {title} (ID {id})");
			}
			for (id, title) in known.iter().filter(|(id, _)| !now.contains_key(id)) {
				println!("  - {title} (ID {id})");
			}
			for (id, title) in now
				.iter()
				.filter(|(id, title)| known.get(id).is_some_and(|old| old != *title))
			{
				println!("  ~ {} -> {title} (ID {id})", known[id]);
			}
			known = now;
		}
		if let Some(cpu) = library_thread_cpu_ms() {
			println!("Library thread CPU after following: {cpu} ms");
		}
	}

	drop(library);
	if temporary {
		let _ = std::fs::remove_dir_all(&state);
	}
	Ok(())
}

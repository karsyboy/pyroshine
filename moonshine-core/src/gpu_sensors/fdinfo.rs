//! Per-process GPU time from DRM fdinfo (`/proc/<pid>/fdinfo/<fd>`).
//!
//! Every open DRM file reports its client ID, PCI device and cumulative
//! engine time (`drm-engine-<engine>: <ns> ns`, or `drm-cycles-<engine>`
//! against `drm-total-cycles-<engine>` on xe) together with resident memory;
//! see the kernel's `Documentation/gpu/drm-usage-stats.rst`. Duplicated or
//! inherited descriptors share a client ID and are counted once.

use std::collections::HashMap;
use std::time::Duration;

use moonshine_management::dto::GpuProcessStats;

use super::{ProcessGroup, process_stats};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Counter {
	Nanos(u64),
	Cycles { cycles: u64, total: u64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Client {
	counters: HashMap<String, Counter>,
	capacity: HashMap<String, u64>,
	vram_bytes: Option<u64>,
}

/// One parsed fdinfo file of a DRM client.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
	client_id: u64,
	pdev: Option<String>,
	client: Client,
}

fn parse(text: &str) -> Option<Entry> {
	let mut client_id = None;
	let mut pdev = None;
	let mut client = Client::default();
	let mut cycles = HashMap::new();
	let mut totals = HashMap::new();
	for line in text.lines() {
		let Some((key, value)) = line.split_once(':') else {
			continue;
		};
		let value = value.trim();
		let number = || value.split_whitespace().next()?.parse::<u64>().ok();
		if key == "drm-client-id" {
			client_id = number();
		} else if key == "drm-pdev" {
			pdev = Some(value.to_string());
		} else if let Some(engine) = key.strip_prefix("drm-engine-capacity-") {
			if let Some(capacity) = number() {
				client.capacity.insert(engine.to_string(), capacity.max(1));
			}
		} else if let Some(engine) = key.strip_prefix("drm-engine-") {
			if let Some(nanos) = number().filter(|_| value.ends_with("ns")) {
				client.counters.insert(engine.to_string(), Counter::Nanos(nanos));
			}
		} else if let Some(engine) = key.strip_prefix("drm-total-cycles-") {
			if let Some(total) = number() {
				totals.insert(engine.to_string(), total);
			}
		} else if let Some(engine) = key.strip_prefix("drm-cycles-") {
			if let Some(count) = number() {
				cycles.insert(engine.to_string(), count);
			}
		} else if key.starts_with("drm-resident-vram")
			&& let Some(bytes) = memory(value)
		{
			client.vram_bytes = Some(client.vram_bytes.unwrap_or(0) + bytes);
		}
	}
	for (engine, count) in cycles {
		if let Some(total) = totals.get(&engine) {
			client.counters.insert(
				engine,
				Counter::Cycles {
					cycles: count,
					total: *total,
				},
			);
		}
	}
	Some(Entry {
		client_id: client_id?,
		pdev,
		client,
	})
}

/// `<n>`, `<n> KiB`, `<n> MiB` or `<n> GiB`.
fn memory(value: &str) -> Option<u64> {
	let mut parts = value.split_whitespace();
	let amount: u64 = parts.next()?.parse().ok()?;
	let unit = match parts.next() {
		None => 1,
		Some("KiB") => 1 << 10,
		Some("MiB") => 1 << 20,
		Some("GiB") => 1 << 30,
		Some(_) => return None,
	};
	Some(amount.saturating_mul(unit))
}

/// Remembers each client's counters between samples.
pub(super) struct Tracker {
	pci_address: Option<String>,
	previous: HashMap<(&'static str, u64), Client>,
}

impl Tracker {
	pub(super) fn new(pci_address: Option<String>) -> Self {
		Self {
			pci_address,
			previous: HashMap::new(),
		}
	}

	/// GPU use of each group with a DRM client on this device. Engine shares
	/// need a previous sample `elapsed` ago and are empty on the first.
	pub(super) fn sample(&mut self, groups: &[ProcessGroup], elapsed: Option<Duration>) -> Vec<GpuProcessStats> {
		let mut seen: HashMap<u64, &'static str> = HashMap::new();
		let mut current: HashMap<(&'static str, u64), Client> = HashMap::new();
		for group in groups {
			for pid in &group.pids {
				for entry in drm_entries(*pid) {
					if self.pci_address.is_some() && entry.pdev.is_some() && entry.pdev != self.pci_address {
						continue;
					}
					// A client shared between groups is attributed to the first.
					if *seen.entry(entry.client_id).or_insert(group.role) == group.role {
						current.insert((group.role, entry.client_id), entry.client);
					}
				}
			}
		}
		let stats = shares(groups, &self.previous, &current, elapsed);
		self.previous = current;
		stats
	}
}

fn shares(
	groups: &[ProcessGroup],
	previous: &HashMap<(&'static str, u64), Client>,
	current: &HashMap<(&'static str, u64), Client>,
	elapsed: Option<Duration>,
) -> Vec<GpuProcessStats> {
	let elapsed_ns = elapsed.map_or(0, |elapsed| elapsed.as_nanos() as u64);
	groups
		.iter()
		.filter_map(|group| {
			let clients: Vec<_> = current.iter().filter(|((role, _), _)| *role == group.role).collect();
			if clients.is_empty() {
				return None;
			}
			let mut busy: HashMap<String, f64> = HashMap::new();
			let mut vram = None;
			for (key, client) in clients {
				if let Some(bytes) = client.vram_bytes {
					vram = Some(vram.unwrap_or(0) + bytes);
				}
				let Some(before) = previous.get(key) else {
					continue;
				};
				for (engine, counter) in &client.counters {
					let capacity = client.capacity.get(engine).copied().unwrap_or(1) as f64;
					let share = match (before.counters.get(engine), counter) {
						(Some(Counter::Nanos(then)), Counter::Nanos(now)) if elapsed_ns > 0 && now >= then => {
							(now - then) as f64 * 100.0 / elapsed_ns as f64
						},
						(
							Some(Counter::Cycles {
								cycles: then,
								total: then_total,
							}),
							Counter::Cycles { cycles: now, total },
						) if total > then_total && now >= then => (now - then) as f64 * 100.0 / (total - then_total) as f64,
						_ => continue,
					};
					*busy.entry(engine.clone()).or_default() += share / capacity;
				}
			}
			Some(process_stats(group, busy.into_iter().collect(), vram))
		})
		.collect()
}

/// The fdinfo of every DRM file `pid` has open. Unreadable processes (exited,
/// or another user's) have none.
fn drm_entries(pid: u32) -> Vec<Entry> {
	let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
		return Vec::new();
	};
	fds.filter_map(Result::ok)
		.filter(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target.starts_with("/dev/dri/")))
		.filter_map(|fd| std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fd.file_name().to_str()?)).ok())
		.filter_map(|text| parse(&text))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	const AMDGPU: &str = "pos:\t0\nflags:\t0100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t64\n\
		drm-pdev:\t0000:03:00.0\npasid:\t63\ndrm-total-vram:\t512 MiB\ndrm-resident-vram:\t256 MiB\n\
		drm-memory-vram:\t262144 KiB\ndrm-engine-gfx:\t1000000 ns\ndrm-engine-enc:\t250000 ns\n";

	const XE: &str = "drm-driver:\txe\ndrm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\n\
		drm-resident-vram0:\t4 MiB\ndrm-cycles-rcs:\t1000\ndrm-total-cycles-rcs:\t10000\n\
		drm-cycles-vcs:\t0\ndrm-total-cycles-vcs:\t10000\ndrm-engine-capacity-vcs:\t2\n";

	#[test]
	fn parses_engine_time_and_resident_vram() {
		let entry = parse(AMDGPU).unwrap();
		assert_eq!(entry.client_id, 64);
		assert_eq!(entry.pdev.as_deref(), Some("0000:03:00.0"));
		assert_eq!(
			entry.client.vram_bytes,
			Some(256 << 20),
			"drm-memory-vram is the legacy duplicate"
		);
		assert_eq!(entry.client.counters["gfx"], Counter::Nanos(1_000_000));
		assert_eq!(entry.client.counters["enc"], Counter::Nanos(250_000));

		let entry = parse(XE).unwrap();
		assert_eq!(
			entry.client.counters["rcs"],
			Counter::Cycles {
				cycles: 1000,
				total: 10000
			}
		);
		assert_eq!(entry.client.capacity["vcs"], 2);
		assert_eq!(entry.client.vram_bytes, Some(4 << 20));

		assert!(parse("pos:\t0\nflags:\t02\n").is_none(), "not a DRM file");
	}

	fn group(role: &'static str) -> ProcessGroup {
		ProcessGroup {
			role,
			label: role.into(),
			pids: Vec::new(),
		}
	}

	fn client(counters: &[(&str, Counter)], capacity: &[(&str, u64)]) -> Client {
		Client {
			counters: counters
				.iter()
				.map(|(name, counter)| (name.to_string(), *counter))
				.collect(),
			capacity: capacity
				.iter()
				.map(|(name, value)| (name.to_string(), *value))
				.collect(),
			vram_bytes: Some(1 << 20),
		}
	}

	#[test]
	fn engine_shares_are_counter_deltas_over_the_window() {
		let groups = [group("application"), group("pyroshine")];
		let mut previous = HashMap::new();
		let mut current = HashMap::new();
		// The application's two clients used 0.6 s and 0.2 s of graphics time in one second.
		previous.insert(
			("application", 1),
			client(&[("gfx", Counter::Nanos(1_000_000_000))], &[]),
		);
		current.insert(
			("application", 1),
			client(&[("gfx", Counter::Nanos(1_600_000_000))], &[]),
		);
		previous.insert(("application", 2), client(&[("gfx", Counter::Nanos(0))], &[]));
		current.insert(("application", 2), client(&[("gfx", Counter::Nanos(200_000_000))], &[]));
		// Pyroshine: half of two video engines (cycles, xe) and a client opened in the window.
		previous.insert(
			("pyroshine", 3),
			client(&[("vcs", Counter::Cycles { cycles: 0, total: 1000 })], &[("vcs", 2)]),
		);
		current.insert(
			("pyroshine", 3),
			client(
				&[(
					"vcs",
					Counter::Cycles {
						cycles: 1000,
						total: 2000,
					},
				)],
				&[("vcs", 2)],
			),
		);
		current.insert(("pyroshine", 4), client(&[("vcs", Counter::Nanos(5))], &[]));

		let stats = shares(&groups, &previous, &current, Some(Duration::from_secs(1)));
		assert_eq!(stats.len(), 2);
		assert_eq!(stats[0].role, "application");
		assert_eq!(stats[0].engines.len(), 1);
		assert_eq!(
			(stats[0].engines[0].engine.as_str(), stats[0].engines[0].busy_percent),
			("gfx", 80.0)
		);
		assert_eq!(stats[0].vram_bytes, Some(2 << 20));
		assert_eq!(stats[1].engines[0].busy_percent, 50.0);
		assert_eq!(stats[1].engines[0].class, "video");
		assert_eq!(stats[1].vram_bytes, Some(2 << 20), "a new client counts towards memory");

		let first = shares(&groups, &HashMap::new(), &current, None);
		assert!(
			first.iter().all(|group| group.engines.is_empty()),
			"the first sample has no deltas"
		);
	}

	#[test]
	fn reads_this_process_without_drm_files() {
		let groups = [ProcessGroup {
			role: "pyroshine",
			label: "Pyroshine".into(),
			pids: vec![std::process::id()],
		}];
		let mut tracker = Tracker::new(None);
		assert!(tracker.sample(&groups, None).is_empty());
	}
}

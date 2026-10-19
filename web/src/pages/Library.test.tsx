import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { LibraryApplication, LibrarySnapshot, LibrarySource } from "../api/types";
import { LibraryContent, LibraryPage, PAGE_SIZE, filterApplications } from "./Library";

const library = vi.hoisted(() => ({
  state: {
    snapshot: null as LibrarySnapshot | null,
    error: null as { kind: string; message: string } | null,
    reload: async () => {},
  },
}));
vi.mock("../state/daemon", () => ({ useDaemon: () => ({ connected: true, notify: () => {} }), useNow: () => 1_000_000 }));
vi.mock("../state/library", () => ({ useLibrary: () => library.state }));

const app = (overrides: Partial<LibraryApplication>): LibraryApplication => ({
  key: "steam:620",
  id: 620,
  title: "Portal 2",
  source: "steam",
  source_label: "Steam",
  manual: false,
  scanner: 0,
  native_id: "620",
  state: "available",
  offered: true,
  state_detail: null,
  can_hide: true,
  artwork: "steam",
  command: ["steam"],
  ...overrides,
});

const source = (overrides: Partial<LibrarySource> = {}): LibrarySource => ({
  scanner: 0,
  kind: "steam",
  label: "Steam",
  root: "$HOME/.local/share/Steam",
  status: "ok",
  message: null,
  hint: null,
  applications: 3,
  stale_applications: 0,
  last_scan_ms: 1_000_000 - 30_000,
  duration_ms: 12,
  watched: true,
  ...overrides,
});

const applications = [
  app({ key: "manual:0", id: 1, title: "Steam", source: "manual", source_label: "Manual", manual: true, scanner: null, native_id: null, can_hide: false }),
  app({}),
  app({ key: "steam:400", id: 400, title: "Portal", native_id: "400", state: "hidden", offered: false }),
  app({ key: "heroic:legendary/Min", id: 9, title: "Hades (Heroic)", source: "heroic", source_label: "Heroic", native_id: "legendary/Min" }),
  app({ key: "desktop:portal.desktop", id: 0, title: "Portal 2", source: "desktop", source_label: "Desktop entries", native_id: "portal.desktop", state: "duplicate", offered: false, can_hide: false, artwork: null }),
];

const snapshot = (overrides: Partial<LibrarySnapshot> = {}): LibrarySnapshot => ({
  generation: 3,
  scanning: false,
  auto_refresh: true,
  poll_interval_secs: 300,
  last_refresh_ms: 1_000_000 - 30_000,
  notice: null,
  sources: [source()],
  applications,
  ...overrides,
});

const render = (value: LibrarySnapshot) =>
  renderToStaticMarkup(<LibraryContent snapshot={value} navigate={() => {}} onRescan={() => {}} onToggle={() => {}} />);

describe("filtering", () => {
  const titles = (filter: Parameters<typeof filterApplications>[1]) => filterApplications(applications, filter).map((a) => a.key);

  it("searches titles, launchers, launcher IDs and Moonlight IDs", () => {
    expect(titles({ query: "portal", view: "all", source: "all" })).toEqual(["steam:620", "steam:400", "desktop:portal.desktop"]);
    expect(titles({ query: "HEROIC", view: "all", source: "all" })).toEqual(["heroic:legendary/Min"]);
    expect(titles({ query: "400", view: "all", source: "all" })).toEqual(["steam:400"]);
    expect(titles({ query: "legendary", view: "all", source: "all" })).toEqual(["heroic:legendary/Min"]);
  });

  it("filters by visibility and source", () => {
    expect(titles({ query: "", view: "offered", source: "all" })).toEqual(["manual:0", "steam:620", "heroic:legendary/Min"]);
    expect(titles({ query: "", view: "hidden", source: "all" })).toEqual(["steam:400"]);
    expect(titles({ query: "", view: "not_offered", source: "all" })).toEqual(["steam:400", "desktop:portal.desktop"]);
    expect(titles({ query: "", view: "all", source: "manual" })).toEqual(["manual:0"]);
  });
});

describe("library page", () => {
  it("tells manual and discovered applications apart and offers visibility controls for discovered ones", () => {
    const html = render(snapshot());
    expect(html).toContain("3 offered to clients · 1 hidden · updated 30 s ago");
    expect(html).toContain("Manual");
    // Manual entries are edited in Settings; discovered ones get a switch.
    expect(html).toContain("Edit");
    expect(html).toContain('aria-label="Offer Portal 2 to clients"');
    expect(html).not.toContain('aria-label="Offer Steam to clients"');
    expect(html).toContain("Duplicate");
    expect(html).toContain("Hidden");
    expect(html).toContain("Rescan");
  });

  it("shows scan progress and actionable source errors", () => {
    const html = render(
      snapshot({
        scanning: true,
        sources: [
          source({
            status: "partial",
            message: "Steam library /mnt/games is not accessible",
            hint: "Mount the drive or fix its permissions, then rescan.",
            stale_applications: 2,
            watched: false,
          }),
        ],
      }),
    );
    expect(html).toContain("Scanning…");
    expect(html).toContain("Partly unavailable");
    expect(html).toContain("Steam library /mnt/games is not accessible");
    expect(html).toContain("Mount the drive or fix its permissions, then rescan.");
    expect(html).toContain("(2 from the last successful scan)");
    expect(html).toContain("Periodic checks");
  });

  it("explains disabled automatic updates and missing scanners", () => {
    const html = render(snapshot({ auto_refresh: false, sources: [] }));
    expect(html).toContain("Automatic updates are off");
    expect(html).toContain("No launchers are scanned");
  });

  it("renders large libraries a page at a time", () => {
    const many = Array.from({ length: PAGE_SIZE * 3 }, (_, index) => app({ key: `steam:${index}`, id: index + 1, title: `Game ${index}` }));
    const html = render(snapshot({ applications: many }));
    expect(html.match(/role="listitem"/g)?.length).toBe(PAGE_SIZE);
    expect(html).toContain(`Showing ${PAGE_SIZE} of ${PAGE_SIZE * 3}`);
  });

  it("explains a daemon without a library", () => {
    library.state = { ...library.state, snapshot: null, error: { kind: "incompatible", message: "Unknown method" } };
    expect(renderToStaticMarkup(<LibraryPage navigate={() => {}} />)).toContain("does not report its game library");
    library.state = { ...library.state, snapshot: snapshot(), error: null };
    expect(renderToStaticMarkup(<LibraryPage navigate={() => {}} />)).toContain("offered to clients");
  });
});

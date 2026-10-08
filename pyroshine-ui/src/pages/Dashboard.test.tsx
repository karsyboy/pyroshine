import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import fixture from "../api/session.fixture.json";
import type { CaptureStats, SessionSnapshot, StreamStats } from "../api/types";
import { DashboardPage } from "./Dashboard";

const daemon = vi.hoisted(() => ({
  connected: true,
  loaded: true,
  server: null,
  session: null as SessionSnapshot | null,
  stats: null,
  history: [],
}));
vi.mock("../state/daemon", () => ({ useDaemon: () => daemon, useNow: () => 2000 }));

function render(snapshot: SessionSnapshot) {
  daemon.session = snapshot;
  return renderToStaticMarkup(<DashboardPage navigate={() => {}} />);
}

describe("foreground session display", () => {
  // This shared JSON also round-trips through the Rust DTO contract test.
  const snapshot: SessionSnapshot = { ...fixture, phase: "client_disconnected" };

  it("shows the foreground prominently in a retained session and preserves Moonlight identity", () => {
    const html = render(snapshot);
    expect(html).toMatch(/<h4[^>]*>Grim Dawn<\/h4>/);
    expect(html).toContain("Foreground application");
    expect(html).toContain("Moonlight application");
    expect(html).toContain("Steam");
    expect(html).toContain("Application ID");
    expect(html).toContain("42");
  });

  it("updates the displayed title within the same session", () => {
    const session = snapshot.session!;
    for (const title of ["Grim Dawn", "Grim Dawn - Running", "Steam"]) {
      const html = render({ ...snapshot, session: { ...session, foreground_application: { title } } });
      expect(html).toMatch(new RegExp(`<h4[^>]*>${title}<\\/h4>`));
    }
    expect(session.application).toEqual({ id: 42, title: "Steam" });
  });

  it.each([null, undefined])("falls back for unavailable metadata or an older daemon (%s)", (foreground) => {
    const html = render({ ...snapshot, session: { ...snapshot.session!, foreground_application: foreground } });
    expect(html).toMatch(/<h4[^>]*>Steam<\/h4>/);
  });
});

describe("frame pacing", () => {
  const snapshot: SessionSnapshot = { ...fixture, phase: "streaming" };
  const base: StreamStats = {
    epoch: snapshot.session!.epoch,
    at_ms: 0,
    window_ms: 1000,
    frames: 116,
    fps: 116,
    key_frames: 0,
    encoded_bitrate_bps: 100_000_000,
    wire_bitrate_bps: 120_000_000,
    transport_overhead_percent: 20,
    packets: 10_000,
    failed_packets: 0,
    discarded_packets: 0,
    stale_frames_dropped: 0,
    samples_dropped: 0,
    stages: [],
  };
  const capture: CaptureStats = {
    pacing: "vrr",
    source_fps: 94.5,
    interval_p50_us: 10_590,
    interval_p95_us: 10_680,
    interval_p99_us: 12_000,
    interval_max_us: 16_000,
    interval_stddev_us: 340,
    uneven_percent: 0.4,
    content_age_p50_us: 6,
    content_age_p95_us: 14,
  };

  function renderStats(stats: StreamStats) {
    daemon.stats = stats as never;
    try {
      return render(snapshot);
    } finally {
      daemon.stats = null;
    }
  }

  it("shows the game's frame times and VRR capture", () => {
    const html = renderStats({ ...base, capture });
    expect(html).toContain("Frame pacing");
    expect(html).toMatch(/>VRR capture</);
    expect(html).toContain("94.5 fps");
    expect(html).toContain("10.6 ms");
    expect(html).toContain("0.4 %");
  });

  it("labels fixed-refresh capture and static content", () => {
    const html = renderStats({ ...base, capture: { ...capture, pacing: "fixed", source_fps: 0 } });
    expect(html).toContain("Fixed refresh");
    expect(html).toContain("Static");
    expect(html).not.toMatch(/>VRR capture</);
  });

  it("omits the section for a daemon without capture statistics", () => {
    const html = renderStats(base);
    expect(html).toContain("Performance");
    expect(html).not.toContain("Frame pacing");
  });
});

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import fixture from "../api/session.fixture.json";
import type { AudioDetails, CaptureStats, GpuStats, SessionSnapshot, StreamStats } from "../api/types";
import { setPreference } from "../state/preferences";
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

describe("GPU", () => {
  const snapshot: SessionSnapshot = { ...fixture, phase: "streaming" };
  const base: StreamStats = {
    epoch: snapshot.session!.epoch,
    at_ms: 0,
    window_ms: 1000,
    frames: 60,
    fps: 60,
    key_frames: 0,
    encoded_bitrate_bps: 1,
    wire_bitrate_bps: 1,
    transport_overhead_percent: null,
    packets: 1,
    failed_packets: 0,
    discarded_packets: 0,
    stale_frames_dropped: 0,
    samples_dropped: 0,
    stages: [],
  };
  const gpu: GpuStats = {
    driver: "amdgpu",
    source: "sysfs",
    busy_percent: 97,
    encoder_busy_percent: 41,
    core_clock_mhz: 2950,
    memory_clock_mhz: 1258,
    temperature_c: 53,
    hotspot_c: 71,
    power_w: 245,
    power_limit_w: 340,
    vram_used_bytes: 3 * 2 ** 30,
    vram_total_bytes: 16 * 2 ** 30,
    processes: [
      { role: "application", label: "Grim Dawn", engines: [{ engine: "gfx", class: "graphics", busy_percent: 84.6 }], vram_bytes: 2 * 2 ** 30 },
      { role: "pyroshine", label: "Pyroshine", engines: [], vram_bytes: 300 * 2 ** 20 },
    ],
  };

  function renderGpu(value: GpuStats | undefined) {
    daemon.stats = { ...base, gpu: value } as never;
    try {
      return render(snapshot);
    } finally {
      daemon.stats = null;
    }
  }

  it("shows the sensors and GPU time per process", () => {
    const html = renderGpu(gpu);
    expect(html).toContain("GPU busy");
    expect(html).toContain("97 %");
    expect(html).toContain("core 2950 MHz");
    expect(html).toContain("hotspot 71 °C");
    expect(html).toContain("limit 340 W");
    expect(html).toContain("of 16.0 GiB");
    expect(html).toContain("Graphics (gfx)");
    expect(html).toContain("85 %");
    expect(html).toContain("Grim Dawn");
    expect(html).toContain("Idle");
  });

  it("shows only the sensors a driver reports, and nothing for an older daemon", () => {
    const sparse: GpuStats = {
      ...gpu,
      driver: "i915",
      busy_percent: null,
      encoder_busy_percent: null,
      temperature_c: null,
      hotspot_c: null,
      power_w: null,
      vram_used_bytes: null,
      core_clock_mhz: 1650,
      processes: [],
    };
    const html = renderGpu(sparse);
    expect(html).toContain("1650 MHz");
    expect(html).not.toContain("GPU busy");
    expect(html).not.toContain("Temperature");
    expect(renderGpu(undefined)).not.toContain("Video encoder");
  });
});

describe("audio stream", () => {
  const snapshot: SessionSnapshot = { ...fixture, phase: "streaming" };
  const stereo: AudioDetails = {
    channels: 2,
    channel_mask: 3,
    high_quality: false,
    opus_bitrate_bps: 256_000,
    packet_duration_ms: 5,
    encrypted: true,
    quality: "high",
    quality_requested: true,
    opus_streams: 1,
    opus_coupled_streams: 1,
    sample_rate_hz: 48_000,
  };
  const withAudio = (audio: AudioDetails | null) => render({ ...snapshot, session: { ...snapshot.session!, audio } });

  it("shows the quality the client requested and the encoded stream", () => {
    const html = withAudio(stereo);
    expect(html).toContain("High · requested by the client");
    expect(html).toContain("256 kb/s");
    expect(html).toContain("1 stream (1 stereo)");
    expect(html).toContain("48 kHz");
    expect(html).toContain("5 ms");
    expect(html).toContain("AES-128-CBC");
  });

  it("labels the host default and the high-quality surround layout", () => {
    const html = withAudio({
      ...stereo,
      channels: 8,
      channel_mask: 0x63f,
      high_quality: true,
      opus_bitrate_bps: 1_088_000,
      packet_duration_ms: 10,
      quality: "standard",
      quality_requested: false,
      opus_streams: 8,
      opus_coupled_streams: 0,
    });
    expect(html).toContain("Standard · host default");
    expect(html).toContain("7.1 surround · mask 0x63f");
    expect(html).toContain("High-quality surround · 8 mono streams");
    expect(html).toContain("1.1 Mb/s");
  });

  it("falls back for an older daemon and while negotiating", () => {
    const { quality: _q, quality_requested: _r, opus_streams: _s, opus_coupled_streams: _c, sample_rate_hz: _h, ...older } = stereo;
    const html = withAudio({ ...older, opus_bitrate_bps: 96_000 });
    expect(html).toContain("96 kb/s");
    expect(html).toMatch(/>Opus layout<\/dt><dd[^>]*>Standard</);
    expect(html).not.toContain("requested by the client");
    expect(withAudio(null)).toContain("Negotiating stereo…");
  });
});

describe("diagnosis and capture path", () => {
  const snapshot: SessionSnapshot = { ...fixture, phase: "streaming" };
  const capture: CaptureStats = {
    pacing: "fixed",
    source_fps: 60,
    interval_p50_us: 16_667,
    interval_p95_us: 17_000,
    interval_p99_us: 17_000,
    interval_max_us: 17_500,
    interval_stddev_us: 100,
    uneven_percent: 0,
    content_age_p50_us: 900,
    content_age_p95_us: 1_400,
  };
  const stats = (extra: Partial<StreamStats>): StreamStats => ({
    epoch: snapshot.session!.epoch,
    at_ms: 0,
    window_ms: 1000,
    frames: 60,
    fps: 60,
    key_frames: 0,
    encoded_bitrate_bps: 40_000_000,
    wire_bitrate_bps: 44_000_000,
    transport_overhead_percent: 10,
    packets: 4_000,
    failed_packets: 0,
    discarded_packets: 0,
    stale_frames_dropped: 0,
    samples_dropped: 0,
    stages: [],
    ...extra,
  });

  function renderStats(value: StreamStats) {
    daemon.stats = value as never;
    try {
      return render(snapshot);
    } finally {
      daemon.stats = null;
    }
  }

  it("lists the daemon's findings with their hints", () => {
    const html = renderStats(
      stats({
        findings: [
          {
            id: "encoder_bound",
            severity: "warning",
            title: "The encoder cannot keep up with 60 fps",
            detail: "Encoding takes 22.0 ms per frame.",
            hint: "Lower the resolution in the client.",
          },
        ],
      }),
    );
    expect(html).toContain("Diagnosis");
    expect(html).toContain("The encoder cannot keep up with 60 fps");
    expect(html).toContain("Lower the resolution in the client.");
  });

  it("hides informational findings when the preference is off, but keeps warnings", () => {
    const findings = stats({
      findings: [
        { id: "encoder_bound", severity: "warning", title: "The encoder cannot keep up", detail: "Slow.", hint: null },
        { id: "game_bound", severity: "info", title: "The game renders 45 fps", detail: "Limited.", hint: null },
      ],
    });
    const onlyInfo = stats({
      findings: [{ id: "composited", severity: "info", title: "Frames are composited", detail: "90 %.", hint: null }],
    });
    expect(renderStats(findings)).toContain("The game renders 45 fps");
    setPreference("infoFindings", false);
    try {
      const html = renderStats(findings);
      expect(html).toContain("The encoder cannot keep up");
      expect(html).not.toContain("The game renders 45 fps");
      expect(html).toContain("1 informational finding is hidden");
      const quiet = renderStats(onlyInfo);
      expect(quiet).not.toContain("Frames are composited");
      expect(quiet).toContain("No warnings.");
      expect(quiet).not.toContain("Nothing on the host limits the stream");
    } finally {
      setPreference("infoFindings", true);
    }
  });

  it("says when nothing on the host limits the stream, and stays silent for older daemons", () => {
    expect(renderStats(stats({ findings: [] }))).toContain("Nothing on the host limits the stream");
    expect(renderStats(stats({}))).not.toContain("Diagnosis");
  });

  it("names the capture path and the main reason frames are composited", () => {
    const html = renderStats(
      stats({
        capture: {
          ...capture,
          path: {
            current: "mixed",
            direct_frames: 10,
            direct_late_frames: 0,
            composited_frames: 50,
            composition_reasons: [{ id: "steam_overlay", label: "The Steam overlay is open", hint: null, frames: 50 }],
          },
        },
      }),
    );
    expect(html).toContain("Capture path");
    expect(html).toMatch(/>Mixed</);
    expect(html).toContain("83 % composited: the Steam overlay is open.");
    const direct = renderStats(
      stats({
        capture: {
          ...capture,
          path: { current: "direct", direct_frames: 60, direct_late_frames: 0, composited_frames: 0, composition_reasons: [] },
        },
      }),
    );
    expect(direct).toContain("Direct export");
    expect(direct).toContain("The game&#x27;s own buffer goes straight to the encoder.");
  });

  it("shows the encoder backend and GPU", () => {
    const session = snapshot.session!;
    const html = render({
      ...snapshot,
      session: {
        ...session,
        video: {
          codec: "hevc",
          codec_label: "HEVC",
          chroma: "4:2:0",
          bit_depth: 8,
          dynamic_range: "SDR",
          transfer: "BT.709",
          primaries: "BT.709",
          matrix: "BT.709",
          range: "limited",
          width: 2880,
          height: 1920,
          fps: 120,
          bitrate_bps: 80_000_000,
          packet_size: 1392,
          encrypted: true,
          minimum_fec_packets: 2,
          max_reference_frames: 1,
          pyrowave_dialect: null,
          encoder: { backend: "vulkan_video", backend_label: "Vulkan Video", gpu: "Intel Arc B390" },
        },
      },
    });
    expect(html).toContain("Vulkan Video on Intel Arc B390");
  });
});

describe("last stop", () => {
  it("explains an unexpected stop with its cause and what to do", () => {
    const html = render({
      phase: "idle",
      session: null,
      last_stop: {
        epoch: 3,
        reason: "application_stopped",
        message: "The application exited.",
        unexpected: true,
        completed: true,
        at_ms: 0,
        detail: "Steam crashed (SIGSEGV, core dumped), 4 s after launch.",
        hint: "Read its output with: `journalctl --user -u moonshine-session.service -e`",
      },
    });
    expect(html).toContain("Steam crashed (SIGSEGV, core dumped), 4 s after launch.");
    expect(html).not.toContain("The application exited.");
    expect(html).toMatch(/<code[^>]*>journalctl --user -u moonshine-session.service -e<\/code>/);
  });
});

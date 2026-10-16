// Development-only stand-in for the Pyroshine server, selected with
// `?mock=<scenario>` under `npm run dev`. It simulates a server with a
// stream, a pending pairing request and paired clients so every page can be
// exercised. Production builds never import this module (see client.ts).
//
// Scenarios: streaming (default), idle, retained, pairing, offline,
// composited, crashed, old-daemon, signin (signed out), setup (first run).

import fixture from "./schema.fixture.json";
import type { Backend, StreamEventName, StreamHandlers } from "./backend";
import type {
  AuthState,
  HistoryEntry,
  UpdateStatus,
  WebSession,
  ClientsSnapshot,
  ConfigDocument,
  ConfigValues,
  HealthReport,
  LibraryApplication,
  LibrarySnapshot,
  Overview,
  PairingSnapshot,
  ServerEvent,
  SessionSnapshot,
  StreamStats,
  UiError,
} from "./types";

const streams = new Set<StreamHandlers>();

function emit(event: StreamEventName, payload: unknown) {
  streams.forEach((handlers) => handlers.event(event, payload as never));
}

const scenario = new URLSearchParams(window.location.search).get("mock") || "streaming";
const now = Date.now();

const session: SessionSnapshot =
  scenario === "idle" || scenario === "offline" || scenario === "crashed"
    ? {
        phase: "idle",
        session: null,
        last_stop:
          scenario === "crashed"
            ? {
                epoch: 3,
                reason: "application_stopped",
                message: "The application exited.",
                unexpected: true,
                completed: true,
                at_ms: now - 90_000,
                detail: "Cyberpunk 2077 crashed (SIGSEGV, core dumped), 12 s after launch.",
                hint: "Its output is discarded: set stdout = \"journal\" and stderr = \"journal\" for this application, launch it again, then read it with: journalctl --user -u moonshine-session.service -e",
              }
            : null,
      }
    : {
        phase: scenario === "retained" ? "client_disconnected" : "streaming",
        session: {
          epoch: 4,
          application: { id: 81243, title: "Cyberpunk 2077" },
          client_address: "10.0.0.42",
          started_at_ms: now - 47 * 60_000,
          requested: { width: 3840, height: 2160, refresh_rate: 120, hdr: true, audio_channels: 2, audio_channel_mask: 3 },
          video: {
            codec: "pyrowave",
            codec_label: "PyroWave",
            chroma: "4:4:4",
            bit_depth: 10,
            dynamic_range: "HDR10",
            transfer: "PQ",
            primaries: "BT.2020",
            matrix: "BT.2020 NCL",
            range: "full",
            width: 3840,
            height: 2160,
            fps: 120,
            bitrate_bps: 1_200_000_000,
            packet_size: 1392,
            encrypted: false,
            minimum_fec_packets: 2,
            max_reference_frames: 1,
            pyrowave_dialect: "native_wire_v1",
            encoder: { backend: "pyrowave", backend_label: "PyroWave (Vulkan compute)", gpu: "AMD Radeon RX 7900 XTX (RADV NAVI31)" },
          },
          audio: {
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
          },
        },
        last_stop: null,
      };

let pairing: PairingSnapshot = {
  enabled: true,
  requests:
    scenario === "streaming" || scenario === "pairing"
      ? [
          {
            client_id: "0123456789ABCDEF",
            request: "9f2c4e1ab7d34c0e8a61f0c2b5d9e7a4",
            requester: "10.0.0.57",
            fingerprint: "4c1f9a0e7b2d3c5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6",
            received_at_ms: now - 20_000,
            approval_expires_in_ms: 280_000,
            approved: false,
          },
        ]
      : [],
};

let clients: ClientsSnapshot = {
  clients: [
    {
      fingerprint: "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
      client_ids: ["0123456789ABCDEF"],
      label: "Living room TV",
      paired_at_ms: now - 21 * 86_400_000,
      last_seen_ms: now - 3_000,
      last_address: "10.0.0.42",
    },
    {
      fingerprint: "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0",
      client_ids: ["0123456789ABCDEF"],
      label: null,
      paired_at_ms: null,
      last_seen_ms: null,
      last_address: null,
    },
  ],
  legacy_client_ids: [],
};

const values = structuredClone(fixture.defaults) as ConfigValues;
(values as Record<string, unknown>).name = "Living-room PC";
let config: ConfigDocument = {
  path: "/home/player/.config/moonshine/config.toml",
  revision: "r1",
  writable: true,
  read_only_reason: null,
  restart_required: false,
  values,
  defaults: fixture.defaults as ConfigValues,
};

function stats(): StreamStats {
  const jitter = () => 0.85 + Math.random() * 0.3;
  return {
    epoch: 4,
    at_ms: Date.now(),
    window_ms: 1000,
    frames: 120,
    fps: 119.6 + Math.random() * 0.8,
    key_frames: 120,
    encoded_bitrate_bps: 905_000_000 * jitter(),
    wire_bitrate_bps: 1_060_000_000 * jitter(),
    transport_overhead_percent: 17.2,
    packets: 95_000,
    failed_packets: 0,
    discarded_packets: 0,
    stale_frames_dropped: 0,
    samples_dropped: 0,
    stages: [
      { id: "capture_queue", label: "Capture queue", avg_us: 92, p50_us: 80, p95_us: 180, max_us: 420 },
      { id: "import", label: "DMA-BUF import", avg_us: 48, p50_us: 45, p95_us: 70, max_us: 130 },
      { id: "submit", label: "Encoder submit", avg_us: 61, p50_us: 58, p95_us: 90, max_us: 160 },
      { id: "encode", label: "Encode", avg_us: 1480 * jitter(), p50_us: 1450, p95_us: 1810, max_us: 2400 },
      { id: "packetize", label: "Packetization", avg_us: 310, p50_us: 300, p95_us: 380, max_us: 520 },
      { id: "send_queue", label: "Send queue", avg_us: 40, p50_us: 35, p95_us: 80, max_us: 210 },
      { id: "send", label: "Socket send", avg_us: 3900 * jitter(), p50_us: 3850, p95_us: 4600, max_us: 6100 },
      { id: "total", label: "Total pipeline", avg_us: 6100 * jitter(), p50_us: 6000, p95_us: 7300, max_us: 9100 },
    ],
    capture: {
      pacing: "vrr",
      source_fps: 94 + Math.random() * 4,
      interval_p50_us: 10_600,
      interval_p95_us: 10_700 + Math.random() * 400,
      interval_p99_us: 12_400,
      interval_max_us: 16_800,
      interval_stddev_us: 340,
      uneven_percent: 0.4 * jitter(),
      content_age_p50_us: 6,
      content_age_p95_us: 15,
      path:
        scenario === "composited"
          ? {
              current: "mixed",
              direct_frames: 21,
              direct_late_frames: 0,
              composited_frames: 99,
              composition_reasons: [
                {
                  id: "scaling",
                  label: "The game renders at a different resolution than the stream",
                  hint: "Run the game fullscreen at the stream's resolution so its frames can be exported without scaling.",
                  frames: 99,
                },
              ],
            }
          : { current: "direct", direct_frames: 120, direct_late_frames: 0, composited_frames: 0, composition_reasons: [] },
    },
    findings:
      scenario === "composited"
        ? [
            {
              id: "send_backlog",
              severity: "warning",
              title: "The network could not keep up with the bitrate",
              detail: "41 packets were discarded before sending at 1060 Mbit/s (requested 1200 Mbit/s).",
              hint: "Lower the bitrate in the client, or use a wired connection.",
            },
            {
              id: "composited",
              severity: "info",
              title: "Frames are composited: the game renders at a different resolution than the stream",
              detail: "83 % of frames were composited rather than exported directly, which costs GPU time and latency.",
              hint: "Run the game fullscreen at the stream's resolution so its frames can be exported without scaling.",
            },
          ]
        : [],
    gpu: {
      driver: "amdgpu",
      source: "sysfs",
      busy_percent: 88 + Math.random() * 8,
      encoder_busy_percent: 38 + Math.random() * 6,
      core_clock_mhz: 2480 + Math.random() * 60,
      memory_clock_mhz: 1250,
      temperature_c: 66,
      hotspot_c: 81,
      power_w: 286 + Math.random() * 20,
      power_limit_w: 339,
      vram_used_bytes: 14.2 * 2 ** 30,
      vram_total_bytes: 24 * 2 ** 30,
      processes: [
        {
          role: "application",
          label: "Cyberpunk 2077",
          engines: [{ engine: "gfx", class: "graphics", busy_percent: 78 + Math.random() * 6 }],
          vram_bytes: 11.8 * 2 ** 30,
        },
        {
          role: "pyroshine",
          label: "Pyroshine",
          engines: [
            { engine: "compute", class: "compute", busy_percent: 9.4 },
            { engine: "gfx", class: "graphics", busy_percent: 1.2 },
          ],
          vram_bytes: 412 * 2 ** 20,
        },
      ],
    },
  };
}

let health: HealthReport = {
  all_fatal_passed: true,
  checked_at_ms: now - 5 * 3_600_000,
  rechecked: false,
  checks: [
    { name: "Vulkan", outcome: "passed", message: "AMD Radeon RX 7900 XTX (RADV NAVI31), driver 25.2.0, Vulkan 1.4.318", duration_ms: 41, id: "vulkan", hint: null, fatal: false },
    { name: "DMA-BUF", outcome: "passed", message: "VK_KHR_external_memory_fd, VK_EXT_image_drm_format_modifier", duration_ms: 12, id: "dma_buf", hint: null, fatal: false },
    { name: "uinput", outcome: "passed", message: "Writable", duration_ms: 1, id: "uinput", hint: null, fatal: false },
    {
      name: "Sleep inhibit",
      outcome: "warning",
      message: "Cannot acquire sleep inhibit: org.freedesktop.DBus.Error.AccessDenied",
      duration_ms: 3,
      id: "sleep_inhibit",
      hint: "Add your user to the 'pyroshine' group: `sudo usermod -aG pyroshine $USER`\nThen log out and back in.",
      fatal: false,
    },
  ],
};

const events: ServerEvent[] =
  scenario === "offline"
    ? []
    : [
        {
          level: "error",
          code: "codec_unsupported",
          message: "The client requested AV1 4:4:4 8-bit, which this GPU did not pass at startup.",
          hint: "Choose a verified profile in the client: H.264, HEVC, HEVC Main10, AV1, PyroWave, PyroWave 4:4:4, PyroWave HDR10. Pyroshine never falls back silently.",
          at_ms: now - 26 * 60_000,
        },
        {
          level: "error",
          code: "application_not_found",
          message: "Launching Heroic failed: The application command 'heroic' was not found.",
          hint: "Install it, or use an absolute path in the application's command (commands are looked up in the PATH of the Pyroshine service).",
          at_ms: now - 3 * 60_000,
        },
      ];

const overview = (): Overview => ({
  connected: scenario !== "offline",
  server:
    scenario === "offline"
      ? null
      : {
          api_version: 1,
          version: "test",
          name: "Living-room PC",
          pid: 4242,
          started_at_ms: now - 5 * 3_600_000,
          config_path: config.path,
          capabilities: {
            codecs: [
              { id: "h264", label: "H.264" },
              { id: "hevc", label: "HEVC" },
              { id: "hevc_main10", label: "HEVC Main10" },
              { id: "av1", label: "AV1" },
              { id: "pyrowave", label: "PyroWave" },
              { id: "pyrowave_444", label: "PyroWave 4:4:4" },
              { id: "pyrowave_hdr", label: "PyroWave HDR10" },
            ],
            hdr_advertised: true,
            dma_buf: true,
            gpu: "AMD Radeon RX 7900 XTX (RADV NAVI31)",
          },
          health,
          listeners: { address: "0.0.0.0", http_port: 47989, https_port: 47984, rtsp_port: 48010, video_port: 47998, audio_port: 48000, control_port: 47999 },
          pairing_enabled: true,
        },
  session: scenario === "offline" ? null : session,
  pairing: scenario === "offline" ? null : pairing,
  clients: scenario === "offline" ? null : clients,
  stats: session.phase === "streaming" ? stats() : null,
});

if (session.phase === "streaming") {
  setInterval(() => emit("stats", stats()), 1000);
}

function discovered(source: string, label: string, scanner: number, nativeId: string, title: string, id: number): LibraryApplication {
  return {
    key: `${source}:${nativeId}`,
    id,
    title,
    source,
    source_label: label,
    manual: false,
    scanner,
    native_id: nativeId,
    state: "available",
    offered: true,
    state_detail: null,
    can_hide: true,
    artwork: source === "steam" ? "steam" : "launcher",
    command: [source, nativeId],
  };
}

// A large library, so the page can be checked for responsiveness.
const steamGames: LibraryApplication[] = Array.from({ length: 1200 }, (_, index) =>
  discovered("steam", "Steam", 0, String(1000 + index * 10), `Steam Game ${String(index + 1).padStart(4, "0")}`, 500_000 + index),
);
let library: LibrarySnapshot = {
  generation: 7,
  scanning: false,
  auto_refresh: scenario !== "manual-refresh",
  poll_interval_secs: 300,
  last_refresh_ms: now - 42_000,
  notice: null,
  sources: [
    {
      scanner: 0,
      kind: "steam",
      label: "Steam",
      root: "$HOME/.local/share/Steam",
      status: "ok",
      message: null,
      hint: null,
      applications: steamGames.length + 3,
      stale_applications: 0,
      last_scan_ms: now - 42_000,
      duration_ms: 38,
      watched: true,
    },
    {
      scanner: 1,
      kind: "heroic",
      label: "Heroic",
      root: "~/.config/heroic",
      status: "partial",
      message: "Cannot parse /home/player/.config/heroic/gog_store/installed.json: EOF while parsing a list at line 1 column 412",
      hint: "The launcher may be updating its files; Pyroshine retries shortly and keeps offering the last known games.",
      applications: 2,
      stale_applications: 1,
      last_scan_ms: now - 5_000,
      duration_ms: 4,
      watched: true,
    },
    {
      scanner: 2,
      kind: "desktop",
      label: "Desktop entries",
      root: "~/.local/share/applications",
      status: "ok",
      message: null,
      hint: null,
      applications: 2,
      stale_applications: 0,
      last_scan_ms: now - 42_000,
      duration_ms: 12,
      watched: false,
    },
  ],
  applications: [
    {
      key: "manual:0",
      id: 1_234_567,
      title: "Steam",
      source: "manual",
      source_label: "Manual",
      manual: true,
      scanner: null,
      native_id: null,
      state: "available",
      offered: true,
      state_detail: null,
      can_hide: false,
      artwork: "icon_theme",
      command: ["/usr/bin/steam", "steam://open/bigpicture"],
    },
    {
      key: "manual:1",
      id: 7_654_321,
      title: "Cyberpunk 2077",
      source: "manual",
      source_label: "Manual",
      manual: true,
      scanner: null,
      native_id: null,
      state: "available",
      offered: true,
      state_detail: null,
      can_hide: false,
      artwork: "manual",
      command: ["/usr/bin/steam", "-bigpicture", "steam://rungameid/1091500"],
    },
    { ...discovered("steam", "Steam", 0, "1091500", "Cyberpunk 2077", 0), state: "shadowed", offered: false, can_hide: false, state_detail: "Replaced by the manual application 'Cyberpunk 2077'." },
    discovered("steam", "Steam", 0, "1145360", "Hades (Steam)", 81_243),
    { ...discovered("steam", "Steam", 0, "620", "Portal 2", 92_331), state: "hidden", offered: false, state_detail: "Hidden from clients." },
    ...steamGames,
    discovered("heroic", "Heroic", 1, "legendary/Min", "Hades (Heroic)", 45_120),
    { ...discovered("heroic", "Heroic", 1, "gog/1207658924", "Gwent", 66_001), state: "stale", state_detail: "Its library is not reachable; offered from the last successful scan." },
    { ...discovered("desktop", "Desktop entries", 2, "Portal 2.desktop", "Portal 2", 0), state: "duplicate", offered: false, can_hide: false, artwork: "desktop_icon", state_detail: "A shortcut to 'Portal 2', which is offered instead." },
    { ...discovered("desktop", "Desktop entries", 2, "org.prismlauncher.PrismLauncher.desktop", "Prism Launcher", 12_009), artwork: null },
  ],
};

function emitLibrary() {
  library = { ...library, generation: library.generation + 1 };
  emit("library", library);
}

function fail(kind: string, message: string): never {
  throw { kind, message } satisfies UiError;
}

const delay = (ms = 150) => new Promise((resolve) => setTimeout(resolve, ms));

let auth: AuthState = {
  configured: scenario !== "setup",
  setup_pending: false,
  locked: null,
  min_password_length: 8,
  authenticated: scenario !== "signin" && scenario !== "setup",
  csrf_token: "mock",
  session: { id: "0123456789abcdef", expires_at_ms: now + 7 * 86_400_000 },
  api_version: 1,
};

const webSessions: WebSession[] = [
  {
    id: "0123456789abcdef",
    created_at_ms: now - 3 * 86_400_000,
    last_used_at_ms: now,
    expires_at_ms: now + 7 * 86_400_000,
    address: "127.0.0.1",
    agent: "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0",
    current: true,
  },
  {
    id: "fedcba9876543210",
    created_at_ms: now - 9 * 86_400_000,
    last_used_at_ms: now - 2 * 86_400_000,
    expires_at_ms: now + 5 * 86_400_000,
    address: "10.0.0.20",
    agent: "Mozilla/5.0 (Linux; Android 15) AppleWebKit/537.36 Chrome/140.0 Mobile Safari/537.36",
    current: false,
  },
];

function historyEntry(id: number, hoursAgo: number, application: string, unexpected = false): HistoryEntry {
  const started = now - hoursAgo * 3_600_000;
  const ended = started + 47 * 60_000;
  return {
    id,
    epoch: id,
    application,
    foreground_application: application === "Steam" ? "Hades II" : null,
    client_address: "10.0.0.42",
    client_label: "Living room TV",
    started_at_ms: started,
    ended_at_ms: ended,
    requested: { width: 3840, height: 2160, refresh_rate: 120, hdr: true },
    video: {
      codec: "pyrowave",
      codec_label: "PyroWave",
      chroma: "4:4:4",
      bit_depth: 10,
      hdr: true,
      width: 3840,
      height: 2160,
      fps: 120,
      bitrate_bps: 1_200_000_000,
      encoder: "PyroWave (Vulkan compute)",
      gpu: "AMD Radeon RX 7900 XTX (RADV NAVI31)",
    },
    audio_channels: 2,
    disconnects: unexpected ? 0 : 1,
    reconnects: unexpected ? 0 : 1,
    events: unexpected
      ? [{ at_ms: ended, kind: "stopped", detail: "The application exited." }]
      : [
          { at_ms: started + 20 * 60_000, kind: "disconnected" },
          { at_ms: started + 21 * 60_000, kind: "reconnected" },
          { at_ms: ended, kind: "stopped", detail: "The client ended the session." },
        ],
    stop: unexpected
      ? {
          reason: "application_stopped",
          message: "The application exited.",
          detail: `${application} crashed (SIGSEGV, core dumped), 12 s after launch.`,
          hint: "Set stdout = \"journal\" and stderr = \"journal\" for this application to keep its output.",
          unexpected: true,
        }
      : { reason: "user_stopped", message: "The client ended the session.", unexpected: false },
    performance: unexpected
      ? null
      : {
          windows: 2820,
          streaming_ms: 2_820_000,
          frames: 338_400,
          fps_avg: 119.6,
          fps_min: 88.0,
          encoded_bitrate_avg_bps: 960_000_000,
          wire_bitrate_avg_bps: 1_105_000_000,
          stale_frames_dropped: 12,
          failed_packets: 0,
          discarded_packets: 3,
          stages: [
            { id: "convert", label: "Color conversion", avg_us: 410, p95_max_us: 980 },
            { id: "encode", label: "Encode", avg_us: 2_150, p95_max_us: 4_900 },
            { id: "send", label: "Packetize and send", avg_us: 620, p95_max_us: 1_700 },
          ],
          gpu_busy_avg_percent: 71,
          encoder_busy_avg_percent: null,
          gpu_temperature_max_c: 74,
          findings: [{ id: "game_below_target", severity: "info", title: "The game renders below the stream's frame rate", windows: 140 }],
        },
  };
}

let history: HistoryEntry[] =
  scenario === "offline" ? [] : [historyEntry(12, 3, "Cyberpunk 2077"), historyEntry(11, 27, "Steam"), historyEntry(10, 50, "Heroic", true)];

let updates: UpdateStatus = {
  current_version: "0.18.0",
  automatic: false,
  channel: "stable",
  checking: false,
  last_checked_ms: null,
  last_error: null,
  latest: null,
  update_available: false,
  dismissed: false,
  install: {
    kind: "arch",
    label: "Arch Linux / CachyOS package",
    steps: ["Update from the [pyrowave] repository: sudo pacman -Syu", 'Restart the service to run the new version: sudo systemctl restart "pyroshine@$USER"'],
  },
};

function requireAuth() {
  if (!auth.authenticated) fail("unauthenticated", "Sign in to manage Pyroshine.");
}

function offline() {
  if (scenario === "offline") fail("unavailable", "Pyroshine cannot be reached. Check that the service is running; this page reconnects on its own.");
}

export const mock: Backend = {
  auth: async () => {
    await delay();
    return auth;
  },
  setup: async (code, password) => {
    await delay(400);
    if (code.replace(/[-\s]/g, "").length !== 16) fail("invalid_credentials", "The password or setup code is not correct.");
    if (password.length < 8) fail("invalid", "Choose a password of at least 8 characters.");
    auth = { ...auth, configured: true, authenticated: true };
    return auth;
  },
  login: async (password) => {
    await delay(400);
    if (password !== "pyroshine") fail("invalid_credentials", "The password or setup code is not correct. (The mock password is “pyroshine”.)");
    auth = { ...auth, authenticated: true };
    return auth;
  },
  logout: async () => {
    await delay();
    auth = { ...auth, authenticated: false };
  },
  changePassword: async (current) => {
    await delay(400);
    if (current !== "pyroshine") fail("invalid_credentials", "The password or setup code is not correct.");
  },
  webSessions: async () => {
    await delay();
    return webSessions;
  },
  revokeWebSession: async (id) => {
    await delay();
    const index = webSessions.findIndex((session) => session.id === id);
    if (index < 0) fail("not_found", "This session already ended.");
    webSessions.splice(index, 1);
  },

  overview: async () => {
    await delay();
    requireAuth();
    offline();
    return overview();
  },
  endSession: async () => {
    await delay();
    session.phase = "stopping";
    emit("session", { ...session });
    setTimeout(() => {
      emit("session", { phase: "idle", session: null, last_stop: null });
    }, 1200);
  },
  approvePairing: async (clientId, request, pin, label) => {
    await delay();
    if (!/^\d{1,16}$/.test(pin)) fail("invalid", "Enter the PIN shown by Moonlight (digits only).");
    pairing = { ...pairing, requests: pairing.requests.map((r) => (r.request === request ? { ...r, approved: true } : r)) };
    emit("pairing", pairing);
    setTimeout(() => {
      pairing = { ...pairing, requests: pairing.requests.filter((r) => r.request !== request) };
      clients = {
        ...clients,
        clients: [
          ...clients.clients,
          {
            fingerprint: "4c1f9a0e7b2d3c5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6",
            client_ids: ["0123456789ABCDEF"],
            label: label || null,
            paired_at_ms: Date.now(),
            last_seen_ms: Date.now(),
            last_address: "10.0.0.57",
          },
        ],
      };
      emit("pairing", pairing);
      emit("pairing-resolved", { client_id: clientId, request, outcome: "paired" });
      emit("clients", clients);
    }, 1500);
  },
  rejectPairing: async (clientId, request) => {
    await delay();
    pairing = { ...pairing, requests: pairing.requests.filter((r) => r.request !== request) };
    emit("pairing", pairing);
    emit("pairing-resolved", { client_id: clientId, request, outcome: "rejected" });
  },
  revokeClient: async (fingerprint) => {
    await delay();
    clients = { ...clients, clients: clients.clients.filter((c) => c.fingerprint !== fingerprint) };
    emit("clients", clients);
  },
  renameClient: async (fingerprint, label) => {
    await delay();
    clients = {
      ...clients,
      clients: clients.clients.map((c) => (c.fingerprint === fingerprint ? { ...c, label: label || null } : c)),
    };
    emit("clients", clients);
  },

  loadConfig: async () => {
    await delay();
    return { document: config, schema: fixture.schema as never };
  },
  validateConfig: async (next) => {
    await delay();
    const issues = [] as { path: string | null; message: string }[];
    if (typeof next.address === "string" && !/^[0-9a-f:.]+$/i.test(next.address)) {
      issues.push({ path: null, message: `address '${next.address}' is not an IP address` });
    }
    return { valid: issues.length === 0, issues, changed_paths: [], restart_required: true };
  },
  saveConfig: async (next, revision) => {
    await delay();
    if (revision !== config.revision) fail("conflict", "The configuration file changed since it was loaded.");
    config = { ...config, values: next, revision: `r${Date.now()}`, restart_required: true };
    emit("config-saved", { revision: config.revision, restart_required: true });
    return { revision: config.revision, changed_paths: [], restart_required: true };
  },

  library: async () => {
    await delay();
    if (scenario === "old-daemon") fail("incompatible", "Unknown method GetLibrary");
    return library;
  },
  rescanLibrary: async () => {
    await delay();
    library = { ...library, scanning: true };
    emit("library", library);
    setTimeout(() => {
      library = { ...library, scanning: false, last_refresh_ms: Date.now(), sources: library.sources.map((s) => ({ ...s, last_scan_ms: Date.now() })) };
      emitLibrary();
    }, 1500);
  },
  setApplicationHidden: async (key, hidden) => {
    await delay();
    library = {
      ...library,
      applications: library.applications.map((a) =>
        a.key === key
          ? { ...a, state: hidden ? "hidden" : "available", offered: !hidden, state_detail: hidden ? "Hidden from clients." : null }
          : a,
      ),
    };
    emitLibrary();
  },

  recentEvents: async () => {
    await delay();
    return events;
  },
  runHealthCheck: async () => {
    await delay(1200);
    health = { ...health, rechecked: true, checked_at_ms: Date.now() };
    emit("health", health);
    return health;
  },
  diagnosticReport: async () => {
    await delay();
    return JSON.stringify({ generated_at_ms: Date.now(), ...overview(), events }, null, 2);
  },
  supportBundle: async () => {
    await delay(800);
    return { blob: new Blob(["mock bundle"], { type: "application/gzip" }), name: "pyroshine-diag-20261014-155902.tar.gz" };
  },

  history: async (limit, before) => {
    await delay();
    if (scenario === "old-daemon") fail("not_found", "Unknown management API route.");
    const entries = history.filter((entry) => before === undefined || entry.id < before).slice(0, limit);
    return { available: true, error: null, retention_days: 90, max_sessions: 1000, total: history.length, entries };
  },
  historyEntry: async (id) => {
    await delay();
    return history.find((entry) => entry.id === id) ?? fail("not_found", "This session is no longer in the history.");
  },
  clearHistory: async () => {
    await delay();
    history = [];
  },

  updates: async () => {
    await delay();
    return updates;
  },
  checkUpdates: async () => {
    await delay(900);
    updates = {
      ...updates,
      last_checked_ms: Date.now(),
      latest: { version: "0.18.1", prerelease: false, published_at: "2026-10-20T10:00:00Z", url: "https://github.com/karsyboy/pyroshine/releases/tag/v0.18.1" },
      update_available: true,
      dismissed: false,
    };
    return updates;
  },
  updateSettings: async (automatic, channel) => {
    await delay();
    updates = { ...updates, automatic, channel };
    return updates;
  },
  dismissUpdate: async () => {
    await delay();
    updates = { ...updates, dismissed: true };
    return updates;
  },

  stream: (handlers) => {
    if (!auth.authenticated) {
      setTimeout(() => handlers.signedOut(), 0);
      return () => {};
    }
    if (scenario === "offline") {
      setTimeout(() => handlers.disconnected(), 0);
      return () => {};
    }
    streams.add(handlers);
    setTimeout(() => handlers.event("overview", overview()), 50);
    return () => {
      streams.delete(handlers);
    };
  },
};

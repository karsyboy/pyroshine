// Development-only stand-in for the Rust side, used when the UI runs in a
// plain browser (`npm run dev`). It simulates a daemon with a stream, a
// pending pairing request and paired clients so every page can be exercised.
// Production builds never import this module (see bridge.ts).

import fixture from "./schema.fixture.json";
import type {
  ClientsSnapshot,
  ConfigDocument,
  ConfigValues,
  Overview,
  PairingSnapshot,
  SessionSnapshot,
  StreamStats,
  UiError,
} from "./types";

type Handler = (payload: unknown) => void;
const listeners = new Map<string, Set<Handler>>();

function emit(event: string, payload: unknown) {
  listeners.get(event)?.forEach((handler) => handler(payload));
}

const scenario = new URLSearchParams(window.location.search).get("mock") ?? "streaming";
const now = Date.now();

const session: SessionSnapshot =
  scenario === "idle" || scenario === "offline"
    ? { phase: "idle", session: null, last_stop: null }
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
          },
          audio: { channels: 2, channel_mask: 3, high_quality: true, opus_bitrate_bps: 512_000, packet_duration_ms: 5, encrypted: true },
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
  };
}

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
          health: {
            all_fatal_passed: true,
            checks: [
              { name: "Vulkan", outcome: "passed", message: "Vulkan 1.4 device available", duration_ms: 41 },
              { name: "DMA-BUF import", outcome: "passed", message: "Supported", duration_ms: 12 },
              { name: "uinput", outcome: "passed", message: "/dev/uinput is writable", duration_ms: 1 },
              { name: "Sleep inhibition", outcome: "warning", message: "polkit rule not installed", duration_ms: 3 },
            ],
          },
          listeners: { address: "0.0.0.0", http_port: 47989, https_port: 47984, rtsp_port: 48010, video_port: 47998, audio_port: 48000, control_port: 47999 },
          pairing_enabled: true,
        },
  session: scenario === "offline" ? null : session,
  pairing: scenario === "offline" ? null : pairing,
  clients: scenario === "offline" ? null : clients,
  stats: session.phase === "streaming" ? stats() : null,
});

if (session.phase === "streaming") {
  setInterval(() => emit("daemon://stats", stats()), 1000);
}

function fail(kind: string, message: string): never {
  throw { kind, message } satisfies UiError;
}

export async function mockInvoke<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  await new Promise((resolve) => setTimeout(resolve, 150));
  switch (command) {
    case "overview":
    case "refresh":
      return overview() as T;
    case "end_session":
      session.phase = "stopping";
      emit("daemon://session", { ...session });
      setTimeout(() => {
        emit("daemon://session", { phase: "idle", session: null, last_stop: null });
      }, 1200);
      return undefined as T;
    case "approve_pairing": {
      if (!/^\d{1,16}$/.test(String(args.pin))) fail("invalid", "Enter the PIN shown by Moonlight (digits only).");
      pairing = { ...pairing, requests: pairing.requests.map((r) => (r.request === args.request ? { ...r, approved: true } : r)) };
      emit("daemon://pairing", pairing);
      setTimeout(() => {
        pairing = { ...pairing, requests: pairing.requests.filter((r) => r.request !== args.request) };
        clients = {
          ...clients,
          clients: [
            ...clients.clients,
            {
              fingerprint: "4c1f9a0e7b2d3c5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6",
              client_ids: ["0123456789ABCDEF"],
              label: (args.label as string) || null,
              paired_at_ms: Date.now(),
              last_seen_ms: Date.now(),
              last_address: "10.0.0.57",
            },
          ],
        };
        emit("daemon://pairing", pairing);
        emit("daemon://pairing-resolved", { client_id: args.clientId, request: args.request, outcome: "paired" });
        emit("daemon://clients", clients);
      }, 1500);
      return undefined as T;
    }
    case "reject_pairing":
      pairing = { ...pairing, requests: pairing.requests.filter((r) => r.request !== args.request) };
      emit("daemon://pairing", pairing);
      emit("daemon://pairing-resolved", { client_id: args.clientId, request: args.request, outcome: "rejected" });
      return undefined as T;
    case "revoke_client":
      clients = { ...clients, clients: clients.clients.filter((c) => c.fingerprint !== args.fingerprint) };
      emit("daemon://clients", clients);
      return undefined as T;
    case "rename_client":
      clients = {
        ...clients,
        clients: clients.clients.map((c) => (c.fingerprint === args.fingerprint ? { ...c, label: (args.label as string) || null } : c)),
      };
      emit("daemon://clients", clients);
      return undefined as T;
    case "load_config":
      return { document: config, schema: fixture.schema } as T;
    case "validate_config": {
      const next = args.values as Record<string, Record<string, unknown>>;
      const issues = [] as { path: string | null; message: string }[];
      if (typeof next.address === "string" && !/^[0-9a-f:.]+$/i.test(next.address)) {
        issues.push({ path: null, message: `address '${next.address}' is not an IP address` });
      }
      return { valid: issues.length === 0, issues, changed_paths: [], restart_required: true } as T;
    }
    case "save_config":
      if (args.revision !== config.revision) fail("conflict", "The configuration file changed since it was loaded.");
      config = { ...config, values: args.values as ConfigValues, revision: `r${Date.now()}`, restart_required: true };
      emit("daemon://config-saved", { revision: config.revision, restart_required: true });
      return { revision: config.revision, changed_paths: [], restart_required: true } as T;
    case "quit":
      return undefined as T;
    default:
      return fail("failed", `Unknown command ${command}`);
  }
}

export function mockListen(event: string, handler: Handler): () => void {
  if (!listeners.has(event)) listeners.set(event, new Set());
  listeners.get(event)!.add(handler);
  return () => listeners.get(event)?.delete(handler);
}

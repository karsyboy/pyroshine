import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { HealthCheck, ServerEvent, ServerInfo } from "../api/types";
import { DiagnosticsPage, orderedChecks } from "./Diagnostics";

const daemon = vi.hoisted(() => ({
  server: null as ServerInfo | null,
  events: [] as ServerEvent[],
  notify: () => {},
}));
vi.mock("../state/daemon", () => ({ useDaemon: () => daemon, useNow: () => 1_000_000 }));

const check = (name: string, outcome: HealthCheck["outcome"], hint: string | null = null): HealthCheck => ({
  name,
  outcome,
  message: `${name} message`,
  duration_ms: 1,
  id: name.toLowerCase(),
  hint,
  fatal: outcome === "failed",
});

const server: ServerInfo = {
  api_version: 1,
  version: "test",
  name: "Host",
  pid: 1,
  started_at_ms: 0,
  config_path: "/config.toml",
  capabilities: { codecs: [{ id: "h264", label: "H.264" }], hdr_advertised: false, dma_buf: true, gpu: "GPU" },
  health: {
    all_fatal_passed: false,
    checks: [check("Vulkan", "passed"), check("uinput", "warning"), check("Render access", "failed", "Add your user to this group: `sudo usermod -aG render $USER`")],
    checked_at_ms: 1_000_000 - 30_000,
    rechecked: true,
  },
  listeners: { address: "0.0.0.0", http_port: 1, https_port: 2, rtsp_port: 3, video_port: 4, audio_port: 5, control_port: 6 },
  pairing_enabled: true,
};

describe("host checks", () => {
  it("lists problems first and keeps the daemon's order within each outcome", () => {
    const names = orderedChecks([check("A", "passed"), check("B", "warning"), check("C", "failed"), check("D", "warning")]).map(
      (c) => c.name,
    );
    expect(names).toEqual(["C", "B", "D", "A"]);
  });

  it("shows each problem with its fix and when the checks ran", () => {
    daemon.server = server;
    const html = renderToStaticMarkup(<DiagnosticsPage />);
    expect(html).toContain("1 failed · 1 warning");
    expect(html).toContain("Checked 30 s ago");
    expect(html).toMatch(/<code[^>]*>sudo usermod -aG render \$USER<\/code>/);
    expect(html.indexOf("Render access")).toBeLessThan(html.indexOf("Vulkan"));
    expect(html).toContain("Run checks");
    expect(html).toContain("Copy diagnostics");
    expect(html).toContain("Save support bundle");
  });

  it("explains a skipped startup check", () => {
    daemon.server = { ...server, health: null };
    expect(renderToStaticMarkup(<DiagnosticsPage />)).toContain("Skipped at startup");
  });
});

describe("recent problems", () => {
  it("lists events newest first with their hints", () => {
    daemon.server = server;
    daemon.events = [
      { level: "error", code: "codec_unsupported", message: "Older problem", at_ms: 1_000_000 - 120_000, hint: "Choose H.264." },
      { level: "warning", code: "application_stopped", message: "Newer problem", at_ms: 1_000_000 - 5_000 },
    ];
    const html = renderToStaticMarkup(<DiagnosticsPage />);
    expect(html.indexOf("Newer problem")).toBeLessThan(html.indexOf("Older problem"));
    expect(html).toContain("Choose H.264.");
    daemon.events = [];
    expect(renderToStaticMarkup(<DiagnosticsPage />)).toContain("No rejected launches");
  });
});

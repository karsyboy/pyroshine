import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { AttachError } from "../api/types";
import { Unavailable } from "./Unavailable";

const daemon = vi.hoisted(() => ({
  connected: false,
  loaded: true,
  attach_error: null as AttachError | null,
  refresh: () => Promise.resolve(),
}));
vi.mock("../state/daemon", () => ({ useDaemon: () => daemon }));

describe("unavailable daemon", () => {
  it("explains how to start a service that is not running", () => {
    daemon.attach_error = null;
    const html = renderToStaticMarkup(<Unavailable page="dashboard" />);
    expect(html).toContain("Pyroshine isn&#x27;t running");
    expect(html).toContain("systemctl status");
  });

  it("reports a transient attachment failure as being retried", () => {
    daemon.attach_error = { kind: "unavailable", message: "Bus not ready" };
    const html = renderToStaticMarkup(<Unavailable page="dashboard" />);
    expect(html).toContain("Pyroshine is running, but this app can&#x27;t connect to it");
    expect(html).toContain("Bus not ready");
    expect(html).toContain("keeps retrying automatically");
  });

  it("reports an incompatible daemon as a persistent error", () => {
    daemon.attach_error = { kind: "incompatible", message: "Unknown method GetStats" };
    const html = renderToStaticMarkup(<Unavailable page="settings" />);
    expect(html).toContain("Unknown method GetStats");
    expect(html).toContain("Retrying won&#x27;t help");
  });
});

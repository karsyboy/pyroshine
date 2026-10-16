import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { Unavailable } from "./Unavailable";

const daemon = vi.hoisted(() => ({
  connected: false,
  loaded: true,
  reconnecting: true,
  refresh: () => Promise.resolve(),
}));
vi.mock("../state/daemon", () => ({ useDaemon: () => daemon }));

describe("unreachable server", () => {
  it("explains how to start the service and that the page reconnects", () => {
    const html = renderToStaticMarkup(<Unavailable title="Dashboard" />);
    expect(html).toContain("Pyroshine can&#x27;t be reached");
    expect(html).toContain("systemctl status");
    expect(html).toContain("reconnects automatically");
    expect(html).toContain("Reconnecting");
  });

  it("mentions the tunnel for remote browsers", () => {
    daemon.reconnecting = false;
    const html = renderToStaticMarkup(<Unavailable title="Settings" />);
    expect(html).toContain("SSH tunnel or VPN");
    expect(html).not.toContain("Reconnecting");
  });
});

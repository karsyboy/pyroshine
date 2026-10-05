import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import fixture from "../api/session.fixture.json";
import type { SessionSnapshot } from "../api/types";
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

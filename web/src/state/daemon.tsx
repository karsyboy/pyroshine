import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { api } from "../api/client";
import type { StreamEventName, StreamEvents } from "../api/backend";
import type {
  HistoryEntry,
  LibrarySnapshot,
  Overview,
  PairingResolution,
  ServerEvent,
  StreamStats,
  UpdateStatus,
} from "../api/types";
import { useAuth } from "./auth";

/// Seconds of statistics kept for the dashboard's charts.
export const HISTORY = 90;
/// Events kept for the Diagnostics page (the server keeps 50).
export const EVENTS = 50;

interface Notice {
  id: number;
  severity: "success" | "info" | "warning" | "error";
  message: string;
}

interface DaemonState extends Overview {
  /// The first overview arrived (avoids flashing "not running" at startup).
  loaded: boolean;
  /// The live connection to the server is being re-established.
  reconnecting: boolean;
  /// This page asked the server to restart and it is not back yet.
  restarting: boolean;
  /// Call after asking the server to restart.
  expectRestart: () => void;
  history: StreamStats[];
  /// Recent server events, oldest first.
  events: ServerEvent[];
  notices: Notice[];
  /// The last pairing resolution, for the pairing page.
  resolution: PairingResolution | null;
  /// Bumped when the server saves the configuration.
  configRevision: string | null;
  /// The last session added to the history.
  lastHistoryEntry: HistoryEntry | null;
  /// The last automatic update check.
  update: UpdateStatus | null;
  /// The latest library snapshot the stream carried (while a page watches).
  librarySnapshot: LibrarySnapshot | null;
  notify: (severity: Notice["severity"], message: string) => void;
  dismiss: (id: number) => void;
  refresh: () => Promise<void>;
}

const empty: Overview = { connected: false, server: null, session: null, pairing: null, clients: null, stats: null };

const Context = createContext<DaemonState | null>(null);

const outcomes: Record<string, [Notice["severity"], string]> = {
  paired: ["success", "Client paired. It can now start streaming."],
  rejected: ["info", "Pairing request rejected."],
  expired: ["warning", "A pairing request expired. Start pairing again in Moonlight."],
  failed: ["error", "Pairing failed. Check that the PIN matches the one Moonlight shows, then try again."],
  cancelled: ["info", "The client cancelled pairing."],
  replaced: ["info", "The client sent a new pairing request."],
};

/// Live server state from the event stream. Every (re)connection starts
/// with a complete overview that replaces whatever was missed, so a lost
/// connection, a server restart or a reload never leaves stale state.
export function DaemonProvider({ children }: { children: ReactNode }) {
  const auth = useAuth();
  const [overview, setOverview] = useState<Overview>(empty);
  const [loaded, setLoaded] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);
  const [restarting, setRestarting] = useState(false);
  /// The start time of the server instance a restart was asked of.
  const restartedFrom = useRef<number | null>(null);
  const [history, setHistory] = useState<StreamStats[]>([]);
  const [events, setEvents] = useState<ServerEvent[]>([]);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [resolution, setResolution] = useState<PairingResolution | null>(null);
  const [configRevision, setConfigRevision] = useState<string | null>(null);
  const [lastHistoryEntry, setLastHistoryEntry] = useState<HistoryEntry | null>(null);
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  const [librarySnapshot, setLibrarySnapshot] = useState<LibrarySnapshot | null>(null);
  const nextNotice = useRef(1);

  const notify = useCallback((severity: Notice["severity"], message: string) => {
    const id = nextNotice.current++;
    // Keep at most three; a burst of events must not bury the page.
    setNotices((current) => [...current.slice(-2), { id, severity, message }]);
  }, []);
  const dismiss = useCallback((id: number) => setNotices((current) => current.filter((n) => n.id !== id)), []);

  const apply = useCallback((next: Overview) => {
    // A new server instance (its start time changed) completes a restart.
    if (restartedFrom.current !== null && next.server && next.server.started_at_ms !== restartedFrom.current) {
      restartedFrom.current = null;
      setRestarting(false);
      notify("success", "Pyroshine restarted.");
    }
    setOverview({ ...next, connected: true });
    setLoaded(true);
    setReconnecting(false);
    if (!next.stats) setHistory([]);
    api.recentEvents().then(setEvents, () => setEvents([]));
  }, [notify]);

  const expectRestart = useCallback(() => {
    restartedFrom.current = overview.server?.started_at_ms ?? null;
    setRestarting(true);
  }, [overview.server?.started_at_ms]);

  // A restart that never comes back falls back to the reconnecting notice.
  useEffect(() => {
    if (!restarting) return;
    const timer = setTimeout(() => {
      restartedFrom.current = null;
      setRestarting(false);
    }, 120_000);
    return () => clearTimeout(timer);
  }, [restarting]);

  const refresh = useCallback(async () => {
    apply(await api.overview());
  }, [apply]);

  const authenticated = auth.state?.authenticated ?? false;
  const { signedOut } = auth;
  useEffect(() => {
    if (!authenticated) return;
    const handlers: { [K in StreamEventName]?: (payload: StreamEvents[K]) => void } = {
      overview: apply,
      session: (session) =>
        setOverview((current) => ({ ...current, session, stats: session.phase === "streaming" ? current.stats : null })),
      pairing: (pairing) => setOverview((current) => ({ ...current, pairing })),
      clients: (clients) => setOverview((current) => ({ ...current, clients })),
      stats: (stats) => {
        setOverview((current) => ({ ...current, stats }));
        setHistory((current) => [...current.filter((s) => s.epoch === stats.epoch).slice(-(HISTORY - 1)), stats]);
      },
      "pairing-resolved": (resolved) => {
        setResolution(resolved);
        const outcome = outcomes[resolved.outcome];
        if (outcome) notify(outcome[0], outcome[1]);
      },
      "config-saved": (saved) => setConfigRevision(saved.revision),
      "server-event": (event) => {
        notify(event.level === "error" ? "error" : "warning", event.message);
        setEvents((current) => [...current.slice(-(EVENTS - 1)), event]);
      },
      health: (health) =>
        setOverview((current) => (current.server ? { ...current, server: { ...current.server, health } } : current)),
      history: setLastHistoryEntry,
      update: setUpdate,
      library: setLibrarySnapshot,
    };
    return api.stream({
      event: (name, payload) => (handlers[name] as ((payload: unknown) => void) | undefined)?.(payload),
      disconnected: () => {
        setReconnecting(true);
        setOverview((current) => ({ ...current, connected: false }));
        setLoaded(true);
      },
      signedOut: () => signedOut(),
    });
  }, [authenticated, apply, notify, signedOut]);

  // Clear history when the stream ends.
  useEffect(() => {
    if (overview.session?.phase !== "streaming") setHistory([]);
  }, [overview.session?.phase]);

  const value = useMemo(
    () => ({
      ...overview,
      loaded,
      reconnecting,
      restarting,
      expectRestart,
      history,
      events,
      notices,
      resolution,
      configRevision,
      lastHistoryEntry,
      update,
      librarySnapshot,
      notify,
      dismiss,
      refresh,
    }),
    [
      overview,
      loaded,
      reconnecting,
      restarting,
      expectRestart,
      history,
      events,
      notices,
      resolution,
      configRevision,
      lastHistoryEntry,
      update,
      librarySnapshot,
      notify,
      dismiss,
      refresh,
    ],
  );
  return <Context.Provider value={value}>{children}</Context.Provider>;
}

export function useDaemon(): DaemonState {
  const value = useContext(Context);
  if (!value) throw new Error("useDaemon outside DaemonProvider");
  return value;
}

/// Re-render every `interval` ms (for clocks and countdowns).
export function useNow(interval = 1000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), interval);
    return () => clearInterval(timer);
  }, [interval]);
  return now;
}

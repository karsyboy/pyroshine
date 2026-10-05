import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { call, on } from "../api/bridge";
import type {
  ClientsSnapshot,
  Overview,
  PairingResolution,
  PairingSnapshot,
  ServerEvent,
  SessionSnapshot,
  StreamStats,
} from "../api/types";

/// Seconds of statistics kept for the dashboard's charts.
export const HISTORY = 90;

interface Notice {
  id: number;
  severity: "success" | "info" | "warning" | "error";
  message: string;
}

interface DaemonState extends Overview {
  /// The first overview arrived (avoids flashing "not running" at startup).
  loaded: boolean;
  history: StreamStats[];
  notices: Notice[];
  /// The last pairing resolution, for the pairing page.
  resolution: PairingResolution | null;
  /// Bumped when the daemon saves the configuration.
  configRevision: string | null;
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

export function DaemonProvider({ children }: { children: ReactNode }) {
  const [overview, setOverview] = useState<Overview>(empty);
  const [loaded, setLoaded] = useState(false);
  const [history, setHistory] = useState<StreamStats[]>([]);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [resolution, setResolution] = useState<PairingResolution | null>(null);
  const [configRevision, setConfigRevision] = useState<string | null>(null);
  const nextNotice = useRef(1);

  const notify = useCallback((severity: Notice["severity"], message: string) => {
    const id = nextNotice.current++;
    // Keep at most three; a burst of events must not bury the window.
    setNotices((current) => [...current.slice(-2), { id, severity, message }]);
  }, []);
  const dismiss = useCallback((id: number) => setNotices((current) => current.filter((n) => n.id !== id)), []);

  const apply = useCallback((next: Overview) => {
    setOverview(next);
    setLoaded(true);
    if (!next.stats) setHistory([]);
  }, []);

  const refresh = useCallback(async () => {
    apply(await call<Overview>("refresh"));
  }, [apply]);

  useEffect(() => {
    const unlisten: Promise<() => void>[] = [
      on<Overview>("daemon://overview", apply),
      on<SessionSnapshot>("daemon://session", (session) =>
        setOverview((current) => ({ ...current, session, stats: session.phase === "streaming" ? current.stats : null })),
      ),
      on<PairingSnapshot>("daemon://pairing", (pairing) => setOverview((current) => ({ ...current, pairing }))),
      on<ClientsSnapshot>("daemon://clients", (clients) => setOverview((current) => ({ ...current, clients }))),
      on<StreamStats>("daemon://stats", (stats) => {
        setOverview((current) => ({ ...current, stats }));
        setHistory((current) => [...current.filter((s) => s.epoch === stats.epoch).slice(-(HISTORY - 1)), stats]);
      }),
      on<PairingResolution>("daemon://pairing-resolved", (resolved) => {
        setResolution(resolved);
        const outcome = outcomes[resolved.outcome];
        if (outcome) notify(outcome[0], outcome[1]);
      }),
      on<{ revision: string }>("daemon://config-saved", (saved) => setConfigRevision(saved.revision)),
      on<ServerEvent>("daemon://server-event", (event) =>
        notify(event.level === "error" ? "error" : "warning", event.message),
      ),
    ];
    // Subscribe first, then read the cached state, so nothing falls between.
    void call<Overview>("overview").then(apply);
    return () => {
      unlisten.forEach((promise) => void promise.then((stop) => stop()));
    };
  }, [apply, notify]);

  // Clear history when the stream ends.
  useEffect(() => {
    if (overview.session?.phase !== "streaming") setHistory([]);
  }, [overview.session?.phase]);

  const value = useMemo(
    () => ({ ...overview, loaded, history, notices, resolution, configRevision, notify, dismiss, refresh }),
    [overview, loaded, history, notices, resolution, configRevision, notify, dismiss, refresh],
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

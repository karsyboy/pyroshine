// The management API over HTTP: JSON requests to `/api/v1/` on the page's
// own origin with the session cookie, and the live state as Server-Sent
// Events. Changes carry the session's CSRF token (`X-CSRF-Token`), which the
// server hands out with the sign-in state; the cookie itself is HttpOnly and
// never visible here.

import type { Backend, StreamEventName, StreamHandlers } from "./backend";
import type { AuthState, UiError } from "./types";

const BASE = "/api/v1/";

/// The box art of library entry `key`, as clients get it. `version` (the
/// catalog generation) renews the browser's cached copy when art changes.
export function artworkUrl(key: string, version: number): string {
  return `${BASE}library/artwork?key=${encodeURIComponent(key)}&v=${version}`;
}

/** The CSRF token of the current session, from the last sign-in state. */
let csrf: string | null = null;
/** Called when a request finds the session gone. */
let onSignedOut: (() => void) | null = null;

export function setSignedOutHandler(handler: (() => void) | null) {
  onSignedOut = handler;
}

function remember(state: AuthState): AuthState {
  csrf = state.authenticated ? state.csrf_token : null;
  return state;
}

const unreachable: UiError = {
  kind: "unavailable",
  message: "Pyroshine cannot be reached. Check that the service is running; this page reconnects on its own.",
};

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (method !== "GET" && csrf) headers["X-CSRF-Token"] = csrf;
  let response: Response;
  try {
    response = await fetch(BASE + path, {
      method,
      headers,
      credentials: "same-origin",
      cache: "no-store",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw unreachable;
  }
  if (response.status === 204) return undefined as T;
  if (response.ok) return (await response.json()) as T;
  let error: UiError = { kind: "failed", message: `Pyroshine answered ${response.status}.` };
  try {
    const parsed = (await response.json()) as { error?: UiError };
    if (parsed.error) error = parsed.error;
  } catch {
    // Not a management error document (for example a proxy's page).
  }
  if (response.status === 401 && error.kind === "unauthenticated") {
    csrf = null;
    onSignedOut?.();
  }
  throw error;
}

const json = <T>(path: string) => request<T>("GET", path);

/** Names forwarded from the event stream. */
const EVENTS: StreamEventName[] = [
  "overview",
  "session",
  "pairing",
  "pairing-requested",
  "pairing-resolved",
  "clients",
  "config-saved",
  "stats",
  "server-event",
  "health",
  "library",
  "history",
  "update",
];

function stream(handlers: StreamHandlers): () => void {
  let source: EventSource | null = null;
  let stopped = false;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let delay = 1000;

  const open = () => {
    if (stopped) return;
    source = new EventSource(BASE + "stream", { withCredentials: true });
    for (const name of EVENTS) {
      source.addEventListener(name, (message) => {
        delay = 1000;
        try {
          handlers.event(name, JSON.parse((message as MessageEvent<string>).data));
        } catch {
          // A malformed event is skipped; the next overview corrects state.
        }
      });
    }
    source.addEventListener("auth", () => {
      close();
      csrf = null;
      handlers.signedOut();
    });
    source.onerror = () => {
      if (stopped || !source) return;
      handlers.disconnected();
      // EventSource retries a dropped connection itself, but gives up on an
      // error response (signed out, server restarting behind a proxy). Then
      // ask why, and reopen with a growing delay.
      if (source.readyState === EventSource.CLOSED) {
        close();
        void json<AuthState>("auth").then(
          (state) => {
            if (!state.authenticated) {
              csrf = null;
              handlers.signedOut();
            } else {
              remember(state);
              schedule();
            }
          },
          () => schedule(),
        );
      }
    };
  };
  const close = () => {
    source?.close();
    source = null;
  };
  const schedule = () => {
    if (stopped) return;
    retry = setTimeout(open, delay);
    delay = Math.min(delay * 2, 15_000);
  };

  open();
  return () => {
    stopped = true;
    if (retry) clearTimeout(retry);
    close();
  };
}

/** The file name the server suggests in `Content-Disposition`. */
function attachmentName(response: Response, fallback: string): string {
  const header = response.headers.get("Content-Disposition") ?? "";
  const match = /filename="([^"\\/]+)"/.exec(header);
  return match ? match[1] : fallback;
}

export const http: Backend = {
  auth: () => json<AuthState>("auth").then(remember),
  setup: (code, password) => request<AuthState>("POST", "auth/setup", { code, password }).then(remember),
  login: (password) => request<AuthState>("POST", "auth/login", { password }).then(remember),
  logout: async () => {
    await request("POST", "auth/logout", {});
    csrf = null;
  },
  changePassword: (current, next) => request("POST", "auth/password", { current, new: next }),
  webSessions: () => json("auth/sessions"),
  revokeWebSession: (id) => request("DELETE", `auth/sessions/${encodeURIComponent(id)}`),

  overview: () => json("overview"),
  endSession: () => request("POST", "session/end", {}),
  desktopStatus: () => json("desktop"),
  authorizeDesktop: () => request("POST", "desktop/authorize", {}),
  restoreDesktop: () => request("POST", "desktop/restore", {}),
  restart: () => request("POST", "server/restart", {}),
  approvePairing: (clientId, requestToken, pin, label) =>
    request("POST", "pairing/approve", { client_id: clientId, request: requestToken, pin, label }),
  rejectPairing: (clientId, requestToken) => request("POST", "pairing/reject", { client_id: clientId, request: requestToken }),
  renameClient: (fingerprint, label) => request("POST", "clients/rename", { fingerprint, label }),
  revokeClient: (fingerprint) => request("POST", "clients/revoke", { fingerprint }),

  loadConfig: () => json("config"),
  validateConfig: (values) => request("POST", "config/validate", { values }),
  saveConfig: (values, revision) => request("PUT", "config", { values, revision }),

  library: () => json("library"),
  rescanLibrary: () => request("POST", "library/rescan", {}),
  setApplicationHidden: (key, hidden) => request("POST", "library/hidden", { key, hidden }),

  recentEvents: () => json("events/recent"),
  runHealthCheck: () => request("POST", "health/run", {}),
  diagnosticReport: async () => JSON.stringify(await json<unknown>("diagnostics/report"), null, 2),
  supportBundle: async () => {
    let response: Response;
    try {
      response = await fetch(BASE + "diagnostics/bundle", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json", ...(csrf ? { "X-CSRF-Token": csrf } : {}) },
        body: "{}",
      });
    } catch {
      throw unreachable;
    }
    if (!response.ok) {
      const parsed = (await response.json().catch(() => ({}))) as { error?: UiError };
      if (response.status === 401) onSignedOut?.();
      throw parsed.error ?? { kind: "failed", message: `Pyroshine answered ${response.status}.` };
    }
    return { blob: await response.blob(), name: attachmentName(response, "pyroshine-diag.tar.gz") };
  },

  history: (limit, before) => json(`history?limit=${limit}${before === undefined ? "" : `&before=${before}`}`),
  historyEntry: (id) => json(`history/${id}`),
  clearHistory: () => request("DELETE", "history"),

  updates: () => json("updates"),
  checkUpdates: () => request("POST", "updates/check", {}),
  updateSettings: (automatic, channel) => request("PUT", "updates/settings", { automatic, channel }),
  dismissUpdate: (version) => request("POST", "updates/dismiss", { version }),

  stream,
};

// The backend the UI talks to. Production builds always use the server's
// HTTP API. During development (`npm run dev`), `?mock=<scenario>` in the
// address selects the simulated server instead; without it the dev server
// proxies `/api` to a running Pyroshine (see vite.config.ts).

import type { Backend, StreamHandlers } from "./backend";
import { http } from "./http";

const mocked =
  import.meta.env.DEV && typeof window !== "undefined" && new URLSearchParams(window.location.search).has("mock");

const backend: Promise<Backend> = mocked ? import("./mock").then((module) => module.mock) : Promise.resolve(http);

type Method = Exclude<keyof Backend, "stream">;

function delegate<K extends Method>(key: K): Backend[K] {
  return ((...args: unknown[]) =>
    backend.then((resolved) => (resolved[key] as (...rest: unknown[]) => unknown)(...args))) as Backend[K];
}

export const api: Backend = {
  auth: delegate("auth"),
  setup: delegate("setup"),
  login: delegate("login"),
  logout: delegate("logout"),
  changePassword: delegate("changePassword"),
  webSessions: delegate("webSessions"),
  revokeWebSession: delegate("revokeWebSession"),
  overview: delegate("overview"),
  endSession: delegate("endSession"),
  approvePairing: delegate("approvePairing"),
  rejectPairing: delegate("rejectPairing"),
  renameClient: delegate("renameClient"),
  revokeClient: delegate("revokeClient"),
  loadConfig: delegate("loadConfig"),
  validateConfig: delegate("validateConfig"),
  saveConfig: delegate("saveConfig"),
  library: delegate("library"),
  rescanLibrary: delegate("rescanLibrary"),
  setApplicationHidden: delegate("setApplicationHidden"),
  recentEvents: delegate("recentEvents"),
  runHealthCheck: delegate("runHealthCheck"),
  diagnosticReport: delegate("diagnosticReport"),
  supportBundle: delegate("supportBundle"),
  history: delegate("history"),
  historyEntry: delegate("historyEntry"),
  clearHistory: delegate("clearHistory"),
  updates: delegate("updates"),
  checkUpdates: delegate("checkUpdates"),
  updateSettings: delegate("updateSettings"),
  dismissUpdate: delegate("dismissUpdate"),
  stream: (handlers: StreamHandlers) => {
    let stop: (() => void) | null = null;
    let cancelled = false;
    void backend.then((resolved) => {
      if (!cancelled) stop = resolved.stream(handlers);
    });
    return () => {
      cancelled = true;
      stop?.();
    };
  },
};

/** Save `blob` as a download named `name`. */
export function download(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.rel = "noopener";
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

export { errorKind, errorMessage } from "./backend";

// The management API as the UI uses it; `http.ts` implements it against the
// Pyroshine server (`/api/v1/`).

import type {
  DesktopStatus,
  AuthState,
  ClientsSnapshot,
  ConfigDocument,
  ConfigSchema,
  ConfigValues,
  HealthReport,
  HistoryEntry,
  HistoryPage,
  LibrarySnapshot,
  Overview,
  PairingResolution,
  PairingSnapshot,
  SaveOutcome,
  ServerEvent,
  SessionSnapshot,
  StreamStats,
  UiError,
  UpdateStatus,
  ValidationReport,
  WebSession,
} from "./types";

/** Events of the live stream (`GET /api/v1/stream`), by name. */
export interface StreamEvents {
  overview: Overview;
  session: SessionSnapshot;
  pairing: PairingSnapshot;
  "pairing-requested": unknown;
  "pairing-resolved": PairingResolution;
  clients: ClientsSnapshot;
  "config-saved": { revision: string; restart_required: boolean };
  stats: StreamStats;
  "server-event": ServerEvent;
  health: HealthReport;
  library: LibrarySnapshot;
  history: HistoryEntry;
  update: UpdateStatus;
}

export type StreamEventName = keyof StreamEvents;

export interface StreamHandlers {
  /** A named event arrived. A stream always starts with `overview`. */
  event: <K extends StreamEventName>(name: K, payload: StreamEvents[K]) => void;
  /** The stream is lost and being re-established (server restart, network). */
  disconnected: () => void;
  /** The session ended; sign in again. */
  signedOut: () => void;
}

export interface Backend {
  auth(): Promise<AuthState>;
  setup(code: string, password: string): Promise<AuthState>;
  login(password: string): Promise<AuthState>;
  logout(): Promise<void>;
  changePassword(current: string, next: string): Promise<void>;
  webSessions(): Promise<WebSession[]>;
  revokeWebSession(id: string): Promise<void>;

  overview(): Promise<Overview>;
  endSession(): Promise<void>;
  desktopStatus(): Promise<DesktopStatus>;
  authorizeDesktop(): Promise<void>;
  restoreDesktop(): Promise<DesktopStatus>;
  /// Shut down normally and start again; ends a running session.
  restart(): Promise<void>;
  approvePairing(clientId: string, request: string, pin: string, label: string): Promise<void>;
  rejectPairing(clientId: string, request: string): Promise<void>;
  renameClient(fingerprint: string, label: string): Promise<void>;
  revokeClient(fingerprint: string): Promise<void>;

  loadConfig(): Promise<{ document: ConfigDocument; schema: ConfigSchema }>;
  validateConfig(values: ConfigValues): Promise<ValidationReport>;
  saveConfig(values: ConfigValues, revision: string): Promise<SaveOutcome>;

  library(): Promise<LibrarySnapshot>;
  rescanLibrary(): Promise<void>;
  setApplicationHidden(key: string, hidden: boolean): Promise<void>;

  recentEvents(): Promise<ServerEvent[]>;
  runHealthCheck(): Promise<HealthReport>;
  /** The redacted diagnostic report, as indented JSON text. */
  diagnosticReport(): Promise<string>;
  /** A redacted support bundle (`.tar.gz`). */
  supportBundle(): Promise<{ blob: Blob; name: string }>;

  history(limit: number, before?: number): Promise<HistoryPage>;
  historyEntry(id: number): Promise<HistoryEntry>;
  clearHistory(): Promise<void>;

  updates(): Promise<UpdateStatus>;
  checkUpdates(): Promise<UpdateStatus>;
  updateSettings(automatic: boolean, channel: UpdateStatus["channel"]): Promise<UpdateStatus>;
  dismissUpdate(version: string): Promise<UpdateStatus>;

  /** Follow live state; returns the function that stops following. */
  stream(handlers: StreamHandlers): () => void;
}

export function errorMessage(error: unknown): string {
  if (error && typeof error === "object" && "message" in error) {
    return String((error as UiError).message);
  }
  return String(error);
}

export function errorKind(error: unknown): string {
  if (error && typeof error === "object" && "kind" in error) {
    return String((error as UiError).kind);
  }
  return "failed";
}

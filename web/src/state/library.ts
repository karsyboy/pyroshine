import { useCallback, useEffect, useState } from "react";
import { api, errorKind, errorMessage } from "../api/client";
import type { LibrarySnapshot } from "../api/types";
import { useDaemon } from "./daemon";

export interface LibraryState {
  snapshot: LibrarySnapshot | null;
  /// Why the library cannot be shown (`incompatible`: the server predates it).
  error: { kind: string; message: string } | null;
  reload: () => Promise<void>;
}

/// The server's application catalog: read when the page opens (or the server
/// reconnects), then kept current from the event stream's library events.
/// Scans run in the server; this only receives their results, so the page
/// stays responsive however large the library is.
export function useLibrary(connected: boolean): LibraryState {
  const { librarySnapshot } = useDaemon();
  const [snapshot, setSnapshot] = useState<LibrarySnapshot | null>(null);
  const [error, setError] = useState<LibraryState["error"]>(null);

  const reload = useCallback(async () => {
    try {
      setSnapshot(await api.library());
      setError(null);
    } catch (e) {
      setError({ kind: errorKind(e), message: errorMessage(e) });
    }
  }, []);

  useEffect(() => {
    if (connected) void reload();
  }, [connected, reload]);

  useEffect(() => {
    if (librarySnapshot) {
      setSnapshot(librarySnapshot);
      setError(null);
    }
  }, [librarySnapshot]);

  return { snapshot, error, reload };
}

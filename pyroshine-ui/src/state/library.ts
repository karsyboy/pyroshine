import { useCallback, useEffect, useState } from "react";
import { call, errorKind, errorMessage, on } from "../api/bridge";
import type { LibrarySnapshot } from "../api/types";

export interface LibraryState {
  snapshot: LibrarySnapshot | null;
  /// Why the library cannot be shown (`incompatible`: the daemon predates it).
  error: { kind: string; message: string } | null;
  reload: () => Promise<void>;
}

/// The daemon's application catalog, kept current from `daemon://library`.
/// Scans run in the daemon; this only receives their results, so the window
/// stays responsive however large the library is.
export function useLibrary(connected: boolean): LibraryState {
  const [snapshot, setSnapshot] = useState<LibrarySnapshot | null>(null);
  const [error, setError] = useState<LibraryState["error"]>(null);

  const reload = useCallback(async () => {
    try {
      setSnapshot(await call<LibrarySnapshot>("library"));
      setError(null);
    } catch (e) {
      setError({ kind: errorKind(e), message: errorMessage(e) });
    }
  }, []);

  useEffect(() => {
    // Subscribe first, then read, so no change falls between.
    const stop = on<LibrarySnapshot>("daemon://library", (next) => {
      setSnapshot(next);
      setError(null);
    });
    if (connected) void reload();
    return () => {
      void stop.then((unlisten) => unlisten());
    };
  }, [connected, reload]);

  return { snapshot, error, reload };
}

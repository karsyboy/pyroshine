// The static `[[application]]` entries of the configuration file, edited on
// the Applications page. Saves go through the same validation and revision
// check as the settings page; like every setting they apply after a restart.

import { useCallback, useEffect, useState } from "react";
import { api, errorKind, errorMessage } from "../api/client";
import type { ConfigDocument, ConfigSchema, FieldSpec, LibraryApplication } from "../api/types";
import { getPath, normalize, setPath } from "../config/draft";
import { useDaemon } from "./daemon";

export type Item = Record<string, unknown>;

export interface ManualApplications {
  /// Settings of one entry; null until the configuration has loaded.
  fields: FieldSpec[] | null;
  items: Item[];
  writable: boolean;
  readOnlyReason: string | null;
  /// Saved changes wait for a restart.
  restartRequired: boolean;
  error: string | null;
  /// Replace the list and save the file. Rejects with the reason.
  save: (items: Item[]) => Promise<void>;
}

/// The configuration entry behind a manual library entry (`manual:<index>`),
/// confirmed by its title: the file may have changed since the server started.
export function configIndex(application: LibraryApplication, items: Item[]): number | null {
  const index = Number(application.key.replace(/^manual:/, ""));
  if (Number.isInteger(index) && items[index]?.title === application.title) return index;
  const byTitle = items.findIndex((item) => item.title === application.title);
  return byTitle >= 0 ? byTitle : null;
}

export function useManualApplications(): ManualApplications {
  const daemon = useDaemon();
  const [loaded, setLoaded] = useState<{ document: ConfigDocument; schema: ConfigSchema } | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setLoaded(await api.loadConfig());
      setError(null);
    } catch (failure) {
      setError(errorMessage(failure));
    }
  }, []);

  // Reload when the server comes (back), restarts or the file is saved elsewhere.
  const startedAt = daemon.server?.started_at_ms;
  useEffect(() => {
    if (daemon.connected) void load();
  }, [daemon.connected, startedAt, daemon.configRevision, load]);

  const field = loaded?.schema.fields.find((candidate) => candidate.kind.type === "applications") ?? null;
  const items = (field && loaded ? (getPath(loaded.document.values, field.path) as Item[] | undefined) : undefined) ?? [];

  const save = useCallback(
    async (next: Item[]) => {
      if (!loaded || !field) throw new Error("The configuration is not loaded yet.");
      const values = normalize(setPath(structuredClone(loaded.document.values), field.path, next), loaded.schema.fields);
      const report = await api.validateConfig(values);
      if (!report.valid) throw new Error(report.issues.map((issue) => issue.message).join(" "));
      try {
        await api.saveConfig(values, loaded.document.revision);
      } catch (failure) {
        if (errorKind(failure) === "conflict") {
          await load();
          throw new Error("The configuration file changed meanwhile; it was reloaded. Check the entry and save again.");
        }
        throw failure;
      }
      await load();
    },
    [loaded, field, load],
  );

  return {
    fields: field && field.kind.type === "applications" ? field.kind.item : null,
    items,
    writable: loaded?.document.writable ?? false,
    readOnlyReason: loaded?.document.read_only_reason ?? null,
    restartRequired: loaded?.document.restart_required ?? false,
    error,
    save,
  };
}

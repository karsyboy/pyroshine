// The configuration the Applications page edits: static `[[application]]`
// entries, `[[application_scanner]]` entries and the discovery options
// (`[library]`). Saves go through the same validation and revision check as
// the settings page; like every setting they apply after a restart.

import { useCallback, useEffect, useState } from "react";
import { api, errorKind, errorMessage } from "../api/client";
import type { ConfigDocument, ConfigSchema, FieldSpec, LibraryApplication, LibrarySource, ScannerVariant } from "../api/types";
import { getPath, normalize, setPath } from "../config/draft";
import { useDaemon } from "./daemon";

export type Item = Record<string, unknown>;

export interface LibraryConfig {
  /// False until the configuration has loaded.
  ready: boolean;
  /// Settings of one static application.
  applicationFields: FieldSpec[];
  applications: Item[];
  scannerVariants: ScannerVariant[];
  scanners: Item[];
  /// Discovery options beside the scanners (automatic updates).
  discoveryFields: FieldSpec[];
  /// The saved value of a setting.
  value: (path: string) => unknown;
  writable: boolean;
  readOnlyReason: string | null;
  /// Saved changes wait for a restart.
  restartRequired: boolean;
  error: string | null;
  /// Static applications.
  saveApplications: (items: Item[]) => Promise<void>;
  saveScanners: (items: Item[]) => Promise<void>;
  /// Set settings by path and save the file. Rejects with the reason.
  save: (changes: Record<string, unknown>) => Promise<void>;
}

/// The configuration entry behind a manual library entry (`manual:<index>`),
/// confirmed by its title: the file may have changed since the server started.
export function configIndex(application: LibraryApplication, items: Item[]): number | null {
  const index = Number(application.key.replace(/^manual:/, ""));
  if (Number.isInteger(index) && items[index]?.title === application.title) return index;
  const byTitle = items.findIndex((item) => item.title === application.title);
  return byTitle >= 0 ? byTitle : null;
}

/// The configuration entry behind a running scanner, confirmed by its type.
export function scannerIndex(source: LibrarySource, items: Item[]): number | null {
  if (items[source.scanner]?.type === source.kind) return source.scanner;
  const ofKind = items.flatMap((item, index) => (item.type === source.kind ? [index] : []));
  return ofKind.length === 1 ? ofKind[0] : null;
}

export function useLibraryConfig(): LibraryConfig {
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

  const fields = loaded?.schema.fields ?? [];
  const applicationsField = fields.find((field) => field.kind.type === "applications");
  const scannersField = fields.find((field) => field.kind.type === "scanners");
  const values = loaded?.document.values;
  const list = (field: FieldSpec | undefined) => ((field && values ? getPath(values, field.path) : undefined) as Item[] | undefined) ?? [];

  const save = useCallback(
    async (changes: Record<string, unknown>) => {
      if (!loaded) throw new Error("The configuration is not loaded yet.");
      let next = structuredClone(loaded.document.values);
      for (const [path, value] of Object.entries(changes)) next = setPath(next, path, value);
      next = normalize(next, loaded.schema.fields);
      const report = await api.validateConfig(next);
      if (!report.valid) throw new Error(report.issues.map((issue) => issue.message).join(" "));
      try {
        await api.saveConfig(next, loaded.document.revision);
      } catch (failure) {
        if (errorKind(failure) === "conflict") {
          await load();
          throw new Error("The configuration file changed meanwhile; it was reloaded. Check the change and save again.");
        }
        throw failure;
      }
      await load();
    },
    [loaded, load],
  );

  return {
    ready: loaded !== null && applicationsField !== undefined && scannersField !== undefined,
    applicationFields: applicationsField?.kind.type === "applications" ? applicationsField.kind.item : [],
    applications: list(applicationsField),
    scannerVariants: scannersField?.kind.type === "scanners" ? scannersField.kind.variants : [],
    scanners: list(scannersField),
    discoveryFields: scannersField ? fields.filter((field) => field.section === scannersField.section && field !== scannersField) : [],
    value: (path) => (values ? getPath(values, path) : undefined),
    writable: loaded?.document.writable ?? false,
    readOnlyReason: loaded?.document.read_only_reason ?? null,
    restartRequired: loaded?.document.restart_required ?? false,
    error,
    saveApplications: (items) => save(applicationsField ? { [applicationsField.path]: items } : {}),
    saveScanners: (items) => save(scannersField ? { [scannersField.path]: items } : {}),
    save,
  };
}

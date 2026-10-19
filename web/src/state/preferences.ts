import { useSyncExternalStore } from "react";

/// Preferences of this browser. They only change what the page shows, so
/// they live in the browser's storage, apply at once and need no server
/// restart; the server's reports (Copy diagnostics) are unaffected.
export interface Preferences {
  /// Show the dashboard's informational findings, not only its warnings.
  infoFindings: boolean;
}

const KEY = "pyroshine.preferences";
const defaults: Preferences = { infoFindings: true };

const listeners = new Set<() => void>();
// Through `window`: Node (the tests) has no window, and touching its own
// experimental localStorage only prints a warning.
const storage = () => (typeof window === "undefined" ? undefined : window.localStorage);
let current = load();

/// Storage can be missing (tests) or refuse access; fall back to defaults.
function load(): Preferences {
  try {
    const stored: unknown = JSON.parse(storage()?.getItem(KEY) ?? "null");
    if (stored && typeof stored === "object") {
      const values = stored as Partial<Record<keyof Preferences, unknown>>;
      return { infoFindings: typeof values.infoFindings === "boolean" ? values.infoFindings : defaults.infoFindings };
    }
  } catch {
    // Unreadable or corrupt: keep the defaults.
  }
  return defaults;
}

export function setPreference<K extends keyof Preferences>(key: K, value: Preferences[K]) {
  current = { ...current, [key]: value };
  try {
    storage()?.setItem(KEY, JSON.stringify(current));
  } catch {
    // Applies for this run even when it cannot be stored.
  }
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function usePreferences(): Preferences {
  return useSyncExternalStore(subscribe, () => current, () => current);
}

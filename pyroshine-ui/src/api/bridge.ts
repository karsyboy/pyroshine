// Calls into the app's Rust side. In a plain browser (`npm run dev` without
// Tauri) a mock backend stands in, for UI development only; production
// builds never include it.

import type { UiError } from "./types";

const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

type Handler<T> = (payload: T) => void;

async function mock() {
  if (!import.meta.env.DEV) {
    throw { kind: "unavailable", message: "Open Pyroshine from the desktop app." } satisfies UiError;
  }
  return import("./mock");
}

export async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (inTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>(command, args);
  }
  return (await mock()).mockInvoke<T>(command, args);
}

export async function on<T>(event: string, handler: Handler<T>): Promise<() => void> {
  if (inTauri) {
    const { listen } = await import("@tauri-apps/api/event");
    return listen<T>(event, (message) => handler(message.payload));
  }
  return (await mock()).mockListen(event, handler as Handler<unknown>);
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

// Pure helpers for editing a configuration draft. Paths use the daemon's
// dotted setting names (`stream.video.fec_mode`).

import type { ConfigValues, FieldSpec } from "../api/types";

export function getPath(values: unknown, path: string): unknown {
  return path.split(".").reduce<unknown>((value, key) => {
    if (value && typeof value === "object") return (value as Record<string, unknown>)[key];
    return undefined;
  }, values);
}

/// A copy of `values` with `path` set; `undefined` removes the key, which lets
/// the daemon apply the setting's default.
export function setPath<T extends ConfigValues>(values: T, path: string, value: unknown): T {
  const [key, ...rest] = path.split(".");
  const copy: Record<string, unknown> = Array.isArray(values) ? [...values] as unknown as Record<string, unknown> : { ...values };
  if (rest.length === 0) {
    if (value === undefined) delete copy[key];
    else copy[key] = value;
  } else {
    const child = copy[key] && typeof copy[key] === "object" ? (copy[key] as ConfigValues) : {};
    copy[key] = setPath(child, rest.join("."), value);
  }
  return copy as T;
}

export function equal(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (a == null || b == null) return (a ?? null) === (b ?? null);
  if (typeof a !== typeof b || typeof a !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const other = b as unknown[];
    return a.length === other.length && a.every((item, index) => equal(item, other[index]));
  }
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
  return [...keys].every((key) => equal(left[key], right[key]));
}

/// Schema fields whose draft value differs from the saved value.
export function changedFields(fields: FieldSpec[], saved: ConfigValues, draft: ConfigValues): string[] {
  return fields.filter((field) => !equal(getPath(saved, field.path), getPath(draft, field.path))).map((field) => field.path);
}

export function move<T>(items: T[], from: number, to: number): T[] {
  if (to < 0 || to >= items.length || from === to) return items;
  const copy = [...items];
  const [item] = copy.splice(from, 1);
  copy.splice(to, 0, item);
  return copy;
}

/// Map daemon issue paths onto schema fields: `application[2].command` and
/// `application` both belong to the applications field.
export function issueField(path: string | null): string | null {
  if (!path) return null;
  return path.replace(/\[\d+\].*$/, "");
}

function cleanCommand(command: unknown): unknown {
  return Array.isArray(command) ? command.filter((arg) => String(arg) !== "") : command;
}

function cleanItem(item: Record<string, unknown>, fields: FieldSpec[]): Record<string, unknown> {
  const next = { ...item };
  for (const field of fields) {
    const value = next[field.path];
    if (!Array.isArray(value)) continue;
    if (field.kind.type === "command") next[field.path] = cleanCommand(value);
    if (field.kind.type === "path_list") next[field.path] = value.filter((path) => String(path).trim() !== "");
    if (field.kind.type === "command_list") {
      next[field.path] = value.map(cleanCommand).filter((command) => Array.isArray(command) && command.length > 0);
    }
  }
  return next;
}

/// Drop the empty rows the list editors leave behind (an unfilled argument,
/// directory or hook), so they are not saved as empty strings.
export function normalize(values: ConfigValues, fields: FieldSpec[]): ConfigValues {
  let next = values;
  for (const field of fields) {
    const list = getPath(values, field.path);
    if (!Array.isArray(list)) continue;
    if (field.kind.type === "applications") {
      const item = field.kind.item;
      next = setPath(next, field.path, list.map((entry) => cleanItem(entry as Record<string, unknown>, item)));
    }
    if (field.kind.type === "scanners") {
      const variants = field.kind.variants;
      next = setPath(
        next,
        field.path,
        list.map((entry) => {
          const scanner = entry as Record<string, unknown>;
          const variant = variants.find((candidate) => candidate.id === scanner.type);
          return variant ? cleanItem(scanner, variant.fields) : scanner;
        }),
      );
    }
  }
  return next;
}

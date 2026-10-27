import { describe, expect, it } from "vitest";
import fixture from "../api/schema.fixture.json";
import type { ConfigSchema, FieldKind } from "../api/types";
import { changedFields, equal, getPath, issueField, move, setPath } from "./draft";
import { renderedKinds } from "./kinds";

const schema = fixture.schema as ConfigSchema;
const defaults = fixture.defaults as Record<string, unknown>;

describe("draft helpers", () => {
  it("reads and writes nested settings without mutating the original", () => {
    const next = setPath(defaults, "stream.video.fec_mode", "auto");
    expect(getPath(next, "stream.video.fec_mode")).toBe("auto");
    expect(getPath(defaults, "stream.video.fec_mode")).toBe("fixed");
    expect(getPath(next, "stream.audio.port")).toBe(getPath(defaults, "stream.audio.port"));
  });

  it("removes keys so the daemon applies defaults", () => {
    const next = setPath({ a: { b: 1, c: 2 } }, "a.b", undefined);
    expect(next).toEqual({ a: { c: 2 } });
  });

  it("treats missing and null optional values as equal", () => {
    expect(equal({ gpu: null }, {})).toBe(true);
    expect(equal([["a"]], [["a"]])).toBe(true);
    expect(equal([["a"]], [["a", "b"]])).toBe(false);
  });

  it("reports changed schema fields", () => {
    const draft = setPath(setPath(defaults, "name", "Other"), "compositor.keyboard.options", "caps:escape");
    expect(changedFields(schema.fields, defaults, draft).sort()).toEqual(["compositor.keyboard.options", "name"]);
  });

  it("reorders list entries", () => {
    expect(move(["a", "b", "c"], 2, 0)).toEqual(["c", "a", "b"]);
    expect(move(["a", "b"], 0, 5)).toEqual(["a", "b"]);
  });

  it("maps issue paths to fields", () => {
    expect(issueField("application[2].command")).toBe("application");
    expect(issueField("stream.video.port")).toBe("stream.video.port");
    expect(issueField(null)).toBeNull();
  });
});

describe("schema contract", () => {
  // The fixture is the daemon's real schema (kept current by a Rust test).
  const kinds = new Set<string>();
  const visit = (kind: FieldKind) => {
    kinds.add(kind.type);
    if (kind.type === "applications") kind.item.forEach((field) => visit(field.kind));
    if (kind.type === "scanners") kind.variants.forEach((variant) => variant.fields.forEach((field) => visit(field.kind)));
  };
  schema.fields.forEach((field) => visit(field.kind));

  it("only uses field kinds the editor renders", () => {
    for (const kind of kinds) expect(renderedKinds).toContain(kind);
  });

  it("places every field in a known section with a value in the defaults", () => {
    const sections = new Set(schema.sections.map((section) => section.id));
    for (const field of schema.fields) {
      expect(sections.has(field.section), field.path).toBe(true);
      const optional = "optional" in field.kind && field.kind.optional;
      const unset = field.kind.type === "choice" && field.kind.unset_label !== null;
      if (!optional && !unset) expect(getPath(defaults, field.path), field.path).not.toBeUndefined();
    }
  });
});

describe("normalize", () => {
  it("drops empty arguments, directories and hooks from list entries", async () => {
    const { normalize } = await import("./draft");
    const values = setPath(
      setPath(defaults, "application", [
        { title: "Game", command: ["/usr/bin/game", "", "--flag"], pre_command: [[""], ["/usr/bin/true", ""]], launch_timeout_secs: 2 },
      ]),
      "application_scanner",
      [{ type: "desktop", directories: ["/a", " ", ""], include_terminal: false, resolve_icons: true, launch_timeout_secs: 2 }],
    );
    const clean = normalize(values, schema.fields);
    expect(getPath(clean, "application")).toEqual([
      { title: "Game", command: ["/usr/bin/game", "--flag"], pre_command: [["/usr/bin/true"]], launch_timeout_secs: 2 },
    ]);
    expect((getPath(clean, "application_scanner") as Record<string, unknown>[])[0].directories).toEqual(["/a"]);
  });
});

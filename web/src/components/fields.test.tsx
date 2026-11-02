import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { FieldSpec } from "../api/types";
import { FieldControl } from "./fields";

const env: FieldSpec = {
  path: "env",
  section: "applications",
  label: "Environment",
  help: "Extra environment variables.",
  kind: { type: "environment" },
  advanced: true,
  caution: null,
  required: false,
};

const render = (value: unknown) => renderToStaticMarkup(<FieldControl spec={env} value={value} onChange={() => {}} />);

describe("environment field", () => {
  it("shows one row per variable", () => {
    const html = render({ COSMIC_BACKEND: "winit", DXVK_HUD: "fps" });
    expect(html).toContain('value="COSMIC_BACKEND"');
    expect(html).toContain('value="winit"');
    expect(html).toContain('value="DXVK_HUD"');
    expect(html).toContain("Add variable");
  });

  it("offers an empty row when unset", () => {
    const html = render(null);
    expect(html).toContain('aria-label="Variable 1 name"');
    expect(html).not.toContain('aria-label="Variable 2 name"');
  });

  it("flags names systemd would reject", () => {
    expect(render({ "1BAD": "x" })).toContain("Letters, digits and _");
    expect(render({ GOOD_1: "x" })).not.toContain("Letters, digits and _");
  });
});

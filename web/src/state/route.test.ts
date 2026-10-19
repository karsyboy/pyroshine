import { describe, expect, it } from "vitest";
import { parse } from "./route";

describe("routes", () => {
  it("opens the pages the tray and notifications link to", () => {
    expect(parse("#/dashboard").page).toBe("dashboard");
    const request = parse("#/clients?request=9f2c4e1ab7d34c0e8a61f0c2b5d9e7a4");
    expect(request.page).toBe("clients");
    expect(request.params.get("request")).toBe("9f2c4e1ab7d34c0e8a61f0c2b5d9e7a4");
    expect(parse("#/updates").page).toBe("updates");
  });

  it("accepts aliases and falls back to the dashboard", () => {
    expect(parse("#/pairing").page).toBe("clients");
    expect(parse("#/applications").page).toBe("library");
    expect(parse("#/history/").page).toBe("history");
    expect(parse("").page).toBe("dashboard");
    expect(parse("#/../../etc/passwd").page).toBe("dashboard");
    expect(parse("#/https://example.com").page).toBe("dashboard");
  });
});

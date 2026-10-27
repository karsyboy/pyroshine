import { describe, expect, it, vi } from "vitest";
import type { LibraryApplication, LibrarySource } from "../api/types";
import { configIndex, scannerIndex } from "./applications";

vi.mock("./daemon", () => ({ useDaemon: () => ({}) }));

const manual = (key: string, title: string) => ({ key, title }) as LibraryApplication;

describe("configIndex", () => {
  const items = [{ title: "Desktop" }, { title: "Steam" }];
  it("uses the entry's index when the title still matches", () => {
    expect(configIndex(manual("manual:1", "Steam"), items)).toBe(1);
  });
  it("finds a moved entry by its title", () => {
    expect(configIndex(manual("manual:0", "Steam"), items)).toBe(1);
  });
  it("reports an entry that is gone from the file", () => {
    expect(configIndex(manual("manual:2", "Heroic"), items)).toBeNull();
  });
});

describe("scannerIndex", () => {
  const source = (scanner: number, kind: string) => ({ scanner, kind }) as LibrarySource;
  const items = [{ type: "steam" }, { type: "desktop" }, { type: "desktop" }];
  it("uses the running scanner's index when its type still matches", () => {
    expect(scannerIndex(source(2, "desktop"), items)).toBe(2);
  });
  it("falls back to the only scanner of that type", () => {
    expect(scannerIndex(source(1, "steam"), items)).toBe(0);
  });
  it("refuses to guess between several scanners of a type", () => {
    expect(scannerIndex(source(0, "desktop"), items)).toBeNull();
  });
  it("reports a scanner that is gone from the file", () => {
    expect(scannerIndex(source(0, "heroic"), items)).toBeNull();
  });
});

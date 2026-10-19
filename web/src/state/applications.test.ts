import { describe, expect, it, vi } from "vitest";
import type { LibraryApplication } from "../api/types";
import { configIndex } from "./applications";

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

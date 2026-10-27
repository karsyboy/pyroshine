import { useCallback, useEffect, useState } from "react";

export type Page =
  | "dashboard"
  | "library"
  | "clients"
  | "history"
  | "settings"
  | "diagnostics"
  | "updates"
  | "maintenance";
const pages: Page[] = ["dashboard", "library", "clients", "history", "settings", "diagnostics", "updates", "maintenance"];

/// Other names a link may use for a page (the tray and older bookmarks).
const aliases: Record<string, Page> = {
  applications: "library",
  pairing: "clients",
  sessions: "history",
  session: "dashboard",
  performance: "dashboard",
  account: "maintenance",
};

export interface Route {
  page: Page;
  params: URLSearchParams;
}

export function parse(hash: string): Route {
  const [path, query = ""] = hash.replace(/^#\/?/, "").split("?");
  const name = path.replace(/\/+$/, "");
  const page = pages.includes(name as Page) ? (name as Page) : (aliases[name] ?? "dashboard");
  return { page, params: new URLSearchParams(query) };
}

/// Hash routing (`#/clients?request=…`): every page has an address the tray,
/// notifications and bookmarks can open directly, and the server only ever
/// serves one document.
export function useRoute(): [Route, (to: string) => void] {
  const [route, setRoute] = useState(() => parse(window.location.hash));
  const navigate = useCallback((to: string) => {
    window.location.hash = `/${to}`;
  }, []);
  useEffect(() => {
    const changed = () => setRoute(parse(window.location.hash));
    window.addEventListener("hashchange", changed);
    window.addEventListener("popstate", changed);
    return () => {
      window.removeEventListener("hashchange", changed);
      window.removeEventListener("popstate", changed);
    };
  }, []);
  return [route, navigate];
}

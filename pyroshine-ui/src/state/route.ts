import { useCallback, useEffect, useState } from "react";
import { on } from "../api/bridge";

export type Page = "dashboard" | "clients" | "settings" | "diagnostics";
const pages: Page[] = ["dashboard", "clients", "settings", "diagnostics"];

export interface Route {
  page: Page;
  params: URLSearchParams;
}

function parse(hash: string): Route {
  const [path, query = ""] = hash.replace(/^#\/?/, "").split("?");
  const page = pages.includes(path as Page) ? (path as Page) : "dashboard";
  return { page, params: new URLSearchParams(query) };
}

/// Hash routing (`#/clients?request=…`). The Rust side opens the window at a
/// hash and sends `ui://navigate` when an already open window should move.
export function useRoute(): [Route, (to: string) => void] {
  const [route, setRoute] = useState(() => parse(window.location.hash));
  const navigate = useCallback((to: string) => {
    window.location.hash = `/${to}`;
  }, []);
  useEffect(() => {
    const changed = () => setRoute(parse(window.location.hash));
    window.addEventListener("hashchange", changed);
    window.addEventListener("popstate", changed);
    const stop = on<string>("ui://navigate", navigate);
    return () => {
      window.removeEventListener("hashchange", changed);
      window.removeEventListener("popstate", changed);
      void stop.then((unlisten) => unlisten());
    };
  }, [navigate]);
  return [route, navigate];
}

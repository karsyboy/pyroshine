// The backend the UI talks to: the server's HTTP API. During development
// (`npm run dev`) the dev server proxies `/api` to a running Pyroshine (see
// vite.config.ts).

export { http as api, artworkUrl } from "./http";

/** Save `blob` as a download named `name`. */
export function download(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.rel = "noopener";
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

export { errorKind, errorMessage } from "./backend";

import { defineConfig, type Plugin } from "vitest/config";
import react from "@vitejs/plugin-react";
import { version } from "./package.json";

/// `pyroshine-web.json` names the Pyroshine version the files were built
/// for; the server compares it with its own and the page warns on a mismatch.
function buildInfo(): Plugin {
  return {
    name: "pyroshine-build-info",
    apply: "build",
    generateBundle() {
      this.emitFile({ type: "asset", fileName: "pyroshine-web.json", source: `${JSON.stringify({ version })}\n` });
    },
  };
}

// The server serves `dist/` (installed as share/pyroshine/web). `npm run dev`
// serves the UI with hot reload against a running Pyroshine whose API it
// proxies.
const environment = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env ?? {};
const target = environment.PYROSHINE_URL ?? "http://127.0.0.1:47990";

export default defineConfig({
  plugins: [react(), buildInfo()],
  clearScreen: false,
  define: { __APP_VERSION__: JSON.stringify(version) },
  server: {
    port: 1420,
    strictPort: true,
    proxy: {
      "/api": {
        target,
        changeOrigin: true,
        // The server accepts changes only from its own origin.
        headers: { origin: target },
      },
    },
  },
  build: {
    target: "es2022",
    outDir: "dist",
    chunkSizeWarningLimit: 1500,
  },
  test: { environment: "node" },
});

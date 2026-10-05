import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Tauri serves the built files; `npm run dev` serves them on a fixed port
// for `tauri dev` and for a browser preview with the mock backend.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    target: "es2022",
    outDir: "dist",
    chunkSizeWarningLimit: 1500,
  },
  test: { environment: "node" },
});

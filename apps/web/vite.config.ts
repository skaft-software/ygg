import react from "@vitejs/plugin-react";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

function disableXtermFallbackNavigation(): Plugin {
  return {
    name: "disable-xterm-fallback-navigation",
    enforce: "pre",
    transform(source, id) {
      const normalized = id.replaceAll("\\", "/");
      if (!normalized.includes("/node_modules/@xterm/")) return null;

      // xterm ships fallback handlers that call window.open even when the
      // application supplies explicit OSC and plain-link handlers. Remove
      // those unreachable fallbacks from the first-party production bundle;
      // TerminalPanel routes both link forms through an audited noopener anchor.
      const fallback = /\bwindow\.open\s*\(\s*\)/gu;
      const transformed = source.replace(fallback, "null");
      return transformed === source ? null : { code: transformed, map: null };
    },
  };
}

export default defineConfig({
  base: "/",
  plugins: [disableXtermFallbackNavigation(), react()],
  server: {
    host: "127.0.0.1",
    strictPort: true,
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    cssCodeSplit: false,
    minify: "oxc",
    // Keep the compact Local variable pair inside app.css so the extension
    // remains a deterministic fixed-name web bundle.
    assetsInlineLimit: 64_000,
    // The language catalog is intentionally bundled as one fixed-name asset;
    // keep its size warning threshold aligned with that accepted payload.
    chunkSizeWarningLimit: 1_600,
    rolldownOptions: {
      output: {
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/chunk-[name].js",
        manualChunks(id) {
          const normalized = id.replaceAll("\\", "/");
          return normalized.includes("/node_modules/@codemirror/") ||
            normalized.includes("/node_modules/@lezer/")
            ? "file-languages"
            : undefined;
        },
        assetFileNames: ({ names }) =>
          names.some((name) => name.endsWith(".css"))
            ? "assets/app.css"
            : "assets/[name][extname]",
      },
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: "./src/test-setup.ts",
    include: ["src/**/*.test.{ts,tsx}"],
    restoreMocks: true,
    coverage: {
      reporter: ["text", "json-summary"],
    },
  },
});

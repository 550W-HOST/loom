import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const appDir = fileURLToPath(new URL(".", import.meta.url));

export default defineConfig({
  root: appDir,
  base: "./",
  resolve: {
    conditions: ["source"],
    dedupe: ["react", "react-dom"],
  },
  build: {
    outDir: resolve(appDir, "dist"),
    emptyOutDir: true,
    sourcemap: true,
    reportCompressedSize: false,
    rollupOptions: {
      output: {
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
});

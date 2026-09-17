import path from "node:path";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  resolve: {
    conditions: ["source"],
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["src/test/setup.ts"],
    include: [
      "src/loom/**/*.test.ts",
      "src/loom/**/*.test.tsx",
      "src/hooks/queries/sidebar-navigation-query.test.tsx",
      "src/hooks/realtime-cache-effects.test.ts",
      "src/lib/ws.test.ts",
      "src/App.hash-navigation.test.tsx",
      "src/lib/route-paths.test.ts",
      "src/lib/split-layout/persistence.test.ts",
      "src/views/root-compose-initial-prompt.test.ts",
      "src/views/root-compose-thread-environment.test.ts",
      "src/views/thread-detail/splitThreadNavigation.test.ts",
      "src/components/promptbox/modifier-submit-shortcut.test.ts",
      "src/components/promptbox/mentions/*.test.ts",
      "src/components/promptbox/editor/*.test.ts",
      "src/components/thread/timeline/GeneratedConversationMessage.test.ts",
      "src/components/thread/timeline/streaming-markdown-split.test.ts",
      "src/components/thread/timeline/timeline-auto-expand.test.ts",
    ],
    exclude: ["dist/**", "node_modules/**"],
  },
});

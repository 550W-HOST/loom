import path from "node:path";
import { defineWorkspaceTestConfig } from "../../vitest.shared.js";

export default defineWorkspaceTestConfig({
  resolve: {
    alias: {
      "@bb/domain": path.resolve(import.meta.dirname, "../domain/src/index.ts"),
      "@bb/server-contract": path.resolve(
        import.meta.dirname,
        "../server-contract/src/index.ts",
      ),
    },
  },
  test: {
    silent: "passed-only",
    name: "@bb/sdk/browser-boundary",
    include: [
      "test/browser-boundary.test.ts",
      "test/realtime-subprotocol.test.ts",
    ],
    exclude: ["dist/**", "node_modules/**"],
  },
});

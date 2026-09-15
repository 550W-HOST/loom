import { defineWorkspaceTestConfig } from "../../vitest.shared.js";

export default defineWorkspaceTestConfig({
  test: {
    silent: "passed-only",
    name: "@bb/config/browser-build",
    include: ["test/app-surface.test.ts"],
    exclude: ["dist/**", "node_modules/**"],
  },
});

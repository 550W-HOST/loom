import { defineWorkspaceTestConfig } from "../../vitest.shared.js";

export default defineWorkspaceTestConfig({
  test: {
    silent: "passed-only",
    name: "@bb/host-daemon-contract/browser",
    include: ["test/browser-boundary.test.ts"],
    exclude: ["dist/**", "node_modules/**"],
  },
});

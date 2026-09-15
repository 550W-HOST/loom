import { defineWorkspaceTestConfig } from "../../vitest.shared.js";

export default defineWorkspaceTestConfig({
  test: {
    environment: "node",
    include: [
      "src/automation-provider-model-picker.test.tsx",
      "src/automation-safety-docs.test.ts",
      "src/client.test.ts",
      "src/format-schedule.test.ts",
      "src/frontend-imports.test.ts",
      "src/panel.test.tsx"
    ],
    exclude: ["node_modules/**", "dist/**"],
  },
});

import { describe, expect, it, vi } from "vitest";
import {
  AutomationsUnavailableError,
  createUnavailableAutomationsClient,
} from "./client.js";

const route = { projectId: "project", automationId: "automation" };

describe("loom-native Automations client boundary", () => {
  it("fails all ten operations closed with stable typed errors", async () => {
    const client = createUnavailableAutomationsClient();
    const calls = [
      ["automations_overview", client.call("automations_overview")],
      ["automations_list", client.call("automations_list", { projectId: "project" })],
      ["automations_get", client.call("automations_get", route)],
      [
        "automations_create",
        client.call("automations_create", {
          projectId: "project",
          name: "Daily check",
          trigger: {
            triggerType: "schedule",
            cron: "0 9 * * *",
            timezone: "UTC",
          },
          execution: {
            mode: "agent",
            prompt: "Check the build",
            providerId: "pi",
            model: "default",
            permissionMode: "accept-edits",
            environment: { type: "project-default" },
          },
          origin: "human",
        }),
      ],
      [
        "automations_update",
        client.call("automations_update", { ...route, name: "Updated" }),
      ],
      ["automations_delete", client.call("automations_delete", route)],
      ["automations_pause", client.call("automations_pause", route)],
      ["automations_resume", client.call("automations_resume", route)],
      ["automations_run", client.call("automations_run", route)],
      ["automations_runs", client.call("automations_runs", route)],
    ] as const;

    await Promise.all(
      calls.map(async ([operation, call]) => {
        await expect(call).rejects.toEqual(
          expect.objectContaining({
            name: "AutomationsUnavailableError",
            code: "automations_unavailable",
            operation,
          }) satisfies Partial<AutomationsUnavailableError>,
        );
      }),
    );
  });

  it("uses an inert, disposable realtime subscription", () => {
    const client = createUnavailableAutomationsClient();
    const listener = vi.fn();
    const dispose = client.subscribe(listener);

    dispose();
    expect(listener).not.toHaveBeenCalled();
  });
});

import { expect, type APIRequestContext } from "@playwright/test";
import { stackState } from "./stack.js";

/**
 * The API the suite uses to arrange state a user could not arrange quickly.
 *
 * A test that has to click through automation creation to reach the run
 * history it is actually about spends its budget on the wrong surface; the
 * clicks a user cares about are the ones the test then goes on to make.
 */
export async function personalProject(request: APIRequestContext): Promise<string> {
  const response = await request.get(`${stackState().baseURL}/api/v1/projects`);
  expect(response.ok()).toBeTruthy();
  const projects = (await response.json()) as { id: string }[];
  return projects[0]!.id;
}

export interface CreatedAutomation {
  id: string;
  name: string;
}

export async function createScriptAutomation(
  request: APIRequestContext,
  options: { name: string; script: string; project: string },
): Promise<CreatedAutomation> {
  const response = await request.post(
    `${stackState().baseURL}/api/v1/projects/${options.project}/automations`,
    {
      data: {
        name: options.name,
        enabled: true,
        trigger: { triggerType: "schedule", cron: "0 3 * * *", timezone: "UTC" },
        execution: {
          mode: "script",
          script: options.script,
          interpreter: "bash",
          timeoutMs: 10_000,
        },
        origin: "human",
      },
    },
  );
  expect(response.status()).toBe(201);
  const created = (await response.json()) as { id: string; name: string };
  return { id: created.id, name: created.name };
}

export async function runAutomation(
  request: APIRequestContext,
  options: { project: string; automation: string; key: string },
): Promise<void> {
  const response = await request.post(
    `${stackState().baseURL}/api/v1/projects/${options.project}/automations/${options.automation}/run`,
    { data: { idempotencyKey: options.key } },
  );
  expect([200, 201]).toContain(response.status());
}

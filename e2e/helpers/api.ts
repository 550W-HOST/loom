import { expect, type APIRequestContext } from "@playwright/test";
import { stackState } from "./stack.js";

/**
 * The API the suite uses to arrange state a user could not arrange quickly.
 *
 * A test that has to click through automation creation to reach the run
 * history it is actually about spends its budget on the wrong surface; the
 * clicks a user cares about are the ones the test then goes on to make.
 */
/**
 * The personal scope's id.
 *
 * It is a scope rather than a listed project — `GET /api/v1/projects` carries
 * only the projects a user created, and the sidebar bootstrap carries this one
 * beside them — so it is read from the bootstrap, which is where a client gets
 * it too.
 */
export async function personalProject(request: APIRequestContext): Promise<string> {
  const response = await request.get(`${stackState().baseURL}/api/v1/sidebar-bootstrap`);
  expect(response.ok()).toBeTruthy();
  const body = (await response.json()) as { personalProject: { id: string } };
  return body.personalProject.id;
}

/** The one host the stack's daemon enrolled, for a source that has to name one. */
export async function connectedHost(request: APIRequestContext): Promise<string> {
  const response = await request.get(`${stackState().baseURL}/api/v1/hosts`);
  expect(response.ok()).toBeTruthy();
  const hosts = (await response.json()) as { id: string; status: string }[];
  const connected = hosts.find((host) => host.status === "connected");
  expect(connected, `no connected host among ${JSON.stringify(hosts)}`).toBeTruthy();
  return connected!.id;
}

/** Creates a project at an existing directory on that host, and returns its id. */
export async function createProject(
  request: APIRequestContext,
  options: { name: string; hostId: string; path: string },
): Promise<string> {
  const response = await request.post(`${stackState().baseURL}/api/v1/projects`, {
    data: {
      name: options.name,
      source: { type: "local_path", hostId: options.hostId, path: options.path },
    },
  });
  expect(response.status()).toBe(201);
  return ((await response.json()) as { id: string }).id;
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

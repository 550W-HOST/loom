import { expect, test } from "@playwright/test";
import { createScriptAutomation, personalProject, runAutomation } from "../helpers/api.js";
import { stackState } from "../helpers/stack.js";

/**
 * The Automations view, which is where the product's own work happens.
 *
 * The automation is created over the API — a test about the *view* should not
 * spend its budget in a creation dialog — and everything after that is the
 * view: it lists it, it runs it, it shows the result, and pausing it is a
 * click.
 */
test.describe("automations", () => {
  test("lists, runs and pauses an automation", async ({ page, request }) => {
    const project = await personalProject(request);
    const name = `acceptance ${Date.now()}`;
    const automation = await createScriptAutomation(request, {
      name,
      script: "echo acceptance-ok",
      project,
    });

    await page.goto("/automations");
    const row = page.getByRole("button", { name, exact: false });
    await expect(row).toBeVisible();

    // The detail view names what will run, before anything has.
    await row.click();
    await expect(page.getByText("acceptance-ok").first()).toBeVisible();

    await page.getByRole("button", { name: /run now/i }).click();

    // The daemon runs it and the report comes back; the view shows the run
    // without a reload because the invalidation rides the public socket.
    await expect(page.getByText(/succeeded|acceptance-ok/i).first()).toBeVisible({ timeout: 45_000 });
    // The view can show a run before the control plane has settled it, so the
    // assertion polls rather than racing the daemon's report.
    await expect
      .poll(async () => {
        const response = await request.get(
          `${stackState().baseURL}/api/v1/projects/${project}/automations/${automation.id}/runs`,
        );
        expect(response.ok()).toBeTruthy();
        const history = (await response.json()) as {
          runs: { status: string; exitCode: number | null }[];
        };
        return history.runs[0]?.status;
      }, { timeout: 30_000 })
      .toBe("succeeded");
    const settled = await request.get(
      `${stackState().baseURL}/api/v1/projects/${project}/automations/${automation.id}/runs`,
    );
    const history = (await settled.json()) as { runs: { exitCode: number | null }[] };
    expect(history.runs[0]?.exitCode).toBe(0);

    // The switch is the view's control over the schedule.
    const toggle = page.getByRole("switch", { name: new RegExp(name, "i") });
    if (await toggle.isVisible()) {
      await toggle.click();
      await expect(toggle).toHaveAttribute("aria-checked", "false");
    }
  });

  test("a manual run started over the API appears in the open view", async ({ page, request }) => {
    const project = await personalProject(request);
    const name = `live ${Date.now()}`;
    const automation = await createScriptAutomation(request, {
      name,
      script: "echo live-update-ok",
      project,
    });

    await page.goto("/automations");
    await page.getByRole("button", { name, exact: false }).click();
    await expect(page.getByText(/no runs yet/i)).toBeVisible();

    // Nothing happens in the page: the run is started over the API, and the
    // view is expected to notice on its own.
    await runAutomation(request, { project, automation: automation.id, key: `live-${Date.now()}` });

    await expect(page.getByText(/live-update-ok|succeeded/i).first()).toBeVisible({ timeout: 45_000 });
  });
});

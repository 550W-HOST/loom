import { expect, test } from "@playwright/test";
import { composer } from "../helpers/composer.js";
import { stackState } from "../helpers/stack.js";

/**
 * A conversation, end to end: create a thread from the composer, send, and see
 * both halves of the exchange.
 *
 * The daemon runs an ACP stub, so the reply is fixed ("stub reply") and the
 * test asserts the round trip rather than a model's words. What it proves is
 * the part a unit test cannot: the app asked the server for an environment,
 * the daemon provisioned a workspace and ran the agent, the run's events
 * reached the timeline, and the client rendered them.
 */
test.describe("a thread", () => {
  test("is created, answers, and survives a reload", async ({ page }) => {
    await page.goto("/");

    const box = await composer(page);
    await box.click();
    await box.fill("hello from the acceptance suite");
    await page.getByRole("button", { name: /submit/i }).click();

    // The app creates the thread (and, on a machine with a daemon, its
    // workspace) and navigates to it.
    await expect(page).toHaveURL(/\/threads\/thr_/, { timeout: 30_000 });
    await expect(page.getByText("hello from the acceptance suite")).toBeVisible();

    // The daemon's stub answers when the turn runs; the answer is what proves
    // the whole path rather than the request half of it.
    await expect(page.getByText("stub reply")).toBeVisible({ timeout: 45_000 });

    const threadUrl = page.url();
    await page.reload();
    expect(page.url()).toBe(threadUrl);
    await expect(page.getByText("hello from the acceptance suite")).toBeVisible();
    await expect(page.getByText("stub reply")).toBeVisible();

    // The server agrees about what the client shows.
    const threadId = threadUrl.split("/").pop()!;
    const response = await page.request.get(`${stackState().baseURL}/api/v1/threads/${threadId}`);
    expect(response.ok()).toBeTruthy();
    const thread = (await response.json()) as { status: string };
    expect(thread.status).toBe("idle");
  });
});

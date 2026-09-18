import { expect, test } from "@playwright/test";
import { stackState } from "../helpers/stack.js";

/**
 * The shell: what a client sees before it has any data.
 *
 * This is the first thing the server has to get right, because every other
 * screen depends on it: the app derives its API origin from the page it was
 * served from, so a bootstrap that works means the same origin, the API and the
 * socket are all in place.
 */
test.describe("bootstrap", () => {
  test("the app renders the shell from the server's own origin", async ({ page }) => {
    const response = await page.goto("/");
    expect(response?.status()).toBe(200);

    // The composer is the one anchor every viewport has.
    await expect(page.getByRole("textbox", { name: /ask anything/i })).toBeVisible();

    // The sidebar is a drawer on a phone and a column on a desktop; on the
    // phone the shell has to be able to open it before anything in it counts.
    const drawer = page.getByRole("button", { name: "Toggle Sidebar" });
    if (await drawer.isVisible()) {
      await drawer.click();
    }
    await expect(page.getByRole("button", { name: "New thread", exact: true })).toBeVisible();
    await expect(page.getByRole("link", { name: "Automations" })).toBeVisible();

    // Served by the binary, same origin as the API it then talks to.
    expect(page.url().startsWith(stackState().baseURL)).toBeTruthy();
    const health = await page.request.get("/health");
    expect(health.ok()).toBeTruthy();
  });

  test("an unreachable server is reported, not rendered as an empty app", async ({ page }) => {
    // The client is served *by* the server, so "the server is gone" is a state
    // it reaches after loading: the health check it makes on mount is what has
    // to fail honestly.
    await page.route("**/health", (route) => route.abort());

    await page.goto("/");
    await expect(page.getByText(/cannot reach the loom server/i)).toBeVisible();
    await expect(page.getByRole("button", { name: /retry/i })).toBeVisible();
  });
});

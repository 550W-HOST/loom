import { readFileSync } from "node:fs";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { stackState } from "../helpers/stack.js";

const decisionPath = () => `${stackState().provider}.decision`;

/**
 * A blocked turn: the agent asks a question mid-turn and stops until someone
 * answers it in the browser.
 *
 * This is the flow that makes an agent safe to leave running — the question has
 * to survive a reload, and the answer has to reach the process that asked, not
 * merely clear a banner. The stub records the decision it received for exactly
 * that reason: what the agent was told is the contract, and a banner that
 * disappears without telling it anything is the bug this test exists to catch.
 */
/**
 * Opens the banner's question and its answers.
 *
 * The banner is compact until asked: the collapsed row is the signal, the
 * question and its decisions are behind one click. A reload collapses it
 * again, which is the state a returning user finds.
 */
async function expandBanner(banner: Locator): Promise<void> {
  if ((await banner.getAttribute("data-expanded")) === null) {
    await banner.getByRole("button", { name: "Approval needed" }).click();
  }
  await expect(banner).toHaveAttribute("data-expanded", "");
}

async function askForPermission(page: Page, prompt: string) {
  await page.goto("/");
  const composer = page.getByRole("textbox", { name: /ask anything/i });
  await composer.click();
  await composer.fill(prompt);
  await page.getByRole("button", { name: /submit/i }).click();

  await expect(page).toHaveURL(/\/threads\/thr_/, { timeout: 30_000 });
  const threadId = page.url().split("/").pop()!;
  // The collapsed banner is the product's signal that the turn is waiting on a
  // person; the question and its answers are one click behind it.
  const banner = page.getByTestId("approval-banner");
  // A banner that never appears is either a run that never asked or a client
  // that never rendered the question, and the two look identical from here.
  // The recorded interactions are what tells them apart, so a failure carries
  // them instead of only a screenshot.
  try {
    await expect(banner).toBeVisible({ timeout: 20_000 });
  } catch (error) {
    const base = stackState().baseURL;
    const describe = async (path: string) =>
      `${path}: ${(await (await page.request.get(`${base}${path}`)).text()).slice(0, 500)}`;
    console.log(
      [
        await describe(`/api/v1/threads/${threadId}/interactions`),
        await describe(`/api/v1/threads/${threadId}`),
        await describe(`/api/v1/hosts`),
      ].join("\n"),
    );
    throw error;
  }
  await expandBanner(banner);
  // The agent's own words for what it wants to run.
  await expect(page.getByRole("heading", { name: "Run rm -rf /" })).toBeVisible();
}

test.describe("a permission request", () => {
  test("blocks the turn until allowed, and the agent is told the decision", async ({ page }) => {
    await askForPermission(page, "please ask for permission");

    // The question outlives the page that first showed it: the interaction is
    // durable on the server, so a reload cannot lose it.
    const threadUrl = page.url();
    await page.reload();
    expect(page.url()).toBe(threadUrl);
    const reopened = page.getByTestId("approval-banner");
    await expect(reopened).toBeVisible();
    await expandBanner(reopened);

    await page.getByRole("button", { name: "Allow once" }).click();
    await expect(page.getByTestId("approval-banner")).toBeHidden();
    await expect(page.getByText("decision received")).toBeVisible({ timeout: 30_000 });

    expect(readFileSync(decisionPath(), "utf8")).toContain("allow-once");
  });

  test("a denial is the answer the agent receives", async ({ page }) => {
    await askForPermission(page, "ask for permission and deny it");

    await page.getByRole("button", { name: "Deny" }).click();
    await expect(page.getByText("decision received")).toBeVisible({ timeout: 30_000 });

    expect(readFileSync(decisionPath(), "utf8")).toContain("deny");
  });
});

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { connectedHost, createProject } from "../helpers/api.js";

/**
 * The shell as a user navigates it.
 *
 * The product app arrived here whole, so the question for these screens is not
 * whether they render — it is whether they render *loom*: sections that exist
 * open onto real data, and the surfaces loom does not have (plugins, plugin
 * marketplaces, browser control) are gone rather than left as entries that lead
 * nowhere. `docs/ui-baseline.md` holds the surface matrix this asserts.
 *
 * Two details of the shell shape how this is written. The settings sidebar is a
 * drawer on a phone, so its links are *rendered* but hidden until it opens —
 * "offered" is therefore read from the DOM rather than from the a11y tree, and
 * the destinations are opened by the href the sidebar itself carries rather than
 * by a path guessed from the label. And the sidebar repeats some page content
 * (the project list), so page assertions are scoped to `main`.
 */
const SECTIONS = [
  "General",
  "Providers",
  "Appearance",
  "Keyboard",
  "Usage limits",
  "Projects",
  "Machines",
  "Updates",
  "Experiments",
  "Community",
  "Archived threads",
] as const;

/** Sections loom decided against; they must not be reachable at all. */
const REMOVED = [
  "Installed plugins",
  "Plugin marketplaces",
  "Browser",
  "Skills",
  "Marketplace",
] as const;

/**
 * Opens the drawer if the viewport keeps the destinations in one.
 *
 * The trigger's `aria-expanded` is the signal: the closed drawer keeps its links
 * out of the tree entirely, and waiting for the attribute also outlasts the
 * slide-in.
 */
async function revealSidebar(page: import("@playwright/test").Page): Promise<void> {
  const drawer = page.getByRole("button", { name: "Toggle Sidebar" });
  if (!(await drawer.isVisible())) {
    return;
  }
  if ((await drawer.getAttribute("aria-expanded")) === "true") {
    return;
  }
  await drawer.click();
  await expect(drawer).toHaveAttribute("aria-expanded", "true");
}

interface SidebarDestination {
  href: string;
  label: string;
}

/** What the settings sidebar renders, by the label a user reads. */
async function sidebarDestinations(
  page: import("@playwright/test").Page,
): Promise<SidebarDestination[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('a[href^="/settings"]')).map((anchor) => ({
      href: (anchor as HTMLAnchorElement).getAttribute("href") ?? "",
      label: (anchor.textContent ?? "").trim(),
    })),
  );
}

function destinationFor(
  destinations: readonly SidebarDestination[],
  label: string,
): string | undefined {
  return destinations.find((destination) => destination.label === label)?.href;
}

test.describe("the settings shell", () => {
  test("offers every destination, and each one names itself", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByRole("heading", { name: "General" })).toBeVisible();
    await revealSidebar(page);

    const destinations = await sidebarDestinations(page);
    for (const section of SECTIONS) {
      expect(
        destinationFor(destinations, section),
        `${section} is offered as a destination`,
      ).toBeTruthy();
    }

    for (const section of SECTIONS) {
      await page.goto(destinationFor(destinations, section)!);
      await expect(
        page.getByRole("heading", { name: section }).first(),
        `${section} renders a page that names itself`,
      ).toBeVisible();
      await expect(page.getByText(/something went wrong|page not found/i)).toHaveCount(0);
    }
  });

  test("opens a destination from the sidebar, not just by URL", async ({ page }) => {
    // From a fresh load the drawer is closed, so this is the path a user takes:
    // open it if the viewport hides it, click the destination, land on the page.
    await page.goto("/settings");
    await revealSidebar(page);
    await page.getByRole("link", { name: "Providers", exact: true }).click();
    await expect(page).toHaveURL(/\/settings\/providers$/);
    await expect(page.getByRole("heading", { name: "Providers" })).toBeVisible();
  });

  test("does not offer the surfaces loom removed", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByRole("heading", { name: "General" })).toBeVisible();
    await revealSidebar(page);

    const destinations = await sidebarDestinations(page);
    for (const removed of REMOVED) {
      expect(
        destinationFor(destinations, removed),
        `${removed} must not be a destination`,
      ).toBeUndefined();
    }
  });

  test("lists the resources this machine actually has", async ({ page, request }) => {
    // The provider loom runs: a first-class ACP provider, not a plugin.
    await page.goto("/settings/providers");
    await expect(page.getByRole("main").getByText(/^pi$/i).first()).toBeVisible();

    // The daemon this stack enrolled, by the name it was started with.
    await page.goto("/settings/machines");
    await expect(
      page.getByRole("main").getByText("e2e", { exact: true }).first(),
    ).toBeVisible();

    // A project this machine has: created over the API at a directory that
    // exists, then listed by the settings page. The personal scope is not a
    // list entry — the sidebar bootstrap carries it beside the projects, and
    // `GET /api/v1/projects` carries only what a user created — so it is
    // deliberately not what this asserts.
    const name = `acceptance project ${Date.now()}`;
    const path = mkdtempSync(join(tmpdir(), "loom-e2e-project-"));
    await createProject(request, { name, hostId: await connectedHost(request), path });
    await page.goto("/settings/projects");
    await expect(page.getByRole("main").getByText(name).first()).toBeVisible();
  });
});

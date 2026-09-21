import { expect, type Locator, type Page } from "@playwright/test";

/**
 * The composer, wherever the app currently keeps it.
 *
 * A stack with no user-created projects opens on the welcome view, where the
 * composer is one click behind "New thread"; a stack that already has one opens
 * straight onto the composer. A test that only cares about what it types should
 * not have to care which of the two it got — and it must not guess from an
 * instant check, because the app decides after its project list has loaded.
 */
export async function composer(page: Page): Promise<Locator> {
  const box = page.getByRole("textbox", { name: /ask anything/i });
  const welcome = page.getByRole("button", { name: /start a new conversation/i });

  // Whichever of the two the app settles on is the shell being ready.
  await expect(box.or(welcome).first()).toBeVisible({ timeout: 20_000 });
  if (!(await box.isVisible())) {
    await welcome.click();
  }
  await expect(box).toBeVisible({ timeout: 15_000 });
  return box;
}

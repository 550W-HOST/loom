import { readFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import { ensureDaemon, stackState, startDaemon } from "../helpers/stack.js";

/**
 * A machine that goes away and comes back.
 *
 * The daemon is the execution plane: when it stops, the product has to say so
 * rather than look merely idle, and when it returns the machine has to be
 * usable again without a restart. Both halves are what an operator actually
 * experiences, and both are cheap to get wrong in a client that only listens
 * for events while the socket happens to be up.
 */
test.describe("a machine", () => {
  test("reports going offline and recovers when the daemon returns", async ({ page, request }) => {
    const state = stackState();
    const hosts = (await (await request.get(`${state.baseURL}/api/v1/hosts`)).json()) as {
      id: string;
      status: string;
    }[];
    const host = hosts.find((candidate) => candidate.status === "connected");
    expect(host).toBeTruthy();

    await page.goto(`/settings/machines/${host!.id}`);
    await expect(page.getByText(/^Online/u).first()).toBeVisible({ timeout: 30_000 });

    process.kill(Number(readFileSync(state.daemonPidPath, "utf8")), "SIGKILL");
    try {
      await expect(page.getByText(/^Offline/u).first()).toBeVisible({ timeout: 60_000 });

      // The machine comes back the way a deployment brings it back: same state
      // file, same identity, no new machine in the list.
      startDaemon(state);
      await expect(page.getByText(/^Online/u).first()).toBeVisible({ timeout: 60_000 });
    } finally {
      // A failed assertion here must not leave the rest of the suite without
      // the machine every later turn needs.
      ensureDaemon(state);
    }
  });
});

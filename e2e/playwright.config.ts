import { defineConfig, devices } from "@playwright/test";
import { stackBaseURL } from "./helpers/stack.js";

/**
 * Browser acceptance for the product UI.
 *
 * The suite drives the real thing: `cargo`'s binary serving the app it was
 * built with, and a daemon on the same machine running an ACP stub. Nothing is
 * proxied, mocked or stubbed at the HTTP layer — a failure here is a failure a
 * user would see, which is the point of having it at all. The stack is started
 * once per run by `helpers/stack.ts` through `globalSetup`.
 *
 * Two projects because the product is one client on a desktop and a phone: the
 * mobile viewport is where layout regressions live, and it is the one nobody
 * runs by hand.
 */
export default defineConfig({
  testDir: "./tests",
  globalSetup: "./helpers/global-setup.ts",
  globalTeardown: "./helpers/global-teardown.ts",
  // The stack is one server plus one daemon on fixed ports, so parallel files
  // would race over the same automations and the same thread.
  workers: 1,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI
    ? [["list"], ["html", { open: "never" }]]
    : [["list"], ["html", { open: "never" }]],
  outputDir: "test-results",
  use: {
    baseURL: stackBaseURL(),
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },
  projects: [
    {
      name: "desktop",
      use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 900 } },
    },
    {
      name: "mobile",
      use: { ...devices["Pixel 7"] },
    },
  ],
});

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it, vi } from "vitest";
import {
  BbHttpError,
  BrowserSdkUnavailableError,
  createBrowserBbSdk,
} from "../src/browser.js";

describe("browser SDK compile boundary", () => {
  it("rejects top-level and nested operations without network access", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>();
    const websocket = vi.fn();
    const sdk = createBrowserBbSdk({ fetch, websocket });

    await expect(sdk.threads.list()).rejects.toMatchObject({
      name: "BrowserSdkUnavailableError",
      code: "browser_sdk_unavailable",
      operation: "threads.list",
    } satisfies Partial<BrowserSdkUnavailableError>);
    await expect(
      sdk.projects.attachments.upload({ projectId: "project" }),
    ).rejects.toMatchObject({
      operation: "projects.attachments.upload",
    });
    await expect(sdk.subscribe({})).rejects.toMatchObject({
      operation: "subscribe",
    });

    expect(fetch).not.toHaveBeenCalled();
    expect(websocket).not.toHaveBeenCalled();
    expect(sdk.threads.list).toBe(sdk.threads.list);
  });

  it("retains the HTTP error API consumed by the app", () => {
    const error = new BbHttpError({
      body: { message: "conflict" },
      code: "conflict",
      message: "conflict",
      status: 409,
    });
    expect(error).toMatchObject({
      name: "BbHttpError",
      code: "conflict",
      status: 409,
    });
  });

  it("keeps public browser entries free of bb server transports", () => {
    const packageRoot = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "..",
    );
    const source = ["src/browser.ts", "src/response.ts"]
      .map((file) => fs.readFileSync(path.join(packageRoot, file), "utf8"))
      .join("\n");
    const packageJson = fs.readFileSync(
      path.join(packageRoot, "package.json"),
      "utf8",
    );

    expect(source).not.toMatch(
      /(?:from\s+["'](?:hono|ws|@bb\/(?:server-contract|host-daemon-contract|templates))|node:)/u,
    );
    expect(packageJson).not.toMatch(/"(?:hono|ws|@bb\/templates)"/u);
  });
});

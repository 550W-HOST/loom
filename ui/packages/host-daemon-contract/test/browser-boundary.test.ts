import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  HOST_DAEMON_PROTOCOL_VERSION,
  createHostDaemonLocalClient,
  HostDaemonLocalUnavailableError,
  providerUsageResponseSchema,
  workspaceOpenTargetIdSchema,
} from "../src/index.js";

describe("browser contract boundary", () => {
  it("retains the browser-visible schemas and protocol version", () => {
    expect(HOST_DAEMON_PROTOCOL_VERSION).toBe(199);
    expect(workspaceOpenTargetIdSchema.parse("vscode")).toBe("vscode");
    expect(
      providerUsageResponseSchema.parse({
        pi: {
          status: "ok",
          accountEmail: null,
          planLabel: null,
          windows: [],
        },
      }).pi.status,
    ).toBe("ok");
  });

  it("keeps the public browser entry files free of server transports", () => {
    const packageRoot = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "..",
    );
    const publicFiles = [
      "src/index.ts",
      "src/desktop-browser-import.ts",
      "src/local.ts",
      "src/protocol.ts",
      "src/provider-usage.ts",
      "src/workspace.ts",
    ];
    const source = publicFiles
      .map((file) => fs.readFileSync(path.join(packageRoot, file), "utf8"))
      .join("\n");
    expect(source).not.toMatch(
      /(?:from\s+["'](?:hono|@bb\/provider-bridge-protocol)|node:)/u,
    );
  });

  it("fails direct daemon access explicitly without issuing a request", async () => {
    const client = createHostDaemonLocalClient("http://127.0.0.1:38887/");
    await expect(client.status.$get()).rejects.toMatchObject({
      name: "HostDaemonLocalUnavailableError",
      code: "host_daemon_local_unavailable",
      baseUrl: "http://127.0.0.1:38887",
    } satisfies Partial<HostDaemonLocalUnavailableError>);
  });
});

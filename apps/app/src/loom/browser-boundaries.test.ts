import { describe, expect, it } from "vitest";
import { resolveUrlOpenTarget } from "@/lib/in-app-browser-link-preference";
import {
  normalizeExperimentalFileOpenOptions,
  toFilePreviewLineRange,
} from "@/lib/live-file-navigation";

describe("browser-safe product boundaries", () => {
  it("never selects the removed in-app browser", () => {
    expect(
      resolveUrlOpenTarget({
        desktopBrowserAvailable: true,
        openLinksInAppBrowser: true,
        url: "https://example.com",
      }),
    ).toBe("external-browser");
    expect(
      resolveUrlOpenTarget({
        desktopBrowserAvailable: true,
        openLinksInAppBrowser: true,
        url: "file:///tmp/example",
      }),
    ).toBe("unhandled");
  });

  it("accepts typed workspace locations and rejects malformed targets", () => {
    const normalized = normalizeExperimentalFileOpenOptions({
      target: {
        kind: "workspace",
        environmentId: "env-1",
        path: "src/main.ts",
      },
      location: { kind: "range", startLine: 4, endLine: 8 },
      viewer: "builtin",
    });

    expect(normalized).toEqual({
      target: {
        kind: "workspace",
        environmentId: "env-1",
        path: "src/main.ts",
      },
      location: { kind: "range", startLine: 4, endLine: 8 },
    });
    expect(toFilePreviewLineRange(normalized?.location ?? null)).toEqual({
      startLineNumber: 4,
      endLineNumber: 8,
    });
    expect(
      normalizeExperimentalFileOpenOptions({
        target: { kind: "workspace", environmentId: "", path: "" },
        location: null,
      }),
    ).toBeNull();
  });

  it("rejects traversal, path-kind confusion, controls, and oversized paths", () => {
    const invalidTargets = [
      {
        kind: "workspace",
        environmentId: "env-1",
        path: "src/main.ts",
        extra: true,
      },
      { kind: "workspace", environmentId: "env-1", path: "../secret" },
      { kind: "workspace", environmentId: "env-1", path: "/etc/passwd" },
      { kind: "workspace", environmentId: "env-1", path: "src\\main.ts" },
      { kind: "thread-storage", threadId: "thread-1", path: "a/./b" },
      { kind: "host", hostId: "host-1", path: "relative/file.ts" },
      { kind: "host", hostId: "host-1", path: "/tmp/../secret" },
      { kind: "host", hostId: "host-1", path: "/tmp/control\u0000" },
      {
        kind: "workspace",
        environmentId: "env-1",
        path: "a".repeat(32_769),
      },
    ];

    for (const target of invalidTargets) {
      expect(
        normalizeExperimentalFileOpenOptions({ target, location: null }),
      ).toBeNull();
    }

    expect(
      normalizeExperimentalFileOpenOptions({
        target: { kind: "host", hostId: "host-1", path: "/tmp/file.ts" },
        location: { kind: "line", line: 1, column: null, extra: true },
      }),
    ).toBeNull();
    expect(
      normalizeExperimentalFileOpenOptions({
        target: { kind: "host", hostId: "host-1", path: "/tmp/file.ts" },
        location: { kind: "line", line: 1, column: null },
      }),
    ).not.toBeNull();
  });
});

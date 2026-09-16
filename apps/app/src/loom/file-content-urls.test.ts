import { describe, expect, it } from "vitest";
import {
  buildEnvironmentDiffFileContentUrl,
  buildThreadHostFileContentUrl,
  buildThreadStorageContentUrl,
  buildThreadStorageRawContentUrl,
  buildThreadWorktreeRawContentUrl,
} from "@/lib/file-content-urls";

const THREAD = "thr_01M2MZETF4TP01WQ7Y0DCDEZGC";
const WORKTREE_PREFIX = `/api/v1/threads/${THREAD}/worktree/files/`;
const STORAGE_PREFIX = `/api/v1/threads/${THREAD}/thread-storage/files/`;

/**
 * The real product helpers, not the transport underneath them.
 *
 * The previous round tested `buildLoomApiUrl` directly and missed that
 * `file-content-urls.ts` pre-encoded its argument, so the transport encoded it
 * again: `%20` became `%2520` and a file whose name contained a space, Unicode,
 * or a literal `%` could not be opened from the UI. These assert the URL the
 * product actually puts in an `<img>`/link.
 */
describe("thread raw content URLs encode exactly once", () => {
  const worktreeCases: ReadonlyArray<[label: string, path: string, expected: string]> = [
    ["nested ASCII", "src/deep/nested/App.tsx", "src/deep/nested/App.tsx"],
    ["a space", "my file.ts", "my%20file.ts"],
    ["a nested space", "src/my file.ts", "src/my%20file.ts"],
    ["Unicode", "проект/文件.ts", "%D0%BF%D1%80%D0%BE%D0%B5%D0%BA%D1%82/%E6%96%87%E4%BB%B6.ts"],
    ["a literal percent", "100% done.ts", "100%25%20done.ts"],
    ["a percent that looks pre-encoded", "a%20b.ts", "a%2520b.ts"],
    ["a plus and ampersand", "a+b & c.ts", "a%2Bb%20%26%20c.ts"],
    ["parentheses kept readable", "file(1).ts", "file(1).ts"],
    ["a hash", "a#b.ts", "a%23b.ts"],
    ["a question mark", "a?b.ts", "a%3Fb.ts"],
  ];

  for (const [label, path, expected] of worktreeCases) {
    it(`builds the worktree URL for ${label}`, () => {
      expect(buildThreadWorktreeRawContentUrl(THREAD, path)).toBe(
        `${WORKTREE_PREFIX}${expected}`,
      );
    });

    it(`builds the thread-storage URL for ${label}`, () => {
      expect(buildThreadStorageRawContentUrl(THREAD, path)).toBe(
        `${STORAGE_PREFIX}${expected}`,
      );
    });
  }

  it("does not collide a space with a literal percent-escape", () => {
    // The exact double-encoding bug: these two used to render the same URL.
    const space = buildThreadWorktreeRawContentUrl(THREAD, "a b.ts");
    const literal = buildThreadWorktreeRawContentUrl(THREAD, "a%20b.ts");
    expect(space).not.toBe(literal);
    expect(space).toContain("a%20b.ts");
    expect(literal).toContain("a%2520b.ts");
  });

  it("encodes each nested segment independently", () => {
    expect(
      buildThreadWorktreeRawContentUrl(THREAD, "a b/c d/e f.ts"),
    ).toBe(`${WORKTREE_PREFIX}a%20b/c%20d/e%20f.ts`);
  });
});

describe("thread raw content URLs refuse an unaddressable path", () => {
  const refused: ReadonlyArray<[label: string, path: string]> = [
    ["an empty path", ""],
    ["an empty segment", "a//b"],
    ["a trailing empty segment", "a/"],
    ["a leading empty segment", "/a"],
    ["a dot segment", "a/./b"],
    ["a parent segment", "a/../b"],
    ["a bare parent segment", ".."],
    ["a bare dot segment", "."],
    ["a traversal", "../../../../etc/passwd"],
    ["a backslash", "a\\b.ts"],
    ["a control character", "a\u0000b.ts"],
    ["a newline", "a\nb.ts"],
  ];

  for (const [label, path] of refused) {
    it(`refuses ${label} instead of building a URL outside the route`, () => {
      expect(() => buildThreadWorktreeRawContentUrl(THREAD, path)).toThrow();
      expect(() => buildThreadStorageRawContentUrl(THREAD, path)).toThrow();
    });
  }

  it("never resolves a traversal to a different route", () => {
    // Without the rejection, `URL` would normalise `..` and this would address
    // the thread-storage *content* route instead of the file route.
    expect(() =>
      buildThreadWorktreeRawContentUrl(THREAD, "../thread-storage/content"),
    ).toThrow();
  });
});

describe("query-carried content URLs", () => {
  it("keeps a path with special characters intact in the query", () => {
    // These routes carry the path as a query value, so it is not path-encoded:
    // the search params encoder handles it, and it must round-trip untouched.
    const url = buildThreadStorageContentUrl(THREAD, "a b/c%20d/文件.ts");
    expect(url).toContain("path=a+b%2Fc%2520d%2F%E6%96%87%E4%BB%B6.ts");
    expect(new URL(url, "http://localhost").searchParams.get("path")).toBe(
      "a b/c%20d/文件.ts",
    );
  });

  it("keeps the host-file and environment-diff routes addressable", () => {
    expect(buildThreadHostFileContentUrl(THREAD, "/tmp/a b.ts")).toContain(
      "/threads/",
    );
    expect(
      buildEnvironmentDiffFileContentUrl("env_1", {
        target: "uncommitted",
        path: "src/a b.ts",
        side: "new",
      }),
    ).toContain("/environments/env_1/diff/file");
  });
});

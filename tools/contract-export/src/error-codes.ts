import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

export interface ErrorCodeEntry {
  code: string;
  /** HTTP status codes seen next to this code, ascending. */
  statuses: number[];
}

const API_ERROR_RE = /new\s+ApiError\(\s*(\d{3})\s*,\s*"([a-z0-9_]+)"/g;
const HTTP_EXCEPTION_RE =
  /new\s+HTTPException\(\s*(\d{3})\s*,\s*\{\s*message:[^}]*code:\s*"([a-z0-9_]+)"/g;

function walk(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      if (entry === "node_modules") continue;
      walk(path, out);
    } else if (entry.endsWith(".ts") && !entry.endsWith(".test.ts")) {
      out.push(path);
    }
  }
  return out;
}

/**
 * Best-effort inventory of the error codes the server can emit.
 *
 * The contract package types the error *body* but leaves `code` a free string;
 * the concrete codes live at the throw sites under `apps/server`. This scan is
 * read-only and records a code->status map so a Rust implementation can assert
 * it returns the same code for the same failure.
 */
export function collectErrorCodes(bbSrc: string): ErrorCodeEntry[] {
  const byCode = new Map<string, Set<number>>();
  const roots = [join(bbSrc, "apps", "server", "src")];
  for (const root of roots) {
    let files: string[];
    try {
      files = walk(root);
    } catch {
      continue;
    }
    for (const file of files) {
      const text = readFileSync(file, "utf8");
      for (const re of [API_ERROR_RE, HTTP_EXCEPTION_RE]) {
        re.lastIndex = 0;
        let match: RegExpExecArray | null;
        while ((match = re.exec(text))) {
          const status = Number(match[1]);
          const code = match[2]!;
          if (!byCode.has(code)) byCode.set(code, new Set());
          byCode.get(code)!.add(status);
        }
      }
    }
  }
  byCode.set("internal_error", byCode.get("internal_error") ?? new Set([500]));
  return [...byCode.entries()]
    .map(([code, statuses]) => ({
      code,
      statuses: [...statuses].sort((a, b) => a - b),
    }))
    .sort((a, b) => a.code.localeCompare(b.code));
}

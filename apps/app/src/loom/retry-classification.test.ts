import { describe, expect, it } from "vitest";
import {
  isRetryableHttpStatus,
  isAbortError,
  readHttpStatus,
} from "@/lib/http-retry-classification";
import { isTransientReadError, shouldRetryTransientReadQuery } from "@/hooks/queries/query-helpers";
import { LoomHttpError } from "@/lib/loom-http";

/**
 * The retry classification the query client actually uses.
 *
 * `query-helpers.ts` is where React Query's `retry` lands, so this is the
 * behaviour that decides whether a request is replayed — not a helper used only
 * by tests. An abort must not be retried, and a definitive HTTP status must not
 * be retried either (a 404 or a 422 is an answer).
 */

describe("retry classification", () => {
  it("does not retry an aborted request", () => {
    expect(isTransientReadError(new DOMException("aborted", "AbortError"))).toBe(
      false,
    );
    expect(isTransientReadError({ name: "AbortError" })).toBe(false);
    expect(isAbortError(new DOMException("x", "AbortError"))).toBe(true);
    // And through the helper React Query calls.
    expect(
      shouldRetryTransientReadQuery(0, new DOMException("x", "AbortError")),
    ).toBe(false);
  });

  it("does not retry a definitive HTTP failure", () => {
    for (const status of [400, 401, 403, 404, 409, 422]) {
      expect(
        isTransientReadError(new LoomHttpError({ status, message: "no" })),
        `status ${status} must not be retried`,
      ).toBe(false);
      expect(shouldRetryTransientReadQuery(0, new LoomHttpError({ status, message: "no" }))).toBe(
        false,
      );
    }
  });

  it("retries the statuses where another attempt can help", () => {
    for (const status of [408, 429, 500, 502, 503, 504]) {
      expect(isRetryableHttpStatus(status), `status ${status}`).toBe(true);
      expect(
        isTransientReadError(new LoomHttpError({ status, message: "later" })),
      ).toBe(true);
    }
  });

  it("still treats a transport failure as transient", () => {
    expect(isTransientReadError(new TypeError("Failed to fetch"))).toBe(true);
    expect(isTransientReadError(new TypeError("Load failed"))).toBe(true);
    expect(isTransientReadError(new Error("NetworkError"))).toBe(true);
  });

  it("does not treat an unrelated error as transient", () => {
    expect(isTransientReadError(new Error("Unexpected parse error"))).toBe(
      false,
    );
    expect(isTransientReadError(null)).toBe(false);
    expect(isTransientReadError(undefined)).toBe(false);
  });

  it("stops retrying after the configured attempt count", () => {
    const transportFailure = new TypeError("Failed to fetch");
    expect(shouldRetryTransientReadQuery(0, transportFailure)).toBe(true);
    expect(shouldRetryTransientReadQuery(1, transportFailure)).toBe(true);
    // The helper caps its own retries rather than retrying forever.
    expect(shouldRetryTransientReadQuery(2, transportFailure)).toBe(false);
  });

  it("reads a status from any client's error shape", () => {
    expect(readHttpStatus(new LoomHttpError({ status: 418, message: "x" }))).toBe(
      418,
    );
    expect(readHttpStatus({ status: 404 })).toBe(404);
    expect(readHttpStatus(new Error("no status"))).toBeNull();
    expect(readHttpStatus({ status: "404" })).toBeNull();
    expect(readHttpStatus(null)).toBeNull();
  });
});

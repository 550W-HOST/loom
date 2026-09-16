import { toRecord } from "@bb/core-ui";

/**
 * Status-aware retry classification, shared by every query policy.
 *
 * The ported app has three shapes of HTTP failure — the legacy `HttpError`, the
 * SDK's `BbHttpError`, and this issue's `LoomHttpError` — and they must not
 * disagree about what is worth retrying. A 404 or a 422 is a definitive answer:
 * replaying it wastes the user's time and can turn a validation error into a
 * slow one. A 408, a 429, or a 5xx is worth another attempt.
 */

/** True for the statuses where a retry can plausibly succeed. */
export function isRetryableHttpStatus(status: number): boolean {
  return status === 408 || status === 429 || status >= 500;
}

/**
 * The HTTP status carried by an error, whichever client raised it.
 *
 * All three error classes expose a numeric `status`; reading it structurally
 * keeps this module from importing any of them, so it cannot create a cycle
 * with the transport it classifies.
 */
export function readHttpStatus(error: unknown): number | null {
  const status = toRecord(error)?.status;
  return typeof status === "number" && Number.isInteger(status) ? status : null;
}

/** True when the error is an abort, which is a decision, not a failure. */
export function isAbortError(error: unknown): boolean {
  return toRecord(error)?.name === "AbortError";
}

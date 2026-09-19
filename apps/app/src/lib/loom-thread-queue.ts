import type { ThreadQueuedMessage } from "@bb/domain";
import type {
  CreateQueuedMessageRequest,
  ReorderQueuedMessageRequest,
  SendQueuedMessageRequest,
  SendQueuedMessageResponse,
  SetQueuedMessageGroupBoundaryRequest,
  ThreadQueuedMessageListResponse,
  UpdateQueuedMessageRequest,
} from "@bb/server-contract";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The queued-message operations the product app issues over loom.
 *
 * The whole `threads.queuedMessages` area was still the fail-closed browser SDK
 * stub, so queueing a follow-up while a turn runs, editing or reordering a
 * queued row, promoting it with send and dropping it again all threw
 * `BrowserSdkUnavailableError` instead of reaching the server. The contract
 * routes are nested under `/threads/:id/queued-messages` (with a couple of
 * distinct ids for the sub-verbs) and their server handlers already exist, so
 * this is the app half only.
 *
 * It follows the loom-native writer pattern (`loom-project-mutations.ts`,
 * `loom-thread-storage.ts`): the route table decides the verb and the body, and
 * the area is wired into `src/lib/sdk.ts`.
 */

export interface LoomListQueuedMessagesArgs {
  signal?: AbortSignal;
  threadId: string;
}

/** List a thread's live queued rows. */
export function loomListQueuedMessages(
  args: LoomListQueuedMessagesArgs,
): Promise<ThreadQueuedMessageListResponse> {
  return loomApiJson("threads.queuedMessages", {
    param: { id: args.threadId },
    signal: args.signal,
  });
}

export interface LoomCreateQueuedMessageArgs extends CreateQueuedMessageRequest {
  threadId: string;
}

/** Queue a follow-up; the contract answers `201` with the queued row. */
export function loomCreateQueuedMessage(
  args: LoomCreateQueuedMessageArgs,
): Promise<ThreadQueuedMessage> {
  const { threadId, ...json } = args;
  return loomApiJson("threads.createQueuedMessage", {
    param: { id: threadId },
    json,
  });
}

export interface LoomUpdateQueuedMessageArgs extends UpdateQueuedMessageRequest {
  queuedMessageId: string;
  threadId: string;
}

/** Edit a queued row, guarded by the `expectedUpdatedAt` the server checks. */
export function loomUpdateQueuedMessage(
  args: LoomUpdateQueuedMessageArgs,
): Promise<ThreadQueuedMessage> {
  const { queuedMessageId, threadId, ...json } = args;
  return loomApiJson("threads.updateQueuedMessage", {
    param: { id: threadId, queuedMessageId },
    json,
  });
}

export interface LoomDeleteQueuedMessageArgs {
  queuedMessageId: string;
  threadId: string;
}

export function loomDeleteQueuedMessage(
  args: LoomDeleteQueuedMessageArgs,
): Promise<{ ok: true }> {
  return loomApiJson("threads.deleteQueuedMessage", {
    param: { id: args.threadId, queuedMessageId: args.queuedMessageId },
  });
}

export interface LoomSendQueuedMessageArgs extends SendQueuedMessageRequest {
  queuedMessageId: string;
  threadId: string;
}

/** Promote a queued row; the server answers with the resulting send. */
export function loomSendQueuedMessage(
  args: LoomSendQueuedMessageArgs,
): Promise<SendQueuedMessageResponse> {
  const { queuedMessageId, threadId, ...json } = args;
  return loomApiJson("threads.sendQueuedMessage", {
    param: { id: threadId, queuedMessageId },
    json,
  });
}

export interface LoomReorderQueuedMessageArgs
  extends ReorderQueuedMessageRequest {
  queuedMessageId: string;
  threadId: string;
}

/**
 * Move a queued row between two neighbours.
 *
 * The server answers with the reordered list so the caller applies the server's
 * ordering; `groupBoundaryQueuedMessageId` stays absent rather than `undefined`
 * when a drag does not move the grouping boundary.
 */
export function loomReorderQueuedMessage(
  args: LoomReorderQueuedMessageArgs,
): Promise<ThreadQueuedMessageListResponse> {
  return loomApiJson("threads.reorderQueuedMessage", {
    param: { id: args.threadId, queuedMessageId: args.queuedMessageId },
    json: {
      nextQueuedMessageId: args.nextQueuedMessageId,
      previousQueuedMessageId: args.previousQueuedMessageId,
      ...(args.groupBoundaryQueuedMessageId === undefined
        ? {}
        : { groupBoundaryQueuedMessageId: args.groupBoundaryQueuedMessageId }),
    },
  });
}

export interface LoomSetQueuedMessageGroupBoundaryArgs
  extends SetQueuedMessageGroupBoundaryRequest {
  threadId: string;
}

/** Move the grouping boundary; the server answers with the reordered list. */
export function loomSetQueuedMessageGroupBoundary(
  args: LoomSetQueuedMessageGroupBoundaryArgs,
): Promise<ThreadQueuedMessageListResponse> {
  const { threadId, ...json } = args;
  return loomApiJson("threads.setQueuedMessageGroupBoundary", {
    param: { id: threadId },
    json,
  });
}

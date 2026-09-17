import type { PendingInteraction } from "@bb/domain";
import type {
  ResolvePendingInteractionRequest,
  ThreadPendingInteractionsResponse,
} from "@bb/server-contract";
import { loomApiJson } from "./loom-http";

interface LoomThreadInteractionTarget {
  interactionId: string;
  threadId: string;
}

export async function loomListThreadInteractions(args: {
  signal?: AbortSignal;
  threadId: string;
}): Promise<ThreadPendingInteractionsResponse> {
  return loomApiJson("threads.interactions", {
    param: { id: args.threadId },
    signal: args.signal,
  });
}

export async function loomGetThreadInteraction(
  args: LoomThreadInteractionTarget & { signal?: AbortSignal },
): Promise<PendingInteraction> {
  return loomApiJson("threads.interaction", {
    param: {
      id: args.threadId,
      interactionId: args.interactionId,
    },
    signal: args.signal,
  });
}

export async function loomResolveThreadInteraction(
  args: LoomThreadInteractionTarget & {
    resolution: ResolvePendingInteractionRequest;
  },
): Promise<PendingInteraction> {
  return loomApiJson("threads.resolveInteraction", {
    param: {
      id: args.threadId,
      interactionId: args.interactionId,
    },
    json: args.resolution,
  });
}

export async function loomCancelThreadInteraction(
  args: LoomThreadInteractionTarget,
): Promise<PendingInteraction> {
  return loomApiJson("threads.cancelInteraction", {
    param: {
      id: args.threadId,
      interactionId: args.interactionId,
    },
  });
}

export * from "./active-thinking.js";
export * from "./app-keybindings.js";
export * from "./app-settings.js";
export * from "./app-theme.js";
export * from "./change-kinds.js";
export * from "./code-theme.js";
export * from "./debounced-callback-scheduler.js";
export * from "./background-task.js";
export * from "./environment.js";
export * from "./experiments.js";
export * from "./feature-flags.js";
export * from "./git-checkout.js";
export * from "./host.js";
export * from "./host-list-limits.js";
export * from "./item-presentation.js";
export * from "./json-value.js";
export * from "./pending-interactions.js";
export * from "./plugin-catalog-category.js";
export * from "./plugin-id.js";
export * from "./project-path.js";
export * from "./project.js";
export * from "./prompt-history.js";
export * from "./protocol-ids.js";
export * from "./provider-event.js";
export * from "./provider-extension-kind.js";
export * from "./provider-model-catalog.js";
export * from "./provider-types.js";
export * from "./raw-thread-id.js";
export * from "./reasoning-level.js";
export * from "./queued-message.js";
export * from "./shared-types.js";
export * from "./system-message.js";
export * from "./terminal.js";
export * from "./thread-git-diff.js";
export * from "./thread-event-scope.js";
export * from "./thread-events.js";
export * from "./thread-origin-kind.js";
export * from "./thread-status.js";
export * from "./thread-search.js";
export * from "./thread-timeline-active-prompt-mode.js";
export * from "./thread-timeline-goal.js";
export * from "./thread-timeline-model-fallback.js";
export * from "./thread-timeline-pending-todos.js";
export * from "./thread-visibility.js";
export * from "./ui-preferences.js";
export {
  threadListEntrySchema,
  threadPullRequestSchema,
  threadQueuedMessageSchema,
  threadWithRuntimeSchema,
  workspaceStatusSchema,
} from "./thread.js";
export type {
  PullRequestState,
  ThreadActivityState,
  ThreadListEntry,
  ThreadPullRequest,
  ThreadPullRequestAttentionState,
  ThreadPullRequestChecksState,
  ThreadPullRequestMergeabilityState,
  ThreadPullRequestReviewState,
  ThreadQueuedMessage,
  ThreadQueuedWork,
  ThreadRuntimeDisplayStatus,
  ThreadRuntimeState,
  Thread,
  ThreadWithRuntime,
  WorkspaceChangeStats,
  WorkspaceCommitSummary,
  WorkspaceFileStatus,
  WorkspaceStatus,
} from "./thread.js";

import type { EnvironmentStatus } from "./environment.js";
import type { Host } from "./host.js";
import type { JsonValue } from "./json-value.js";
import type { ThreadEvent, ThreadEventType } from "./provider-event.js";
import type { ThreadEventScope } from "./thread-event-scope.js";

export interface ThreadEventRow {
  id: string;
  scope: ThreadEventScope;
  threadId: string;
  seq: number;
  type: ThreadEventType;
  data: Record<string, unknown>;
  createdAt: number;
}

export type Environment = {
  id: string;
  name: string | null;
  projectId: string;
  hostId: string;
  path: string | null;
  isGitRepo: boolean;
  isWorktree: boolean;
  branchName: string | null;
  baseBranch: string | null;
  defaultBranch: string | null;
  mergeBaseBranch: string | null;
  status: EnvironmentStatus;
  environmentProviderId: string | null;
  lifecycle: {
    phase: "active" | "retiring" | "teardown" | "destroyed";
    retireAt: number | null;
    teardown: {
      status: "running" | "failed" | "removed";
      attempt: number;
      message?: string;
    } | null;
  };
  environmentProviderSelection: {
    machine: { type: "existing"; hostId: string };
    inputs: JsonValue | null;
  } | null;
  environmentProviderInstanceKey: string | null;
  managed: boolean;
  workspaceProvisionType: "unmanaged" | "managed-worktree" | "personal" | null;
  createdAt: number;
  updatedAt: number;
};

export { type Host };

export function buildThreadEvent(row: ThreadEventRow): ThreadEvent {
  return {
    ...row.data,
    threadId: row.threadId,
    type: row.type,
    scope: row.scope,
  } as ThreadEvent;
}

export const LEGACY_CODEX_GOAL_EXTENSION_KIND = "provider-codex/goal";

export function toPositiveNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) && value > 0
    ? value
    : undefined;
}

export function isNamespacedGlyph(glyph: string): boolean {
  return /^[a-z0-9-]+\/[a-z0-9][a-z0-9-]*$/u.test(glyph);
}

export function isLegacyDelegationToolCall(call: {
  tool: string;
  presentation?: unknown;
}): boolean {
  return (
    call.presentation === undefined &&
    /(^|:)delegate(?:_to_agent)?$/u.test(call.tool)
  );
}

export function getTerminalBase64DecodedByteLength(value: string): number {
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
  return (value.length / 4) * 3 - padding;
}

export function readTerminalOutputLines(text: string): string[] {
  return text
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

export interface LegacyImageGenerationCompletion {
  callId: string;
  error: string | null;
  item: Record<string, unknown>;
  path: string | null;
  prompt: string | null;
  status: "pending" | "failed" | "interrupted" | "completed";
  transparentBackground: boolean;
}

export function parseLegacyImageGenerationCompletion(
  value: unknown,
): LegacyImageGenerationCompletion | null {
  if (!value || typeof value !== "object") return null;
  const payload = value as {
    rawType?: unknown;
    rawEvent?: {
      method?: unknown;
      params?: { item?: unknown };
    };
  };
  const item = payload.rawEvent?.params?.item;
  if (
    payload.rawType !== "item/completed" ||
    payload.rawEvent?.method !== "item/completed" ||
    !item ||
    typeof item !== "object" ||
    Array.isArray(item)
  ) {
    return null;
  }
  const image = item as Record<string, unknown>;
  if (image.type !== "imageGeneration" || typeof image.id !== "string") {
    return null;
  }
  const status =
    image.status === "inProgress"
      ? "pending"
      : image.status === "failed"
        ? "failed"
        : image.status === "declined"
          ? "interrupted"
          : image.status === "completed"
            ? "completed"
            : null;
  if (status === null) return null;
  return {
    callId: image.id,
    error: image.failure == null ? null : "Image generation failed",
    item: image,
    path: typeof image.savedPath === "string" ? image.savedPath : null,
    prompt:
      typeof image.revisedPrompt === "string" ? image.revisedPrompt : null,
    status,
    transparentBackground: image.transparentBackground === true,
  };
}

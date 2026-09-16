import type { ThreadListEntry } from "@bb/domain";

/**
 * A minimal `ThreadListEntry` for tests that only need list membership.
 *
 * The ported tests import `makeThreadListEntry` from `@bb/test-helpers`, a
 * package this workspace does not include, which left those files unrunnable
 * and therefore outside the CI gate. This covers the fields a sidebar list row
 * actually reads; a test that needs richer thread data should extend it rather
 * than re-deriving the schema.
 */
export function makeThreadListEntry(
  overrides: Partial<ThreadListEntry> = {},
): ThreadListEntry {
  return {
    id: "thr_test",
    projectId: "proj_test",
    environmentId: null,
    providerId: "pi",
    title: "Test thread",
    titleFallback: null,
    sectionId: null,
    status: "idle",
    parentThreadId: null,
    sourceThreadId: null,
    originKind: null,
    originPluginId: null,
    visibility: "visible",
    archivedAt: null,
    pinnedAt: null,
    deletedAt: null,
    lastReadAt: null,
    latestAttentionAt: 0,
    createdAt: 0,
    updatedAt: 0,
    runtime: {
      displayStatus: "idle",
      hostReconnectGraceExpiresAt: null,
    },
    pinSortKey: null,
    activity: {
      activeWorkflowCount: 0,
      activeBackgroundAgentCount: 0,
      activeBackgroundCommandCount: 0,
      activePlanModeCount: 0,
      activeGoalCount: 0,
    },
    hasPendingInteraction: false,
    environmentHostId: null,
    environmentName: null,
    environmentBranchName: null,
    environmentPath: null,
    environmentProviderId: null,
    environmentIsWorktree: null,
    environmentWorkspaceDisplayKind: "other",
    queuedWork: "none",
    ...overrides,
  };
}

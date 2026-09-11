import type {
  ThreadTimelineResponse,
  TimelineCommandWorkRow,
  TimelinePaginationCursor,
  TimelineRow,
  TimelineUserConversationRow,
} from "@bb/server-contract";
import { describe, expect, it } from "vitest";
import {
  mergeLoadedTimelineWithLatest,
  mergeLatestTimelineRows,
  prependOlderTimelineRows,
  type LoadedTimelineState,
} from "../src/timeline/timeline-merge.js";

function userRow(
  id: string,
  sequence: number,
  status: "accepted" | "pending" = "accepted",
): TimelineUserConversationRow {
  return {
    id,
    threadId: "thread-1",
    turnId: "turn-1",
    sourceSeqStart: sequence,
    sourceSeqEnd: sequence,
    startedAt: sequence,
    createdAt: sequence,
    kind: "conversation",
    role: "user",
    initiator: "user",
    senderThreadId: null,
    systemMessageKind: "unlabeled",
    systemMessageSubject: null,
    text: id,
    mentions: [],
    attachments: null,
    turnRequest: {
      isGrouped: false,
      kind: "message",
      status,
    },
  };
}

function commandRow(id: string, sequence: number): TimelineCommandWorkRow {
  return {
    id,
    threadId: "thread-1",
    turnId: "turn-1",
    sourceSeqStart: sequence,
    sourceSeqEnd: sequence,
    startedAt: sequence,
    createdAt: sequence,
    kind: "work",
    workKind: "command",
    status: "completed",
    callId: id,
    command: "pnpm test",
    cwd: null,
    source: null,
    output: "",
    exitCode: 0,
    completedAt: sequence,
    approvalStatus: null,
    activityIntents: [],
  };
}

function cursor(id: string, sequence: number): TimelinePaginationCursor {
  return { anchorId: id, anchorSeq: sequence };
}

function timelineResponse(
  rows: TimelineRow[],
  olderCursor: TimelinePaginationCursor | null,
): ThreadTimelineResponse {
  return {
    rows,
    contextBoundarySeq: null,
    activePromptMode: null,
    activeThinking: null,
    activeWorkflows: [],
    activeBackgroundCommands: [],
    pendingTodos: null,
    goal: null,
    modelFallback: null,
    maxSeq: Math.max(0, ...rows.map((row) => row.sourceSeqEnd)),
    timelinePage: {
      kind: "latest",
      segmentLimit: 20,
      returnedSegmentCount: rows.length === 0 ? 0 : 1,
      hasOlderRows: olderCursor !== null,
      olderCursor,
    },
  };
}

function loadedState(
  rows: TimelineRow[],
  olderCursor: TimelinePaginationCursor | null,
  latestWindowEndSequence: number,
): LoadedTimelineState {
  return {
    latestWindowEndSequence,
    rows,
    olderCursor,
    surfaceKey: "thread-1:default",
  };
}

describe("timeline page merging", () => {
  it("prepends older rows in server order and removes duplicate ids", () => {
    const oldUser = userRow("old-user", 1);
    const oldCommand = commandRow("old-command", 2);
    const latestUser = userRow("latest-user", 3);

    const merged = prependOlderTimelineRows({
      olderRows: [oldUser, oldCommand, oldUser],
      loadedRows: [latestUser],
    });

    expect(merged.map((row) => row.id)).toEqual([
      "old-user",
      "old-command",
      "latest-user",
    ]);
  });

  it("replaces the overlapping live tail while preserving unchanged history", () => {
    const history = userRow("history", 1);
    const liveTail = userRow("live-tail", 20);
    const updatedLiveTail = {
      ...liveTail,
      sourceSeqEnd: 21,
      text: "updated tail",
    };
    const newWork = commandRow("new-work", 22);

    const merged = mergeLatestTimelineRows({
      latestRows: [updatedLiveTail, newWork],
      latestWindowStartSequence: 20,
      loadedRows: [history, liveTail],
    });

    expect(merged.canMerge).toBe(true);
    expect(merged.rows.map((row) => row.id)).toEqual([
      "history",
      "live-tail",
      "new-work",
    ]);
    expect(merged.rows[0]).toBe(history);
    expect(merged.rows[1]).toBe(updatedLiveTail);
    expect(merged.rows[2]).toBe(newWork);
  });

  it("rebuilds the loaded state when the latest page has a sequence gap", () => {
    const oldCursor = cursor("old-page", 1);
    const latestCursor = cursor("latest-page", 40);
    const current = loadedState(
      [userRow("old-user", 1)],
      oldCursor,
      1,
    );
    const latest = timelineResponse([userRow("latest-user", 50)], latestCursor);

    const merged = mergeLoadedTimelineWithLatest({
      current,
      latestTimeline: latest,
      surfaceKey: "thread-1:default",
    });

    expect(merged.rows.map((row) => row.id)).toEqual(["latest-user"]);
    expect(merged.olderCursor).toEqual(latestCursor);
    expect(merged.latestWindowEndSequence).toBe(50);
  });

  it("replaces a pending optimistic message with the accepted server row", () => {
    const pending = userRow("message", 1, "pending");
    const accepted = userRow("message", 1, "accepted");

    const merged = mergeLatestTimelineRows({
      latestRows: [accepted],
      latestWindowStartSequence: 0,
      loadedRows: [pending],
    });

    expect(merged.rows).toEqual([accepted]);
    expect(merged.rows[0]).toBe(accepted);
  });
});

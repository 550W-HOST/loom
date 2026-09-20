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
  maxSeq?: number,
  historyRevision: number | null = 1,
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
    maxSeq: maxSeq ?? Math.max(0, ...rows.map((row) => row.sourceSeqEnd)),
    historyRevision,
    history: { status: "ready", complete: true, reason: null },
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
    historyRevision: 1,
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

  // A refetch asks with `afterSequence`, so the server answers with the rows
  // *after* that sequence and no older cursor: a suffix, not the whole window.
  // Reading the missing cursor as "the window starts at 0" dropped every loaded
  // row the page did not repeat, which is how a resolved approval emptied the
  // thread (W-534).
  it("keeps loaded rows the suffix page does not repeat", () => {
    const prompt = userRow("prompt", 1);
    const status = commandRow("status", 2);
    const current = loadedState([prompt, status], null, 5);
    const latest = timelineResponse([userRow("answer", 8)], null, 8);

    const merged = mergeLoadedTimelineWithLatest({
      current,
      latestTimeline: latest,
      surfaceKey: "thread-1:default",
    });

    expect(merged.rows.map((row) => row.id)).toEqual([
      "prompt",
      "status",
      "answer",
    ]);
  });

  it("keeps the loaded rows when a refetch brings nothing new", () => {
    const prompt = userRow("prompt", 1);
    const answer = userRow("answer", 8);
    const current = loadedState([prompt, answer], null, 11);
    const latest = timelineResponse([], null, 11);

    const merged = mergeLoadedTimelineWithLatest({
      current,
      latestTimeline: latest,
      surfaceKey: "thread-1:default",
    });

    expect(merged.rows.map((row) => row.id)).toEqual(["prompt", "answer"]);
    expect(merged.rows[0]).toBe(prompt);
  });

  // A response can arrive after the one that replaced it — a refetch racing the
  // page it superseded. Applying it would roll the timeline back to a numbering
  // the server has already left, so it is dropped.
  it("drops a response from an earlier revision of the same server", () => {
    const current = {
      ...loadedState([userRow("newer", 3)], null, 3),
      historyRevision: 2,
    };
    const late = timelineResponse([userRow("older", 1)], null, 1, 1);

    const merged = mergeLoadedTimelineWithLatest({
      current,
      latestTimeline: late,
      surfaceKey: "thread-1:default",
    });

    expect(merged).toBe(current);
    expect(merged.rows.map((row) => row.id)).toEqual(["newer"]);
  });

  // A restart is not a rollback, and no longer needs a second identity to say
  // so: the revision lives with the conversation, so a server that comes back
  // serves the same one and the client keeps merging. What it must not merge is
  // a response from another revision, which the cases above cover.

  // A response that carries no numbering at all (nothing to be a position in
  // yet) says nothing about what the client holds, so it must not blank it.
  it("keeps what it has when the server has nothing to be a position in yet", () => {
    const current = loadedState([userRow("prompt", 1)], null, 1);
    const loading = timelineResponse([], null, 0, null);

    const merged = mergeLoadedTimelineWithLatest({
      current,
      latestTimeline: loading,
      surfaceKey: "thread-1:default",
    });

    expect(merged).toBe(current);
  });

  it("replaces the rows when the server's revision changes", () => {
    const current = loadedState(
      [userRow("prompt", 1), userRow("answer", 8)],
      null,
      11,
    );
    // The rebuilt window looks contiguous with what we hold — its sequences
    // are higher — but it is a different numbering, so merging it would put a
    // row from the old numbering among rows it shares no numbers with.
    const rebuilt = timelineResponse([userRow("rebuilt", 5)], null, 20, 2);

    const merged = mergeLoadedTimelineWithLatest({
      current,
      latestTimeline: rebuilt,
      surfaceKey: "thread-1:default",
    });

    expect(merged.historyRevision).toBe(2);
    expect(merged.rows.map((row) => row.id)).toEqual(["rebuilt"]);
    expect(merged.latestWindowEndSequence).toBe(20);
  });
});

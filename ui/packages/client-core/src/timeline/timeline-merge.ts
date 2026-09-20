import type {
  ThreadTimelineResponse,
  TimelinePaginationCursor,
  TimelineRow,
} from "@bb/server-contract";
import { isOptimisticTimelineRowId } from "./optimistic-timeline-row.js";

type NullableTimelinePaginationCursor = TimelinePaginationCursor | null;

export interface LoadedTimelineState {
  /**
   * The server's numbering generation for the rows.
   *
   * Sequences are only comparable inside one generation: a rebuild renumbers
   * every row from one, so a cursor read as a position in the new numbering
   * would land in the wrong place, or past the end. Holding it here is what
   * lets a page from another generation be recognised as a restart rather than
   * stitched onto rows it shares no numbers with. It is `null` before any page
   * has arrived, when there is no numbering to belong to.
   */
  generation: number | null;
  historySnapshot?: string;
  latestWindowEndSequence: number | null;
  olderCursor: NullableTimelinePaginationCursor;
  rows: TimelineRow[];
  surfaceKey: string;
}

interface BuildLoadedTimelineStateArgs {
  generation: number | null;
  historySnapshot?: string;
  latestWindowEndSequence: number | null;
  latestRows: TimelineRow[];
  olderCursor: NullableTimelinePaginationCursor;
  surfaceKey: string;
}

interface AreTimelinePaginationCursorsEqualArgs {
  left: NullableTimelinePaginationCursor;
  right: NullableTimelinePaginationCursor;
}

interface MergeLatestTimelineRowsArgs {
  latestRows: readonly TimelineRow[];
  /**
   * The oldest sequence the latest page covers, or `null` when it carried no
   * rows and so says nothing about where a window would start.
   */
  latestWindowStartSequence: number | null;
  loadedRows: TimelineRow[];
}

interface MergeLatestTimelineRowsResult {
  canMerge: boolean;
  rows: TimelineRow[];
}

interface TimelineRowIdentityEntry {
  row: TimelineRow;
  signature: string;
}

interface PreserveTimelineRowIdentityArgs {
  nextRows: readonly TimelineRow[];
  previousRows: readonly TimelineRow[];
}

interface AreTimelineRowReferencesEqualArgs {
  left: readonly TimelineRow[];
  right: readonly TimelineRow[];
}

interface PrependOlderTimelineRowsArgs {
  loadedRows: readonly TimelineRow[];
  olderRows: readonly TimelineRow[];
}

interface MergeLoadedTimelineWithLatestArgs {
  current: LoadedTimelineState;
  latestTimeline: ThreadTimelineResponse;
  surfaceKey: string;
}

interface RecoverLoadedTimelineAfterStaleCursorArgs {
  current: LoadedTimelineState;
  latestTimeline: ThreadTimelineResponse;
  surfaceKey: string;
}

export function buildLoadedTimelineState({
  generation,
  historySnapshot,
  latestWindowEndSequence,
  latestRows,
  olderCursor,
  surfaceKey,
}: BuildLoadedTimelineStateArgs): LoadedTimelineState {
  return {
    generation,
    historySnapshot,
    latestWindowEndSequence,
    olderCursor,
    rows: latestRows,
    surfaceKey,
  };
}

export function areTimelinePaginationCursorsEqual({
  left,
  right,
}: AreTimelinePaginationCursorsEqualArgs): boolean {
  if (left === null || right === null) {
    return left === right;
  }
  return left.anchorSeq === right.anchorSeq && left.anchorId === right.anchorId;
}

function appendTimelineRowsPreservingOrder(
  target: TimelineRow[],
  rows: readonly TimelineRow[],
): void {
  const seenIds = new Set(target.map((row) => row.id));
  for (const row of rows) {
    if (seenIds.has(row.id)) {
      continue;
    }
    seenIds.add(row.id);
    target.push(row);
  }
}

function timelineRowIdentitySignature(row: TimelineRow): string {
  const turnRequest =
    row.kind === "conversation" && row.role === "user" ? row.turnRequest : null;
  return [
    row.kind,
    row.id,
    row.threadId,
    row.turnId ?? "<null>",
    row.sourceSeqStart,
    row.sourceSeqEnd,
    row.startedAt,
    row.createdAt,
    turnRequest?.isGrouped,
    turnRequest?.kind,
    turnRequest?.status,
  ].join("\u001f");
}

function buildTimelineRowIdentityMap(
  rows: readonly TimelineRow[],
): ReadonlyMap<string, TimelineRowIdentityEntry> {
  const rowsById = new Map<string, TimelineRowIdentityEntry>();
  for (const row of rows) {
    rowsById.set(row.id, {
      row,
      signature: timelineRowIdentitySignature(row),
    });
  }
  return rowsById;
}

function preserveTimelineRowIdentity({
  nextRows,
  previousRows,
}: PreserveTimelineRowIdentityArgs): TimelineRow[] {
  const previousRowsById = buildTimelineRowIdentityMap(previousRows);
  return nextRows.map((row) => {
    const previous = previousRowsById.get(row.id);
    if (
      previous &&
      previous.signature === timelineRowIdentitySignature(row) &&
      JSON.stringify(previous.row) === JSON.stringify(row)
    ) {
      return previous.row;
    }
    return row;
  });
}

function areTimelineRowReferencesEqual({
  left,
  right,
}: AreTimelineRowReferencesEqualArgs): boolean {
  if (left.length !== right.length) return false;
  return left.every((row, index) => row === right[index]);
}

export function prependOlderTimelineRows({
  loadedRows,
  olderRows,
}: PrependOlderTimelineRowsArgs): TimelineRow[] {
  const loadedById = new Map(loadedRows.map((row) => [row.id, row]));
  const uniqueOlderRows: TimelineRow[] = [];
  appendTimelineRowsPreservingOrder(uniqueOlderRows, olderRows);
  const rows: TimelineRow[] = uniqueOlderRows.map((row) => {
    const loaded = loadedById.get(row.id);
    if (
      row.kind === "turn" &&
      loaded?.kind === "turn" &&
      row.children !== null &&
      loaded.children !== null
    ) {
      return {
        ...loaded,
        children: prependOlderTimelineRows({
          olderRows: row.children,
          loadedRows: loaded.children,
        }),
      };
    }
    if (
      row.kind === "work" &&
      row.workKind === "delegation" &&
      loaded?.kind === "work" &&
      loaded.workKind === "delegation"
    ) {
      return {
        ...loaded,
        childRows: prependOlderTimelineRows({
          olderRows: row.childRows,
          loadedRows: loaded.childRows,
        }),
      };
    }
    return loaded ?? row;
  });
  appendTimelineRowsPreservingOrder(rows, loadedRows);
  return rows;
}

export function mergeLatestTimelineRows({
  latestRows,
  latestWindowStartSequence,
  loadedRows: retainedRows,
}: MergeLatestTimelineRowsArgs): MergeLatestTimelineRowsResult {
  const loadedRows = retainedRows.some((row) =>
    isOptimisticTimelineRowId(row.id),
  )
    ? retainedRows.filter((row) => !isOptimisticTimelineRowId(row.id))
    : retainedRows;

  const identityPreservedLatestRows = preserveTimelineRowIdentity({
    nextRows: latestRows,
    previousRows: loadedRows,
  });

  if (loadedRows.length === 0) {
    return {
      canMerge: true,
      rows: identityPreservedLatestRows,
    };
  }

  const latestRowsById = new Map(
    identityPreservedLatestRows.map((row) => [row.id, row]),
  );
  const rowsToRetain = loadedRows.filter(
    (row) =>
      latestWindowStartSequence === null ||
      row.sourceSeqEnd < latestWindowStartSequence ||
      latestRowsById.has(row.id),
  );
  const retainedRowIds = new Set(rowsToRetain.map((row) => row.id));
  const loadedCommonIds = rowsToRetain.flatMap((row) =>
    latestRowsById.has(row.id) ? [row.id] : [],
  );
  const latestCommonIds = identityPreservedLatestRows.flatMap((row) =>
    retainedRowIds.has(row.id) ? [row.id] : [],
  );
  if (
    loadedCommonIds.length !== latestCommonIds.length ||
    loadedCommonIds.some((id, index) => id !== latestCommonIds[index])
  ) {
    return { canMerge: false, rows: identityPreservedLatestRows };
  }

  const rowsBeforeSharedId = new Map<string, TimelineRow[]>();
  let pendingRows: TimelineRow[] = [];
  for (const row of identityPreservedLatestRows) {
    if (!retainedRowIds.has(row.id)) {
      pendingRows.push(row);
      continue;
    }
    if (pendingRows.length > 0) {
      rowsBeforeSharedId.set(row.id, pendingRows);
      pendingRows = [];
    }
  }

  const rows: TimelineRow[] = [];
  for (const row of rowsToRetain) {
    const rowsBefore = rowsBeforeSharedId.get(row.id);
    if (rowsBefore) {
      rows.push(...rowsBefore);
    }
    rows.push(latestRowsById.get(row.id) ?? row);
  }
  rows.push(...pendingRows);
  if (areTimelineRowReferencesEqual({ left: loadedRows, right: rows })) {
    return {
      canMerge: true,
      rows: loadedRows,
    };
  }

  return {
    canMerge: true,
    rows,
  };
}

/**
 * The oldest sequence a response's rows cover, or `null` when it carries none.
 *
 * It is the page's own first row, not its pagination cursor. A page fetched
 * with `afterSequence` is a *suffix* of the timeline and has no older cursor at
 * all, so reading the cursor said the window began at sequence 0 — and the
 * merge then dropped every loaded row the page did not repeat. A later page
 * that carried nothing new (which is what a refetch after the last event
 * returns) therefore emptied the thread: the loaded rows were all "before the
 * window start" and the window itself was empty.
 */
function timelineWindowStartSequence(
  timeline: ThreadTimelineResponse,
): number | null {
  return timeline.rows[0]?.sourceSeqStart ?? null;
}

function timelineWindowsAreContiguous(
  current: LoadedTimelineState,
  latestTimeline: ThreadTimelineResponse,
): boolean {
  if (
    current.latestWindowEndSequence === null ||
    latestTimeline.maxSeq < current.latestWindowEndSequence
  ) {
    return false;
  }
  // Only a page that *has* an older cursor claims to be a window with a known
  // start ("these are the newest rows above `anchorSeq`"): it touches what we
  // hold only when that start reaches back to it. A page without one is the
  // newest rows, and sequence numbers count events rather than rows — the
  // sequences between the two may simply produce no rows — so it continues the
  // timeline instead of opening a gap in it.
  const windowStartSequence = latestTimeline.timelinePage.olderCursor?.anchorSeq;
  return (
    windowStartSequence === undefined ||
    windowStartSequence <= current.latestWindowEndSequence + 1
  );
}

function mergeLoadedTimelineOlderCursor(
  current: NullableTimelinePaginationCursor,
  latest: NullableTimelinePaginationCursor,
): NullableTimelinePaginationCursor {
  if (current === null || latest === null) {
    return null;
  }
  return latest.anchorSeq <= current.anchorSeq ? latest : current;
}

/**
 * Whether this response belongs to a generation the client has already left.
 *
 * Generations only move forward, so an older one is a page the client asked for
 * before a rebuild and is only now receiving. Applying it would roll the
 * timeline back to a numbering that no longer describes the conversation, so a
 * late response is dropped instead of merged.
 */
function isFromAnEarlierGeneration(
  current: LoadedTimelineState,
  latestTimeline: ThreadTimelineResponse,
): boolean {
  return (
    current.generation !== null && latestTimeline.generation < current.generation
  );
}

export function mergeLoadedTimelineWithLatest({
  current,
  latestTimeline,
  surfaceKey,
}: MergeLoadedTimelineWithLatestArgs): LoadedTimelineState {
  if (isFromAnEarlierGeneration(current, latestTimeline)) {
    return current;
  }
  if (
    current.surfaceKey !== surfaceKey ||
    current.generation !== latestTimeline.generation ||
    current.historySnapshot !== latestTimeline.timelinePage.historySnapshot ||
    !timelineWindowsAreContiguous(current, latestTimeline)
  ) {
    return buildLoadedTimelineState({
      generation: latestTimeline.generation,
      historySnapshot: latestTimeline.timelinePage.historySnapshot,
      latestWindowEndSequence: latestTimeline.maxSeq,
      latestRows: latestTimeline.rows,
      olderCursor: latestTimeline.timelinePage.olderCursor,
      surfaceKey,
    });
  }

  const currentRowsById = new Map(current.rows.map((row) => [row.id, row]));
  const latestMerge = mergeLatestTimelineRows({
    latestRows:
      current.historySnapshot === undefined
        ? latestTimeline.rows
        : latestTimeline.rows.map((row) => {
            const loaded = currentRowsById.get(row.id);
            return loaded === undefined
              ? row
              : prependOlderTimelineRows({
                  olderRows: [loaded],
                  loadedRows: [row],
                })[0]!;
          }),
    latestWindowStartSequence: timelineWindowStartSequence(latestTimeline),
    loadedRows: current.rows,
  });
  if (!latestMerge.canMerge) {
    return buildLoadedTimelineState({
      generation: latestTimeline.generation,
      historySnapshot: latestTimeline.timelinePage.historySnapshot,
      latestWindowEndSequence: latestTimeline.maxSeq,
      latestRows: latestTimeline.rows,
      olderCursor: latestTimeline.timelinePage.olderCursor,
      surfaceKey,
    });
  }

  return {
    ...current,
    latestWindowEndSequence: latestTimeline.maxSeq,
    olderCursor: mergeLoadedTimelineOlderCursor(
      current.olderCursor,
      latestTimeline.timelinePage.olderCursor,
    ),
    rows: latestMerge.rows,
  };
}

export function recoverLoadedTimelineAfterStaleCursor({
  current,
  latestTimeline,
  surfaceKey,
}: RecoverLoadedTimelineAfterStaleCursorArgs): LoadedTimelineState {
  if (isFromAnEarlierGeneration(current, latestTimeline)) {
    return current;
  }
  if (
    current.surfaceKey !== surfaceKey ||
    current.generation !== latestTimeline.generation ||
    current.historySnapshot !== latestTimeline.timelinePage.historySnapshot
  ) {
    return buildLoadedTimelineState({
      generation: latestTimeline.generation,
      historySnapshot: latestTimeline.timelinePage.historySnapshot,
      latestWindowEndSequence: latestTimeline.maxSeq,
      latestRows: latestTimeline.rows,
      olderCursor: latestTimeline.timelinePage.olderCursor,
      surfaceKey,
    });
  }

  const latestMerge = mergeLatestTimelineRows({
    latestRows: latestTimeline.rows,
    latestWindowStartSequence: timelineWindowStartSequence(latestTimeline),
    loadedRows: current.rows,
  });
  if (!latestMerge.canMerge) {
    return buildLoadedTimelineState({
      generation: latestTimeline.generation,
      historySnapshot: latestTimeline.timelinePage.historySnapshot,
      latestWindowEndSequence: latestTimeline.maxSeq,
      latestRows: latestTimeline.rows,
      olderCursor: latestTimeline.timelinePage.olderCursor,
      surfaceKey,
    });
  }

  return {
    generation: latestTimeline.generation,
    historySnapshot: latestTimeline.timelinePage.historySnapshot,
    latestWindowEndSequence: latestTimeline.maxSeq,
    olderCursor: latestTimeline.timelinePage.olderCursor,
    rows: latestMerge.rows,
    surfaceKey,
  };
}

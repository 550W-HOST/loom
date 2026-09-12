import {
  encodeClientTurnRequestIdNumber,
  threadEventSchema,
  threadScope,
  turnScope,
} from "@bb/domain";
import type {
  ClientTurnRequestId,
  ThreadEvent,
  ThreadEventRow,
  ThreadEventScope,
  ThreadEventType,
} from "@bb/domain";
import {
  buildThreadTimelineFromEvents,
  buildTimelineViewRows,
  type ThreadTimelineViewRow,
} from "@bb/thread-view";
import type { RelayEventFrame } from "./types.js";
import { isRecord } from "./types.js";

interface PendingRequest {
  requestId: ClientTurnRequestId;
}

interface AdapterState {
  activeRunId: string | null;
  nextRequestNumber: number;
  pendingRequests: PendingRequest[];
  startedRuns: Set<string>;
}

export interface AdaptedThreadEvents {
  rows: ThreadEventRow[];
  status: string | null;
}

export interface ProjectedThreadTimeline {
  rows: ThreadTimelineViewRow[];
  status: string | null;
  eventCount: number;
}

function stringField(record: Record<string, unknown> | null, key: string): string | null {
  const value = record?.[key];
  return typeof value === "string" && value.length > 0 ? value : null;
}

function recordField(record: Record<string, unknown> | null, key: string): Record<string, unknown> | null {
  const value = record?.[key];
  return isRecord(value) ? value : null;
}

function parsePayload(payload: string): Record<string, unknown> | null {
  try {
    const value: unknown = JSON.parse(payload);
    return isRecord(value) ? value : null;
  } catch {
    return null;
  }
}

function threadIdFor(frame: RelayEventFrame, event: Record<string, unknown>): string | null {
  return stringField(event, "thread_id") ?? frame.scope.id ?? null;
}

function runTurnId(runId: string): string {
  return `loom-run-${runId}`;
}

function providerThreadId(threadId: string): string {
  return `loom-provider-${threadId}`;
}

function frameTime(frame: RelayEventFrame): number {
  return Number.isFinite(frame.created_at_ms) ? frame.created_at_ms : 0;
}

function normalizedToolArguments(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : value === undefined ? undefined : { value };
}

function addRow(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  threadId: string,
  sequence: { value: number },
  type: ThreadEventType,
  scope: ThreadEventScope,
  data: Record<string, unknown>,
  suffix: string,
): void {
  sequence.value += 1;
  rows.push({
    id: `${frame.event_id}:${suffix}`,
    scope,
    threadId,
    seq: sequence.value,
    type,
    data,
    createdAt: frameTime(frame),
  });
}

function addFallback(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  threadId: string,
  sequence: { value: number },
  rawType: string,
  detail: string,
): void {
  addRow(
    rows,
    frame,
    threadId,
    sequence,
    "system/operation",
    threadScope(),
    {
      operation: "loom_event",
      status: "completed",
      message: `Received unsupported loom event: ${rawType}`,
      operationId: frame.event_id,
      metadata: { rawType, detail },
    },
    "fallback",
  );
}

function addTurnStart(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  threadId: string,
  runId: string,
  sequence: { value: number },
  state: AdapterState,
): void {
  if (state.startedRuns.has(runId)) return;
  state.startedRuns.add(runId);
  const turnId = runTurnId(runId);
  addRow(
    rows,
    frame,
    threadId,
    sequence,
    "turn/started",
    turnScope(turnId),
    { providerThreadId: providerThreadId(threadId) },
    "turn-started",
  );
  const pending = state.pendingRequests.shift();
  if (pending) {
    addRow(
      rows,
      frame,
      threadId,
      sequence,
      "turn/input/accepted",
      turnScope(turnId),
      {
        providerThreadId: providerThreadId(threadId),
        clientRequestId: pending.requestId,
      },
      "input-accepted",
    );
  }
}

function addRunError(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  threadId: string,
  runId: string,
  sequence: { value: number },
  message: string,
): void {
  addRow(
    rows,
    frame,
    threadId,
    sequence,
    "provider/error",
    turnScope(runTurnId(runId)),
    {
      providerThreadId: providerThreadId(threadId),
      message,
    },
    "run-error",
  );
}

function addRunCompleted(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  threadId: string,
  runId: string,
  sequence: { value: number },
  status: "completed" | "failed" | "interrupted",
  error?: string,
): void {
  addRow(
    rows,
    frame,
    threadId,
    sequence,
    "turn/completed",
    turnScope(runTurnId(runId)),
    {
      providerThreadId: providerThreadId(threadId),
      status,
      ...(error ? { error: { message: error } } : {}),
    },
    "turn-completed",
  );
}

function adaptContractEvent(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  value: Record<string, unknown>,
  sequence: { value: number },
): boolean {
  const parsed = threadEventSchema.safeParse(value);
  if (!parsed.success) return false;

  const { type, threadId, scope, ...data } = parsed.data;
  addRow(
    rows,
    frame,
    threadId,
    sequence,
    type,
    scope,
    data as Record<string, unknown>,
    "contract-event",
  );
  return true;
}

const legacyRunEventTypes = new Set([
  "started",
  "output",
  "tool_call",
  "tool_result",
  "turn",
  "notice",
  "finished",
]);

function adaptMessage(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  event: Record<string, unknown>,
  threadId: string,
  sequence: { value: number },
  state: AdapterState,
): void {
  const message = recordField(event, "message");
  const content = stringField(message, "content");
  const role = stringField(message, "role");
  if (!message || !content || !role) {
    addFallback(rows, frame, threadId, sequence, "thread_message_added", frame.payload);
    return;
  }
  const messageId = stringField(message, "id") ?? frame.event_id;
  if (role === "user") {
    const requestId = encodeClientTurnRequestIdNumber({ value: state.nextRequestNumber });
    state.nextRequestNumber += 1;
    state.pendingRequests.push({ requestId });
    addRow(
      rows,
      frame,
      threadId,
      sequence,
      "client/turn/requested",
      threadScope(),
      {
        direction: "outbound",
        requestId,
        source: "tell",
        initiator: "user",
        senderThreadId: null,
        input: [{ type: "text", text: content, mentions: [] }],
        target: { kind: "new-turn" },
        request: { method: "turn/start", params: {} },
        execution: {
          model: "loom",
          serviceTier: "default",
          reasoningLevel: "none",
          permissionMode: "full",
          source: "client/turn/requested",
        },
      },
      "user-message",
    );
    return;
  }
  if (role === "assistant") {
    const scope = state.activeRunId ? turnScope(runTurnId(state.activeRunId)) : threadScope();
    addRow(
      rows,
      frame,
      threadId,
      sequence,
      "item/completed",
      scope,
      {
        providerThreadId: providerThreadId(threadId),
        item: { type: "agentMessage", id: messageId, text: content },
      },
      "assistant-message",
    );
    return;
  }
  if (role === "system") {
    addRow(
      rows,
      frame,
      threadId,
      sequence,
      "system/error",
      threadScope(),
      { message: content, code: "loom_system_message" },
      "system-message",
    );
    return;
  }
  addFallback(rows, frame, threadId, sequence, "thread_message_added", `role=${role}`);
}

function adaptRunEvent(
  rows: ThreadEventRow[],
  frame: RelayEventFrame,
  event: Record<string, unknown>,
  threadId: string,
  sequence: { value: number },
  state: AdapterState,
): void {
  const runId = stringField(event, "run_id");
  const run = recordField(event, "event");
  const runType = stringField(run, "type");
  if (!runId || !run || !runType) {
    addFallback(rows, frame, threadId, sequence, "thread_run_event", frame.payload);
    return;
  }

  if (adaptContractEvent(rows, frame, run, sequence)) return;
  if (!legacyRunEventTypes.has(runType)) {
    addFallback(rows, frame, threadId, sequence, runType, frame.payload);
    return;
  }

  const turnId = runTurnId(runId);
  const scope = turnScope(turnId);
  state.activeRunId = runId;
  if (runType !== "finished" || !state.startedRuns.has(runId)) {
    addTurnStart(rows, frame, threadId, runId, sequence, state);
  }

  switch (runType) {
    case "started":
      return;
    case "output": {
      const stream = stringField(run, "stream");
      const text = stringField(run, "text") ?? "";
      if (stream === "assistant") {
        addRow(rows, frame, threadId, sequence, "item/agentMessage/delta", scope, {
          providerThreadId: providerThreadId(threadId),
          itemId: `loom-assistant-${runId}`,
          delta: text,
        }, "assistant-delta");
      } else if (stream === "thinking") {
        addRow(rows, frame, threadId, sequence, "item/reasoning/textDelta", scope, {
          providerThreadId: providerThreadId(threadId),
          itemId: `loom-thinking-${runId}`,
          delta: text,
        }, "thinking-delta");
      } else if (stream === "log") {
        addRow(rows, frame, threadId, sequence, "provider/warning", scope, {
          providerThreadId: providerThreadId(threadId),
          category: "general",
          summary: "Provider log",
          details: text,
        }, "provider-log");
      } else {
        addFallback(rows, frame, threadId, sequence, "thread_run_event", `output stream=${stream ?? "unknown"}`);
      }
      return;
    }
    case "tool_call": {
      const toolCallId = stringField(run, "tool_call_id") ?? `${frame.event_id}:tool`;
      const name = stringField(run, "name") ?? "tool";
      const args = normalizedToolArguments(run.args);
      addRow(rows, frame, threadId, sequence, "item/started", scope, {
        providerThreadId: providerThreadId(threadId),
        item: {
          type: "toolCall",
          id: toolCallId,
          tool: name,
          ...(args ? { arguments: args } : {}),
          status: "pending",
        },
      }, "tool-started");
      return;
    }
    case "tool_result": {
      const toolCallId = stringField(run, "tool_call_id") ?? `${frame.event_id}:tool`;
      const name = stringField(run, "name") ?? "tool";
      const ok = run.ok !== false;
      const output = stringField(run, "output") ?? "";
      addRow(rows, frame, threadId, sequence, "item/completed", scope, {
        providerThreadId: providerThreadId(threadId),
        item: {
          type: "toolCall",
          id: toolCallId,
          tool: name,
          status: ok ? "completed" : "failed",
          ...(ok ? { result: output } : { error: output }),
        },
      }, "tool-completed");
      return;
    }
    case "turn": {
      const phase = stringField(run, "phase");
      if (phase === "ended") addRunCompleted(rows, frame, threadId, runId, sequence, "completed");
      return;
    }
    case "notice": {
      const level = stringField(run, "level");
      const message = stringField(run, "message") ?? "Provider notice";
      if (level === "error") {
        addRunError(rows, frame, threadId, runId, sequence, message);
      } else {
        addRow(rows, frame, threadId, sequence, "provider/warning", scope, {
          providerThreadId: providerThreadId(threadId),
          category: "general",
          summary: level === "warning" ? message : "Provider notice",
          ...(level === "warning" ? {} : { details: message }),
        }, "provider-notice");
      }
      return;
    }
    case "finished": {
      const outcome = stringField(run, "outcome") ?? "failed";
      const error = stringField(run, "error");
      if (outcome !== "completed" && error) addRunError(rows, frame, threadId, runId, sequence, error);
      const status = outcome === "completed" ? "completed" : outcome === "cancelled" ? "interrupted" : "failed";
      addRunCompleted(rows, frame, threadId, runId, sequence, status, error ?? undefined);
      state.activeRunId = null;
      return;
    }
    default:
      addFallback(rows, frame, threadId, sequence, "thread_run_event", `run type=${runType}`);
  }
}

export function adaptThreadFrames(frames: readonly RelayEventFrame[]): AdaptedThreadEvents {
  const rows: ThreadEventRow[] = [];
  const sequence = { value: 0 };
  const state: AdapterState = {
    activeRunId: null,
    nextRequestNumber: 0,
    pendingRequests: [],
    startedRuns: new Set(),
  };
  let status: string | null = null;

  for (const frame of frames) {
    const event = parsePayload(frame.payload);
    if (!event) {
      addFallback(rows, frame, frame.scope.id ?? "unknown", sequence, "raw", frame.payload);
      continue;
    }
    const rawType = stringField(event, "type") ?? "unknown";
    const threadId = threadIdFor(frame, event);
    if (!threadId) {
      addFallback(rows, frame, "unknown", sequence, rawType, frame.payload);
      continue;
    }
    switch (rawType) {
      case "thread_message_added":
        adaptMessage(rows, frame, event, threadId, sequence, state);
        break;
      case "thread_status_changed": {
        const nextStatus = stringField(event, "to");
        if (nextStatus) status = nextStatus;
        break;
      }
      case "thread_run_event":
        adaptRunEvent(rows, frame, event, threadId, sequence, state);
        break;
      case "thread_created":
        break;
      default:
        addFallback(rows, frame, threadId, sequence, rawType, frame.payload);
    }
  }
  return { rows, status };
}

function projectionStatus(status: string): "pending" | "idle" | "starting" | "active" | "stopping" | "error" {
  switch (status) {
    case "working":
      return "active";
    case "waiting":
      return "active";
    case "error":
      return "error";
    case "archived":
      return "idle";
    case "idle":
      return "idle";
    default:
      return "idle";
  }
}

function decodeRow(row: ThreadEventRow): { event: ThreadEvent; meta: { id: string; seq: number; createdAt: number } } {
  return {
    event: {
      ...row.data,
      threadId: row.threadId,
      type: row.type,
      scope: row.scope,
    } as ThreadEvent,
    meta: { id: row.id, seq: row.seq, createdAt: row.createdAt },
  };
}

export function projectThreadFrames(args: {
  frames: readonly RelayEventFrame[];
  threadName: string;
  threadStatus: string;
}): ProjectedThreadTimeline {
  const adapted = adaptThreadFrames(args.frames);
  const events = adapted.rows.map(decodeRow);
  const status = adapted.status ?? args.threadStatus;
  const projectionStatusValue = projectionStatus(status);
  const timeline = buildThreadTimelineFromEvents({
    acceptedClientRequestContext: {
      acceptedClientRequestEvents: [],
      rejectedClientRequestEvents: [],
    },
    contextWindowEvents: events,
    events,
    options: {
      includeDiagnosticOperations: true,
      includeNestedRows: true,
      isLatestPage: true,
      providerId: "loom",
      providerDisplayName: "loom",
      threadName: args.threadName,
      threadStatus: projectionStatusValue,
      turnMessageDetail: "full",
      workspaceRoot: null,
    },
  });
  return {
    rows: buildTimelineViewRows(timeline.rows, {
      closedScope: projectionStatusValue !== "active" && projectionStatusValue !== "starting",
    }),
    status,
    eventCount: args.frames.length,
  };
}

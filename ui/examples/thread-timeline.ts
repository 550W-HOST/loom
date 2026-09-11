import {
  encodeClientTurnRequestIdNumber,
  threadScope,
  turnScope,
} from "@bb/domain";
import type { ThreadEventRow } from "@bb/domain";
import {
  buildThreadTimelineFromEvents,
  formatThreadTimelineText,
} from "@bb/thread-view";
import { decodeThreadEventRow } from "../packages/thread-view/src/event-decode.js";
import { EMPTY_ACCEPTED_CLIENT_REQUEST_CONTEXT } from "../packages/thread-view/src/accepted-client-request-context.js";

const threadId = "thread-example";
const turnId = "turn-example";
const providerThreadId = "pi-thread-example";
const requestId = encodeClientTurnRequestIdNumber({ value: 1 });

function event(
  seq: number,
  type: ThreadEventRow["type"],
  data: Record<string, unknown>,
  scope = turnScope(turnId),
): ThreadEventRow {
  return {
    id: `example-${seq}`,
    scope,
    threadId,
    seq,
    type,
    data,
    createdAt: seq,
  };
}

const rows: ThreadEventRow[] = [
  event(1, "client/turn/requested", {
    direction: "outbound",
    requestId,
    source: "tell",
    initiator: "user",
    senderThreadId: null,
    input: [{ type: "text", text: "Inspect the relay", mentions: [] }],
    target: { kind: "new-turn" },
    request: { method: "turn/start", params: {} },
    execution: {
      model: "pi",
      serviceTier: "default",
      reasoningLevel: "medium",
      permissionMode: "full",
      source: "client/turn/requested",
    },
  }, threadScope()),
  event(2, "turn/started", { providerThreadId }),
  event(3, "item/agentMessage/delta", {
    providerThreadId,
    itemId: "assistant-1",
    delta: "The relay has ",
  }),
  event(4, "item/agentMessage/delta", {
    providerThreadId,
    itemId: "assistant-1",
    delta: "eight fixed shards.\n",
  }),
  event(5, "item/completed", {
    providerThreadId,
    item: {
      type: "agentMessage",
      id: "assistant-1",
      text: "The relay has eight fixed shards.\n",
    },
  }),
  event(6, "turn/completed", { providerThreadId, status: "completed" }),
];

const timeline = buildThreadTimelineFromEvents({
  acceptedClientRequestContext: EMPTY_ACCEPTED_CLIENT_REQUEST_CONTEXT,
  contextWindowEvents: [],
  events: rows.map(decodeThreadEventRow),
  options: {
    includeDiagnosticOperations: false,
    includeNestedRows: true,
    isLatestPage: true,
    threadName: "Relay example",
    threadStatus: "idle",
    turnMessageDetail: "full",
    workspaceRoot: null,
  },
});

console.log(
  formatThreadTimelineText(timeline.rows, {
    color: false,
    verbose: true,
  }),
);
console.log("\nRows:\n" + JSON.stringify(timeline.rows, null, 2));

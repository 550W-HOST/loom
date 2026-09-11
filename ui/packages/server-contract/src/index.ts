import { z } from "zod";
import type {
  ExtensionKind,
  JsonObject,
  JsonValue,
  PendingInteractionUserAnswer,
  PendingInteractionUserQuestionQuestion,
  PermissionMode,
  PromptInput,
  PromptTextMention,
  ReasoningLevel,
  ServiceTier,
  ThreadEventItemPresentation,
  ThreadEventPlanStep,
  ThreadEventSearchMode,
  ThreadTurnInitiator,
} from "@bb/domain";

export type TimelineRowStatus =
  | "pending"
  | "completed"
  | "error"
  | "interrupted";

export type TimelineApprovalStatus = "waiting_for_approval" | "denied" | null;

export type TimelineActivityIntent =
  | {
      type: "read";
      command: string;
      name: string;
      path: string | null;
    }
  | {
      type: "list_files";
      command: string;
      path: string | null;
    }
  | {
      type: "search";
      command: string;
      query: string | null;
      path: string | null;
    }
  | { type: "unknown"; command: string };

export interface TimelineRowBase {
  id: string;
  threadId: string;
  turnId: string | null;
  sourceSeqStart: number;
  sourceSeqEnd: number;
  startedAt: number;
  createdAt: number;
}

export interface TimelineConversationAttachments {
  webImages: number;
  localImages: number;
  localFiles: number;
  imageUrls: string[];
  localImagePaths: string[];
  localFilePaths: string[];
}

export interface TimelineConversationTurnRequest {
  isGrouped: boolean;
  kind: "message" | "steer";
  status: "pending" | "accepted" | "rejected";
}

interface TimelineConversationRowBase extends TimelineRowBase {
  kind: "conversation";
  text: string;
  attachments: TimelineConversationAttachments | null;
}

export interface TimelineUserConversationRow
  extends TimelineConversationRowBase {
  role: "user";
  initiator: ThreadTurnInitiator;
  senderThreadId: string | null;
  systemMessageKind: string;
  systemMessageSubject: JsonValue | null;
  turnRequest: TimelineConversationTurnRequest;
  mentions: PromptTextMention[];
}

export interface TimelineAssistantConversationRow
  extends TimelineConversationRowBase {
  role: "assistant";
  turnRequest: null;
}

export type TimelineConversationRow =
  | TimelineUserConversationRow
  | TimelineAssistantConversationRow;

export type TimelineSystemOperationKind =
  | "generic"
  | "reasoning"
  | "compaction"
  | "context-clear"
  | "parent-change"
  | "thread-provisioning"
  | "thread-interrupted"
  | "provider-unhandled"
  | "warning"
  | "deprecation";

export interface TimelineParentChange {
  action: "assign" | "release" | "transfer";
  previousParentThreadId: string | null;
  previousParentThreadTitle: string | null;
  nextParentThreadId: string | null;
  nextParentThreadTitle: string | null;
}

interface TimelineSystemRowBase extends TimelineRowBase {
  kind: "system";
  title: string;
  detail: string | null;
  status: TimelineRowStatus | null;
}

type TimelineNonOperationSystemRow = TimelineSystemRowBase & {
  systemKind: "debug" | "error" | "reconnect";
};

type TimelineGenericOperationSystemRow = TimelineSystemRowBase & {
  systemKind: "operation";
  operationKind: Exclude<TimelineSystemOperationKind, "parent-change">;
  reasoningId?: string;
  completedAt: number | null;
};

export interface TimelineParentChangeSystemRow
  extends TimelineSystemRowBase {
  systemKind: "operation";
  operationKind: "parent-change";
  status: TimelineRowStatus;
  parentChange: TimelineParentChange;
  completedAt: number | null;
}

export type TimelineSystemRow =
  | TimelineNonOperationSystemRow
  | TimelineGenericOperationSystemRow
  | TimelineParentChangeSystemRow;

export interface TimelineFileChange {
  path: string;
  kind: string | null;
  movePath: string | null;
  diff: string | null;
  diffStats: { added: number; removed: number };
}

interface TimelineWorkRowBase extends TimelineRowBase {
  kind: "work";
  status: TimelineRowStatus;
}

export type TimelineRowPresentation = ThreadEventItemPresentation;

export interface TimelineCommandWorkRow extends TimelineWorkRowBase {
  workKind: "command";
  callId: string;
  command: string;
  cwd: string | null;
  source: string | null;
  output: string;
  outputPreview?: {
    experimental_fullOutputAvailability:
      | "available"
      | "detail-limit"
      | "retention-expired";
    totalChars: number;
  };
  exitCode: number | null;
  completedAt: number | null;
  approvalStatus: TimelineApprovalStatus;
  activityIntents: TimelineActivityIntent[];
  presentation?: TimelineRowPresentation;
}

export interface TimelineToolWorkRow extends TimelineWorkRowBase {
  workKind: "tool";
  callId: string;
  toolName: string;
  toolArgs: JsonObject | null;
  output: string;
  outputPreview?: {
    experimental_fullOutputAvailability:
      | "available"
      | "detail-limit"
      | "retention-expired";
    totalChars: number;
  };
  completedAt: number | null;
  approvalStatus: TimelineApprovalStatus;
  presentation?: TimelineRowPresentation;
}

export interface TimelineFileChangeWorkRow extends TimelineWorkRowBase {
  workKind: "file-change";
  callId: string;
  change: TimelineFileChange;
  stdout: string | null;
  stderr: string | null;
  approvalStatus: TimelineApprovalStatus;
  presentation?: TimelineRowPresentation;
}

export interface TimelineWebSearchWorkRow extends TimelineWorkRowBase {
  workKind: "web-search";
  callId: string;
  queries: string[];
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelineWebFetchWorkRow extends TimelineWorkRowBase {
  workKind: "web-fetch";
  callId: string;
  url: string;
  prompt: string | null;
  pattern: string | null;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelineImageViewWorkRow extends TimelineWorkRowBase {
  workKind: "image-view";
  callId: string;
  path: string;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelineImageGenerationWorkRow extends TimelineWorkRowBase {
  workKind: "image-generation";
  callId: string;
  prompt: string | null;
  path: string | null;
  error: string | null;
  transparentBackground: boolean;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelineFileReadWorkRow extends TimelineWorkRowBase {
  workKind: "file-read";
  callId: string;
  path: string;
  cmd: string | null;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelineSearchWorkRow extends TimelineWorkRowBase {
  workKind: "search";
  callId: string;
  mode: ThreadEventSearchMode;
  query: string;
  path: string | null;
  cmd: string | null;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelinePlanStepsWorkRow extends TimelineWorkRowBase {
  workKind: "plan-steps";
  callId: string;
  steps: ThreadEventPlanStep[];
  explanation: string | null;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export interface TimelineExtensionWorkRow extends TimelineWorkRowBase {
  /** Legacy wire row retained for decoding old server snapshots. */
  workKind: "extension";
  callId: string;
  extensionKind: ExtensionKind;
  payload: JsonValue;
  completedAt: number | null;
  presentation: TimelineRowPresentation;
}

interface TimelineApprovalWorkRowBase extends TimelineWorkRowBase {
  workKind: "approval";
  interactionId: string;
  target: { itemId: string; toolName: string | null };
}

export type TimelineApprovalWorkRow =
  | (TimelineApprovalWorkRowBase & {
      approvalKind: "file-edit";
      lifecycle: "waiting" | "denied";
    })
  | (TimelineApprovalWorkRowBase & {
      approvalKind: "permission-grant";
      lifecycle: "pending" | "resolving" | "granted" | "denied" | "interrupted";
      grantScope: "turn" | "session" | null;
      statusReason: string | null;
    });

export interface TimelineQuestionWorkRow extends TimelineWorkRowBase {
  workKind: "question";
  interactionId: string;
  lifecycle: "pending" | "resolving" | "answered" | "interrupted";
  questions: PendingInteractionUserQuestionQuestion[];
  answers: Record<string, PendingInteractionUserAnswer> | null;
  statusReason: string | null;
}

export interface TimelineDelegationWorkRow extends TimelineWorkRowBase {
  workKind: "delegation";
  callId: string;
  toolName: string;
  childRef: string | null;
  background: boolean;
  subagentType: string | null;
  description: string | null;
  output: string;
  completedAt: number | null;
  childRows: TimelineRow[];
  presentation?: TimelineRowPresentation;
}

export interface TimelineWorkflowWorkRow extends TimelineWorkRowBase {
  workKind: "workflow";
  itemId: string;
  taskType: string;
  workflowName: string | null;
  description: string;
  model: string | null;
  taskStatus:
    | "pending"
    | "running"
    | "paused"
    | "completed"
    | "failed"
    | "killed"
    | "stopped";
  workflow: {
    phases: Array<{ index: number; title: string; kind?: string }>;
    agents: Array<{
      index: number;
      label: string;
      state: "queued" | "running" | "done" | "failed" | "skipped";
      model: string;
      attempt: number;
      cached: boolean;
      lastProgressAt: number;
      phaseIndex?: number;
      phaseTitle?: string;
      agentType?: string;
      isolation?: string;
      queuedAt?: number;
      startedAt?: number;
      lastToolName?: string;
      lastToolSummary?: string;
      promptPreview?: string;
      resultPreview?: string;
      error?: string;
      tokens?: number;
      toolCalls?: number;
      durationMs?: number;
    }>;
  } | null;
  usage: { totalTokens: number; toolUses: number; durationMs: number } | null;
  summary: string | null;
  error: string | null;
  completedAt: number | null;
  presentation?: TimelineRowPresentation;
}

export type TimelineWorkRow =
  | TimelineCommandWorkRow
  | TimelineToolWorkRow
  | TimelineFileChangeWorkRow
  | TimelineWebSearchWorkRow
  | TimelineWebFetchWorkRow
  | TimelineImageGenerationWorkRow
  | TimelineImageViewWorkRow
  | TimelineFileReadWorkRow
  | TimelineSearchWorkRow
  | TimelinePlanStepsWorkRow
  | TimelineApprovalWorkRow
  | TimelineQuestionWorkRow
  | TimelineDelegationWorkRow
  | TimelineWorkflowWorkRow;

/**
 * Rows that may appear on the historical wire format. New renderers consume
 * TimelineWorkRow so legacy extension rows cannot leak into the view model.
 */
export type TimelineWireWorkRow = TimelineWorkRow | TimelineExtensionWorkRow;

export interface TimelineTurnRow extends TimelineRowBase {
  kind: "turn";
  turnId: string;
  status: TimelineRowStatus;
  summaryCount: number;
  completedAt: number | null;
  children: TimelineRow[] | null;
}

export type TimelineSourceRow =
  | TimelineConversationRow
  | TimelineWorkRow
  | TimelineSystemRow;

export type TimelineRow = TimelineSourceRow | TimelineTurnRow;

export type TimelineToolArgs = JsonObject | null;

export interface ThreadContextWindowUsage {
  usedTokens: number | null;
  modelContextWindow: number | null;
  estimated: boolean;
}

export interface TimelinePaginationCursor {
  anchorSeq: number;
  anchorId: string;
}

export interface TimelinePageMetadata {
  kind: "latest" | "older";
  segmentLimit: number;
  returnedSegmentCount: number;
  hasOlderRows: boolean;
  olderCursor: TimelinePaginationCursor | null;
  historySnapshot?: string;
  contentPage?: {
    anchorSeq: number;
    start: number;
    end: number;
    total: number;
  };
}

export interface ThreadTimelineResponse {
  rows: TimelineRow[];
  contextBoundarySeq: number | null;
  activePromptMode: unknown;
  activeThinking: unknown;
  activeWorkflows: TimelineWorkflowWorkRow[];
  activeBackgroundCommands: TimelineWorkflowWorkRow[];
  pendingTodos: unknown;
  goal: unknown;
  modelFallback: unknown;
  contextWindowUsage?: ThreadContextWindowUsage;
  timelinePage: TimelinePageMetadata;
  maxSeq: number;
  delta?: unknown;
}

export interface CreateThreadRequest {
  projectId?: string;
  environmentId?: string | null;
  providerId?: string;
  input?: PromptInput[];
  model?: string;
  reasoningLevel?: ReasoningLevel;
  permissionMode?: PermissionMode;
  serviceTier?: ServiceTier;
  executionInputSources?: ExistingThreadExecutionInputSources;
  origin?: string;
  startedOnBehalfOf?: {
    initiator: "agent" | "system";
    threadId?: string;
  } | null;
  originKind?: string;
  [key: string]: unknown;
}

export interface SendMessageRequest {
  input: PromptInput[];
  mode?: "queue-if-active" | "steer-if-active" | "immediate";
  model?: string;
  reasoningLevel?: ReasoningLevel;
  permissionMode?: PermissionMode;
  serviceTier?: ServiceTier;
  executionInputSources?: ExistingThreadExecutionInputSources;
  [key: string]: unknown;
}

export interface CreateQueuedMessageRequest {
  input: PromptInput[];
  model?: string;
  reasoningLevel?: ReasoningLevel;
  permissionMode?: PermissionMode;
  serviceTier?: ServiceTier;
  executionInputSources?: ExistingThreadExecutionInputSources;
  [key: string]: unknown;
}

export type ExistingThreadExecutionInputSources = Record<string, string>;

export interface ProviderCommand {
  name: string;
  source: ProviderCommandSource;
  origin: ProviderCommandOrigin;
  description: string | null;
  argumentHint: string | null;
  pluginId?: string;
}

export type ProviderCommandOrigin = "builtin" | "project" | "user";
export type ProviderCommandSource = "skill" | "command";
export type ProviderCommandSection =
  | "agent-command"
  | "skill"
  | "project-command"
  | "user-command";

export const terminalCreateTargetSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("thread"), threadId: z.string() }),
  z.object({ kind: z.literal("environment"), environmentId: z.string() }),
  z.object({
    kind: z.literal("host_path"),
    hostId: z.string(),
    cwd: z.string().nullable(),
  }),
]);
export type TerminalCreateTarget = z.infer<typeof terminalCreateTargetSchema>;

const terminalSessionSchema = z.object({}).passthrough();
const terminalOutputChunkSchema = z.object({
  seq: z.number().int().nonnegative(),
  dataBase64: z.string(),
});

export const terminalServerMessageSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("attached"),
    session: terminalSessionSchema,
    replayStartSeq: z.number().int().nonnegative(),
    nextSeq: z.number().int().nonnegative(),
  }),
  z.object({
    type: z.literal("output"),
    chunk: terminalOutputChunkSchema,
  }),
  z.object({
    type: z.literal("session-updated"),
    session: terminalSessionSchema,
  }),
  z.object({
    type: z.literal("exited"),
    session: terminalSessionSchema,
  }),
  z.object({
    type: z.literal("error"),
    code: z.string(),
    message: z.string(),
  }),
  z.object({ type: z.literal("pong") }),
]);
export type TerminalServerMessage = z.infer<typeof terminalServerMessageSchema>;

const filePreviewLineRangeSchema = z.object({
  endLineNumber: z.number().int().positive(),
  startLineNumber: z.number().int().positive(),
});
const environmentFilePreviewSourceSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("working-tree") }),
  z.object({ kind: z.literal("head") }),
  z.object({ kind: z.literal("merge-base"), ref: z.string() }),
]);
const previewTabSchema = z.object({
  lineRange: filePreviewLineRangeSchema.nullable(),
  path: z.string(),
});

export const threadTabFileOpenerOwnerSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("workspace-file-preview"),
    environmentId: z.string().nullable(),
    projectId: z.string().nullable(),
    threadId: z.string().nullable(),
    tab: previewTabSchema.extend({
      source: environmentFilePreviewSourceSchema,
      statusLabel: z.literal("deleted").nullable(),
    }),
  }),
  z.object({
    kind: z.literal("host-file-preview"),
    environmentId: z.string().nullable(),
    hostId: z.string().nullable(),
    threadId: z.string().nullable(),
    tab: previewTabSchema,
  }),
  z.object({
    kind: z.literal("thread-storage-file-preview"),
    environmentId: z.string().nullable(),
    threadId: z.string(),
    tab: previewTabSchema,
  }),
]);
export type ThreadTabFileOpenerOwner = z.infer<
  typeof threadTabFileOpenerOwnerSchema
>;

export interface UploadedPromptAttachment {
  type: "localImage" | "localFile";
  path: string;
  name?: string;
  sizeBytes?: number;
  mimeType?: string;
}

export const uploadedPromptAttachmentSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("localImage"),
    path: z.string(),
    name: z.string().optional(),
    sizeBytes: z.number().optional(),
    mimeType: z.string().optional(),
  }),
  z.object({
    type: z.literal("localFile"),
    path: z.string(),
    name: z.string().optional(),
    sizeBytes: z.number().optional(),
    mimeType: z.string().optional(),
  }),
]);

const PROVIDER_COMMAND_SECTIONS: readonly ProviderCommandSection[] = [
  "agent-command",
  "skill",
  "project-command",
  "user-command",
];

export function providerCommandSection(command: {
  source: ProviderCommandSource;
  origin: ProviderCommandOrigin;
}): ProviderCommandSection {
  if (command.origin === "builtin") return "agent-command";
  if (command.source === "skill") return "skill";
  return command.origin === "project" ? "project-command" : "user-command";
}

export function providerCommandSectionRank(command: {
  source: ProviderCommandSource;
  origin: ProviderCommandOrigin;
}): number {
  return PROVIDER_COMMAND_SECTIONS.indexOf(providerCommandSection(command));
}

import type {
  CloseTerminalRequest,
  CreateHostJoinCodeResponse,
  CreateProjectRequest,
  CreateProjectSourceRequest,
  CreateQueuedMessageRequest,
  CreateTerminalRequest,
  CreateThreadRequest,
  CreateThreadSectionRequest,
  DeleteThreadRequest,
  DeleteThreadSectionRequest,
  EnvironmentDiffFileQuery,
  HostDirectoryListing,
  HostDirectoryQuery,
  ProjectAttachmentContentQuery,
  ProjectBranchesQuery,
  ProjectDefaultExecutionOptionsQuery,
  ProjectFileContentQuery,
  ProjectResponse,
  ReorderProjectRequest,
  ReorderQueuedMessageRequest,
  SendMessageRequest,
  SendMessageResponse,
  SendQueuedMessageRequest,
  SendQueuedMessageResponse,
  SetQueuedMessageGroupBoundaryRequest,
  SidebarBootstrapResponse,
  SystemConfigResponse,
  SystemEnvironmentProvidersQuery,
  SystemEnvironmentProvidersResponse,
  SystemExecutionOptionsQuery,
  SystemExecutionOptionsResponse,
  SystemProviderStatesResponse,
  SystemProvidersQuery,
  SystemProviderInfo,
  SystemVersionQuery,
  SystemVersionResponse,
  TerminalListQuery,
  TerminalListResponse,
  TerminalSession,
  ThreadChildSummaryResponse,
  ThreadFilesRawQuery,
  ThreadGetQuery,
  ThreadHostFileContentQuery,
  ReorderPinnedThreadRequest,
  ResolvePendingInteractionRequest,
  ThreadResponse,
  ThreadPendingInteractionsResponse,
  ThreadQueuedMessageListResponse,
  ThreadSectionMutationResponse,
  ThreadSectionResponse,
  ThreadStorageContentQuery,
  ThreadStorageFileListResponse,
  ThreadStorageFilesQuery,
  ThreadStorageLocationResponse,
  ThreadStoragePathListResponse,
  ThreadStoragePathsQuery,
  ThreadTabsResponse,
  ThreadTimelineQuery,
  ThreadTimelineResponse,
  UiPreferenceResponse,
  UiPreferencesResponse,
  UpdateProjectRequest,
  UpdateProjectSourceRequest,
  UpdateQueuedMessageRequest,
  UpdateTerminalRequest,
  UpdateThreadSectionRequest,
  UpdateThreadTabsRequest,
  UpdateUiPreferenceRequest,
} from "@bb/server-contract";
import type {
  AppSettings,
  AppSettingsUpdate,
  Environment,
  Host,
  PendingInteraction,
  ProjectExecutionDefaults,
  ProjectSource,
  ResolvedThreadExecutionOptions,
  ThreadListEntry,
  ThreadQueuedMessage,
} from "@bb/domain";

/**
 * The request shape of every allowlisted route, derived from the exported
 * contract.
 *
 * A generic `json?: unknown` / `formData?: FormData` / loose query record is not
 * enough: TypeScript would still accept a JSON body and a `FormData` on the same
 * GET, and the browser rejects `GET` with a body outright. Binding each route to
 * its contract request — and to *only* the body kind it declares — makes the
 * wrong request unrepresentable rather than merely wrong at runtime.
 *
 * `source` mirrors the contract's own request `source` field (`contracts/bb/
 * server-api.json`), so a route whose contract says `query` cannot be given a
 * body here, and a route whose contract says `json` cannot be given a query.
 */

export type LoomApiRequestSource = "none" | "query" | "json" | "form";

export interface LoomApiRequestSpec {
  readonly source: LoomApiRequestSource;
  /** Query key shape (only when `source` is `query`). */
  readonly query?: unknown;
  /** JSON body shape (only when `source` is `json`). */
  readonly json?: unknown;
  /** Form body shape (only when `source` is `form`). */
  readonly form?: unknown;
}

/** A path parameter bag, keyed by the bare contract name. */
export type LoomApiParamBag = Readonly<Record<string, string>>;

/**
 * The request each allowlisted route accepts.
 *
 * Exported as a `const` object rather than inferred from the route table so the
 * request shapes are reviewable in one place, and so
 * `src/loom/api-client.test.ts` can assert them against the contract's own
 * `request.source`.
 */
export const LOOM_API_REQUEST_SPECS = {
  "filePreviews.content": { source: "none" },
  "environments.diffFile": {
    source: "query",
    query: {} as EnvironmentDiffFileQuery,
  },
  "environments.get": { source: "none" },
  "hosts.createJoinCode": { source: "json", json: {} as Record<string, never> },
  "hosts.delete": { source: "none" },
  "hosts.directory": {
    source: "query",
    query: {} as HostDirectoryQuery,
  },
  "hosts.list": { source: "none" },
  "hosts.updatePermissionCeiling": {
    source: "json",
    json: {} as { maxPermissionMode: "accept-edits" | "auto" | "full" },
  },
  "projects.attachmentContent": {
    source: "query",
    query: {} as ProjectAttachmentContentQuery,
  },
  "projects.branchOptions": {
    source: "query",
    query: {} as ProjectBranchesQuery,
  },
  "projects.create": { source: "json", json: {} as CreateProjectRequest },
  "projects.createSource": {
    source: "json",
    json: {} as CreateProjectSourceRequest,
  },
  "projects.defaultExecutionOptions": {
    source: "query",
    query: {} as ProjectDefaultExecutionOptionsQuery,
  },
  "projects.delete": { source: "none" },
  "projects.deleteSource": { source: "none" },
  "projects.fileContent": {
    source: "query",
    query: {} as ProjectFileContentQuery,
  },
  "projects.reorder": {
    source: "json",
    json: {} as ReorderProjectRequest,
  },
  "projects.sidebarBootstrap": { source: "none" },
  "projects.update": { source: "json", json: {} as UpdateProjectRequest },
  "projects.updateSource": {
    source: "json",
    json: {} as UpdateProjectSourceRequest,
  },
  "system.environmentProviders": {
    source: "query",
    query: {} as SystemEnvironmentProvidersQuery,
  },
  "system.executionOptions": {
    source: "query",
    query: {} as SystemExecutionOptionsQuery,
  },
  "system.generalSettings": {
    source: "json",
    json: {} as AppSettingsUpdate,
  },
  "system.providers": { source: "query", query: {} as SystemProvidersQuery },
  "system.providerStates": {
    source: "query",
    query: {} as SystemProvidersQuery,
  },
  "system.version": { source: "query", query: {} as SystemVersionQuery },
  "system.config": { source: "none" },
  "system.uiPreferences": { source: "none" },
  "system.updateUiPreference": {
    source: "json",
    json: {} as UpdateUiPreferenceRequest,
  },
  "system.resetUiPreference": { source: "none" },
  // The contract declares `source: "form"` with a null schema. The transport
  // accepts the browser's wire-level FormData object rather than pretending a
  // plain record is FormData at the fetch boundary.
  "system.voiceTranscription": {
    source: "form",
    form: {} as FormData,
  },
  "threads.childSummary": { source: "none" },
  "threads.create": { source: "json", json: {} as CreateThreadRequest },
  "threads.createQueuedMessage": {
    source: "json",
    json: {} as CreateQueuedMessageRequest,
  },
  "threads.defaultExecutionOptions": { source: "none" },
  "threads.delete": { source: "json", json: {} as DeleteThreadRequest },
  "threads.deleteQueuedMessage": { source: "none" },
  "threads.get": { source: "query", query: {} as ThreadGetQuery },
  "threads.historyRefresh": { source: "none" },
  "threads.hostFileContent": {
    source: "query",
    query: {} as ThreadHostFileContentQuery,
  },
  "threads.interaction": { source: "none" },
  "threads.interactions": { source: "none" },
  "threads.pin": { source: "none" },
  "threads.pinOrder": {
    source: "json",
    json: {} as ReorderPinnedThreadRequest,
  },
  "threads.queuedMessages": { source: "none" },
  "threads.rawFile": { source: "query", query: {} as ThreadFilesRawQuery },
  "threads.read": { source: "none" },
  "threads.reorderQueuedMessage": {
    source: "json",
    json: {} as ReorderQueuedMessageRequest,
  },
  "threads.resolveInteraction": {
    source: "json",
    json: {} as ResolvePendingInteractionRequest,
  },
  "threads.cancelInteraction": { source: "none" },
  "threads.send": { source: "json", json: {} as SendMessageRequest },
  "threads.sendQueuedMessage": {
    source: "json",
    json: {} as SendQueuedMessageRequest,
  },
  "threads.setQueuedMessageGroupBoundary": {
    source: "json",
    json: {} as SetQueuedMessageGroupBoundaryRequest,
  },
  "threads.stop": { source: "none" },
  "threads.storageContent": {
    source: "query",
    query: {} as ThreadStorageContentQuery,
  },
  "threads.storageFile": { source: "none" },
  "threads.storageFiles": {
    source: "query",
    query: {} as ThreadStorageFilesQuery,
  },
  "threads.storageLocation": { source: "none" },
  "threads.storagePaths": {
    source: "query",
    query: {} as ThreadStoragePathsQuery,
  },
  "threads.tabs": { source: "none" },
  "threads.timeline": { source: "query", query: {} as ThreadTimelineQuery },
  "threads.unpin": { source: "none" },
  "threads.unread": { source: "none" },
  "threads.updateQueuedMessage": {
    source: "json",
    json: {} as UpdateQueuedMessageRequest,
  },
  "threads.updateTabs": {
    source: "json",
    json: {} as UpdateThreadTabsRequest,
  },
  "threads.worktreeFile": { source: "none" },
  "terminals.close": { source: "json", json: {} as CloseTerminalRequest },
  "terminals.create": { source: "json", json: {} as CreateTerminalRequest },
  "terminals.list": { source: "query", query: {} as TerminalListQuery },
  "terminals.update": { source: "json", json: {} as UpdateTerminalRequest },
  "threadSections.create": {
    source: "json",
    json: {} as CreateThreadSectionRequest,
  },
  "threadSections.delete": {
    source: "json",
    json: {} as DeleteThreadSectionRequest,
  },
  "threadSections.update": {
    source: "json",
    json: {} as UpdateThreadSectionRequest,
  },
} as const satisfies Record<string, LoomApiRequestSpec>;

export type SystemProviderStatesQueryShape = SystemProvidersQuery;

/**
 * The response type of each allowlisted route, derived from the contract.
 *
 * Kept beside the request specs so a route's request and response are declared
 * together: a caller that reads `loomApiJson(routeId, …)` gets the contract's
 * own response type without naming it, which is also what lets the transport
 * take a single inferred type argument.
 */
export interface LoomApiResponseSpecs {
  "filePreviews.content": unknown;
  "environments.diffFile": { path: string; content: string; contentEncoding: "base64" | "utf8" };
  "environments.get": Environment;
  "hosts.createJoinCode": CreateHostJoinCodeResponse;
  "hosts.delete": { ok: true };
  "hosts.directory": HostDirectoryListing;
  "hosts.list": Host[];
  "hosts.updatePermissionCeiling": unknown;
  "projects.attachmentContent": unknown;
  "projects.branchOptions": unknown;
  "projects.create": ProjectResponse;
  "projects.createSource": ProjectSource;
  "projects.defaultExecutionOptions": ProjectExecutionDefaults | null;
  "projects.delete": { ok: true };
  "projects.deleteSource": { ok: true };
  "projects.fileContent": unknown;
  "projects.reorder": ProjectResponse[];
  "projects.sidebarBootstrap": SidebarBootstrapResponse;
  "projects.update": ProjectResponse;
  "projects.updateSource": ProjectSource;
  "system.environmentProviders": SystemEnvironmentProvidersResponse;
  "system.executionOptions": SystemExecutionOptionsResponse;
  "system.generalSettings": AppSettings & {
    showUnhandledProviderEvents?: boolean;
  };
  "system.providers": SystemProviderInfo[];
  "system.providerStates": SystemProviderStatesResponse;
  "system.version": SystemVersionResponse;
  "system.config": SystemConfigResponse;
  "system.uiPreferences": UiPreferencesResponse;
  "system.updateUiPreference": UiPreferenceResponse;
  "system.resetUiPreference": UiPreferenceResponse;
  "system.voiceTranscription": { text: string };
  "threads.childSummary": ThreadChildSummaryResponse;
  "threads.create": ThreadResponse;
  "threads.createQueuedMessage": ThreadQueuedMessage;
  "threads.defaultExecutionOptions": ResolvedThreadExecutionOptions | null;
  "threads.delete": { ok: true };
  "threads.deleteQueuedMessage": { ok: true };
  "threads.get": ThreadResponse;
  "threads.historyRefresh": { status: string; reason: string | null };
  "threads.hostFileContent": unknown;
  "threads.interaction": PendingInteraction;
  "threads.interactions": ThreadPendingInteractionsResponse;
  "threads.pin": ThreadResponse;
  "threads.pinOrder": ThreadListEntry[];
  "threads.queuedMessages": ThreadQueuedMessageListResponse;
  "threads.rawFile": unknown;
  "threads.read": ThreadResponse;
  "threads.reorderQueuedMessage": ThreadQueuedMessageListResponse;
  "threads.resolveInteraction": PendingInteraction;
  "threads.cancelInteraction": PendingInteraction;
  "threads.send": SendMessageResponse;
  "threads.sendQueuedMessage": SendQueuedMessageResponse;
  "threads.setQueuedMessageGroupBoundary": ThreadQueuedMessageListResponse;
  "threads.stop": { ok: true };
  "threads.storageContent": unknown;
  "threads.storageFile": unknown;
  "threads.storageFiles": ThreadStorageFileListResponse;
  "threads.storageLocation": ThreadStorageLocationResponse;
  "threads.storagePaths": ThreadStoragePathListResponse;
  "threads.tabs": ThreadTabsResponse;
  "threads.timeline": ThreadTimelineResponse;
  "threads.unpin": ThreadResponse;
  "threads.unread": ThreadResponse;
  "threads.updateQueuedMessage": ThreadQueuedMessage;
  "threads.updateTabs": ThreadTabsResponse;
  "threads.worktreeFile": unknown;
  "terminals.close": TerminalSession;
  "terminals.create": TerminalSession;
  "terminals.list": TerminalListResponse;
  "terminals.update": TerminalSession;
  "threadSections.create": ThreadSectionResponse;
  "threadSections.delete": ThreadSectionMutationResponse;
  "threadSections.update": ThreadSectionMutationResponse;
}

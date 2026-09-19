import type {
  CreateHostJoinCodeResponse,
  CreateThreadRequest,
  EnvironmentDiffFileQuery,
  ProjectAttachmentContentQuery,
  ProjectBranchesQuery,
  ProjectDefaultExecutionOptionsQuery,
  ProjectFileContentQuery,
  SendMessageRequest,
  SendMessageResponse,
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
  ThreadChildSummaryResponse,
  ThreadFilesRawQuery,
  ThreadGetQuery,
  ThreadHostFileContentQuery,
  ResolvePendingInteractionRequest,
  ThreadResponse,
  ThreadPendingInteractionsResponse,
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
  UpdateThreadTabsRequest,
  UpdateUiPreferenceRequest,
} from "@bb/server-contract";
import type {
  Environment,
  Host,
  PendingInteraction,
  ProjectExecutionDefaults,
  ResolvedThreadExecutionOptions,
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
  "projects.defaultExecutionOptions": {
    source: "query",
    query: {} as ProjectDefaultExecutionOptionsQuery,
  },
  "projects.fileContent": {
    source: "query",
    query: {} as ProjectFileContentQuery,
  },
  "projects.sidebarBootstrap": { source: "none" },
  "system.environmentProviders": {
    source: "query",
    query: {} as SystemEnvironmentProvidersQuery,
  },
  "system.executionOptions": {
    source: "query",
    query: {} as SystemExecutionOptionsQuery,
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
  "threads.defaultExecutionOptions": { source: "none" },
  "threads.get": { source: "query", query: {} as ThreadGetQuery },
  "threads.hostFileContent": {
    source: "query",
    query: {} as ThreadHostFileContentQuery,
  },
  "threads.interaction": { source: "none" },
  "threads.interactions": { source: "none" },
  "threads.rawFile": { source: "query", query: {} as ThreadFilesRawQuery },
  "threads.read": { source: "none" },
  "threads.resolveInteraction": {
    source: "json",
    json: {} as ResolvePendingInteractionRequest,
  },
  "threads.cancelInteraction": { source: "none" },
  "threads.send": { source: "json", json: {} as SendMessageRequest },
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
  "threads.unread": { source: "none" },
  "threads.updateTabs": {
    source: "json",
    json: {} as UpdateThreadTabsRequest,
  },
  "threads.worktreeFile": { source: "none" },
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
  "hosts.list": Host[];
  "hosts.updatePermissionCeiling": unknown;
  "projects.attachmentContent": unknown;
  "projects.branchOptions": unknown;
  "projects.defaultExecutionOptions": ProjectExecutionDefaults | null;
  "projects.fileContent": unknown;
  "projects.sidebarBootstrap": SidebarBootstrapResponse;
  "system.environmentProviders": SystemEnvironmentProvidersResponse;
  "system.executionOptions": SystemExecutionOptionsResponse;
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
  "threads.defaultExecutionOptions": ResolvedThreadExecutionOptions | null;
  "threads.get": ThreadResponse;
  "threads.hostFileContent": unknown;
  "threads.interaction": PendingInteraction;
  "threads.interactions": ThreadPendingInteractionsResponse;
  "threads.rawFile": unknown;
  "threads.read": ThreadResponse;
  "threads.resolveInteraction": PendingInteraction;
  "threads.cancelInteraction": PendingInteraction;
  "threads.send": SendMessageResponse;
  "threads.storageContent": unknown;
  "threads.storageFile": unknown;
  "threads.storageFiles": ThreadStorageFileListResponse;
  "threads.storageLocation": ThreadStorageLocationResponse;
  "threads.storagePaths": ThreadStoragePathListResponse;
  "threads.tabs": ThreadTabsResponse;
  "threads.timeline": ThreadTimelineResponse;
  "threads.unread": ThreadResponse;
  "threads.updateTabs": ThreadTabsResponse;
  "threads.worktreeFile": unknown;
}

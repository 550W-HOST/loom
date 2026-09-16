import type {
  EnvironmentDiffFileQuery,
  ProjectAttachmentContentQuery,
  ProjectBranchesQuery,
  ProjectFileContentQuery,
  SystemExecutionOptionsQuery,
  SystemProvidersQuery,
  SystemVersionQuery,
  ThreadFilesRawQuery,
  ThreadHostFileContentQuery,
  ThreadStorageContentQuery,
} from "@bb/server-contract";

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
  "hosts.createJoinCode": { source: "json", json: {} as Record<string, never> },
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
  "projects.fileContent": {
    source: "query",
    query: {} as ProjectFileContentQuery,
  },
  "projects.sidebarBootstrap": { source: "none" },
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
  // The contract declares `source: "form"` with a null schema: multipart with
  // arbitrary string/Blob fields.
  "system.voiceTranscription": {
    source: "form",
    form: {} as Record<string, string | Blob>,
  },
  "threads.hostFileContent": {
    source: "query",
    query: {} as ThreadHostFileContentQuery,
  },
  "threads.rawFile": { source: "query", query: {} as ThreadFilesRawQuery },
  "threads.storageContent": {
    source: "query",
    query: {} as ThreadStorageContentQuery,
  },
  "threads.storageFile": { source: "none" },
  "threads.worktreeFile": { source: "none" },
} as const satisfies Record<string, LoomApiRequestSpec>;

export type SystemProviderStatesQueryShape = SystemProvidersQuery;

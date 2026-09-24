import {
  PERSONAL_PROJECT_ID,
  findLocalPathProjectSourceForHost,
  type Environment,
  type Host,
  type ProjectExecutionDefaults,
  type ResolvedThreadExecutionOptions,
  type ThreadListEntry,
} from "@bb/domain";
import type {
  CreateThreadRequest,
  DeleteThreadRequest,
  ReorderPinnedThreadRequest,
  SendMessageRequest,
  SendMessageResponse,
  SidebarBootstrapResponse,
  SystemEnvironmentProvider,
  SystemEnvironmentProvidersQuery,
  ThreadChildSummaryResponse,
  ThreadGetQuery,
  ThreadResponse,
  ThreadTabsResponse,
  ThreadTimelineQuery,
  ThreadTimelineResponse,
  UpdateThreadTabsRequest,
} from "@bb/server-contract";
import { appSurfaceRequestInit } from "@/lib/app-surface";
import {
  loomApiJson,
  loomApiOrigin,
  throwLoomHttpError,
} from "@/lib/loom-http";

export const PERSONAL_WORKSPACE_PROVIDER_ID = "personal-workspace";
export const PROJECT_CHECKOUT_PROVIDER_ID = "project-checkout";
export const GIT_WORKTREE_PROVIDER_ID = "git-worktree";

const ENVIRONMENT_READY_POLL_INTERVAL_MS = 250;
const ENVIRONMENT_READY_MAX_ATTEMPTS = 120;

export class LoomThreadRuntimeError extends Error {
  readonly code = "loom_thread_runtime_unavailable";

  constructor(message: string) {
    super(message);
    this.name = "LoomThreadRuntimeError";
  }
}

interface CreateLoomEnvironmentRequest {
  kind: "managed" | "unmanaged";
  project_id: string;
  host_id: string;
  path?: string;
  provider_id?: string;
  base_branch?: string;
  branch_name?: string;
}

interface CreateLoomEnvironmentResponse {
  environment: {
    id: string;
    path: string | null;
    status: Environment["status"];
  };
  event_id: string;
}

interface EnvironmentWaitOptions {
  maxAttempts?: number;
  pollIntervalMs?: number;
  sleep?: (milliseconds: number) => Promise<void>;
}

type SidebarProject = SidebarBootstrapResponse["personalProject"];

interface ResolvedProject {
  clientProjectId: string;
  project: SidebarProject;
  serverProjectId: string;
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => globalThis.setTimeout(resolve, milliseconds));
}

async function readSidebar(signal?: AbortSignal): Promise<SidebarBootstrapResponse> {
  return loomApiJson("projects.sidebarBootstrap", { signal });
}

async function resolveProject(
  projectId: string,
  signal?: AbortSignal,
): Promise<ResolvedProject> {
  const sidebar = await readSidebar(signal);
  if (
    projectId === PERSONAL_PROJECT_ID ||
    projectId === sidebar.personalProject.id
  ) {
    return {
      clientProjectId: PERSONAL_PROJECT_ID,
      project: sidebar.personalProject,
      serverProjectId: sidebar.personalProject.id,
    };
  }
  const project = sidebar.projects.find((candidate) => candidate.id === projectId);
  if (project === undefined) {
    throw new LoomThreadRuntimeError(`Project ${projectId} is not available`);
  }
  return {
    clientProjectId: project.id,
    project,
    serverProjectId: project.id,
  };
}

async function requireConnectedHost(
  hostId: string,
  signal?: AbortSignal,
): Promise<Host> {
  const hosts = await loomApiJson("hosts.list", { signal });
  const host = hosts.find((candidate) => candidate.id === hostId);
  if (host === undefined) {
    throw new LoomThreadRuntimeError(`Machine ${hostId} is not enrolled`);
  }
  if (host.status !== "connected") {
    throw new LoomThreadRuntimeError(
      `Machine ${host.name || host.id} is disconnected; reconnect that machine before creating the thread`,
    );
  }
  return host;
}

export async function createLoomEnvironment(
  request: CreateLoomEnvironmentRequest,
  signal?: AbortSignal,
): Promise<CreateLoomEnvironmentResponse> {
  const headers = new Headers({ "content-type": "application/json" });
  const response = await fetch(
    new URL("/api/v1/environments", loomApiOrigin()),
    appSurfaceRequestInit({
      method: "POST",
      headers,
      body: JSON.stringify(request),
      signal,
    }),
  );
  if (!response.ok) {
    await throwLoomHttpError(response);
  }
  return (await response.json()) as CreateLoomEnvironmentResponse;
}

export async function waitForLoomEnvironmentReady(
  environmentId: string,
  expected: { hostId: string; projectId: string },
  options: EnvironmentWaitOptions = {},
): Promise<Environment> {
  const maxAttempts = options.maxAttempts ?? ENVIRONMENT_READY_MAX_ATTEMPTS;
  const pollIntervalMs =
    options.pollIntervalMs ?? ENVIRONMENT_READY_POLL_INTERVAL_MS;
  const wait = options.sleep ?? sleep;

  for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
    const environment = await loomApiJson("environments.get", {
      param: { id: environmentId },
    });
    if (
      environment.hostId !== expected.hostId ||
      environment.projectId !== expected.projectId
    ) {
      throw new LoomThreadRuntimeError(
        `Environment ${environmentId} resolved outside the selected project or machine`,
      );
    }
    if (environment.status === "ready") {
      if (!environment.path) {
        throw new LoomThreadRuntimeError(
          `Environment ${environmentId} is ready but has no workspace path`,
        );
      }
      return environment;
    }
    if (environment.status === "error" || environment.status === "destroyed") {
      throw new LoomThreadRuntimeError(
        `Environment ${environmentId} entered ${environment.status} before it was ready`,
      );
    }
    if (attempt + 1 < maxAttempts) {
      await wait(pollIntervalMs);
    }
  }
  throw new LoomThreadRuntimeError(
    `Environment ${environmentId} did not become ready in time`,
  );
}

async function createReadyEnvironment(
  request: CreateLoomEnvironmentRequest,
  options?: EnvironmentWaitOptions,
): Promise<Environment> {
  const created = await createLoomEnvironment(request);
  const ready = await waitForLoomEnvironmentReady(
    created.environment.id,
    { hostId: request.host_id, projectId: request.project_id },
    options,
  );
  await requireConnectedHost(request.host_id);
  return ready;
}

/**
 * The base branch a worktree provider input asks for, if any.
 *
 * The composer's seed sends `{ branch: { kind: "default" } }` or
 * `{ branch: { kind: "named", name } }`; anything else, including `null`,
 * means the worker resolves the source's default branch.
 */
function baseBranchFromProviderInputs(inputs: unknown): string | undefined {
  if (inputs === null || typeof inputs !== "object") {
    return undefined;
  }
  const branch = (inputs as { branch?: unknown }).branch;
  if (branch === null || typeof branch !== "object") {
    return undefined;
  }
  const named = branch as { kind?: unknown; name?: unknown };
  if (
    named.kind === "named" &&
    typeof named.name === "string" &&
    named.name.trim() !== ""
  ) {
    return named.name;
  }
  return undefined;
}

/** The base branch a host workspace form names, if it names one. */
function baseBranchFromSelection(
  selection: { kind: "default" } | { kind: "named"; name: string },
): string | undefined {
  return selection.kind === "named" ? selection.name : undefined;
}

async function resolveProviderEnvironment(
  environment: Extract<CreateThreadRequest["environment"], { type: "provider" }>,
  resolvedProject: ResolvedProject,
  options?: EnvironmentWaitOptions,
): Promise<string> {
  const hostId = environment.machine.hostId;
  await requireConnectedHost(hostId);

  if (environment.environmentProviderId === GIT_WORKTREE_PROVIDER_ID) {
    const ready = await createReadyEnvironment(
      {
        kind: "managed",
        project_id: resolvedProject.serverProjectId,
        host_id: hostId,
        provider_id: GIT_WORKTREE_PROVIDER_ID,
        base_branch: baseBranchFromProviderInputs(environment.inputs),
      },
      options,
    );
    return ready.id;
  }

  if (environment.inputs !== null) {
    throw new LoomThreadRuntimeError(
      `Environment provider ${environment.environmentProviderId} does not accept inputs in loom`,
    );
  }

  if (environment.environmentProviderId === PERSONAL_WORKSPACE_PROVIDER_ID) {
    if (resolvedProject.clientProjectId !== PERSONAL_PROJECT_ID) {
      throw new LoomThreadRuntimeError(
        "Personal workspace can only be used with the Personal project",
      );
    }
    const ready = await createReadyEnvironment(
      {
        kind: "managed",
        project_id: resolvedProject.serverProjectId,
        host_id: hostId,
      },
      options,
    );
    return ready.id;
  }

  if (environment.environmentProviderId === PROJECT_CHECKOUT_PROVIDER_ID) {
    if (resolvedProject.clientProjectId === PERSONAL_PROJECT_ID) {
      throw new LoomThreadRuntimeError(
        "Project checkout requires a project with a workspace source",
      );
    }
    const source = findLocalPathProjectSourceForHost(
      resolvedProject.project.sources,
      hostId,
    );
    if (source === undefined || !source.path) {
      throw new LoomThreadRuntimeError(
        `Project ${resolvedProject.project.name} has no workspace source on machine ${hostId}`,
      );
    }
    const ready = await createReadyEnvironment(
      {
        kind: "unmanaged",
        project_id: resolvedProject.serverProjectId,
        host_id: hostId,
        path: source.path,
      },
      options,
    );
    return ready.id;
  }

  throw new LoomThreadRuntimeError(
    `Environment provider ${environment.environmentProviderId} is not supported by loom`,
  );
}

async function resolveHostEnvironment(
  environment: Extract<CreateThreadRequest["environment"], { type: "host" }>,
  resolvedProject: ResolvedProject,
  options?: EnvironmentWaitOptions,
): Promise<string> {
  if (!environment.hostId) {
    throw new LoomThreadRuntimeError(
      "Choose a specific connected machine for this workspace",
    );
  }
  await requireConnectedHost(environment.hostId);
  if (environment.workspace.type === "managed-worktree") {
    const ready = await createReadyEnvironment(
      {
        kind: "managed",
        project_id: resolvedProject.serverProjectId,
        host_id: environment.hostId,
        provider_id: GIT_WORKTREE_PROVIDER_ID,
        base_branch: baseBranchFromSelection(environment.workspace.baseBranch),
      },
      options,
    );
    return ready.id;
  }
  let request: CreateLoomEnvironmentRequest;
  if (environment.workspace.type === "personal") {
    request = {
      kind: "managed",
      project_id: resolvedProject.serverProjectId,
      host_id: environment.hostId,
    };
  } else {
    if (environment.workspace.path === null) {
      throw new LoomThreadRuntimeError(
        "An unmanaged workspace requires an absolute path",
      );
    }
    request = {
      kind: "unmanaged",
      project_id: resolvedProject.serverProjectId,
      host_id: environment.hostId,
      path: environment.workspace.path,
    };
  }
  const ready = await createReadyEnvironment(request, options);
  return ready.id;
}

export async function resolveLoomThreadEnvironment(
  projectId: string,
  environment: CreateThreadRequest["environment"],
  options?: EnvironmentWaitOptions,
): Promise<{ clientProjectId: string; environmentId: string; serverProjectId: string }> {
  const project = await resolveProject(projectId);
  if (environment.type === "reuse") {
    const ready = await loomApiJson("environments.get", {
      param: { id: environment.environmentId },
    });
    if (ready.projectId !== project.serverProjectId) {
      throw new LoomThreadRuntimeError(
        `Environment ${ready.id} belongs to a different project`,
      );
    }
    await requireConnectedHost(ready.hostId);
    if (ready.status !== "ready" || !ready.path) {
      throw new LoomThreadRuntimeError(
        `Environment ${ready.id} is ${ready.status} and cannot run yet`,
      );
    }
    return {
      clientProjectId: project.clientProjectId,
      environmentId: ready.id,
      serverProjectId: project.serverProjectId,
    };
  }
  if (environment.type === "provider") {
    return {
      clientProjectId: project.clientProjectId,
      environmentId: await resolveProviderEnvironment(environment, project, options),
      serverProjectId: project.serverProjectId,
    };
  }
  if (environment.type === "host") {
    return {
      clientProjectId: project.clientProjectId,
      environmentId: await resolveHostEnvironment(environment, project, options),
      serverProjectId: project.serverProjectId,
    };
  }
  throw new LoomThreadRuntimeError(
    "Choose a concrete workspace before creating the thread",
  );
}

export async function loomSpawnThread(
  request: CreateThreadRequest,
  options?: EnvironmentWaitOptions,
): Promise<ThreadResponse> {
  if (request.sendAt !== undefined) {
    throw new LoomThreadRuntimeError(
      "Scheduling a new thread is not implemented by loom yet",
    );
  }
  const resolved = await resolveLoomThreadEnvironment(
    request.projectId,
    request.environment,
    options,
  );
  const thread = await loomApiJson("threads.create", {
    json: {
      ...request,
      projectId: resolved.serverProjectId,
      environment: {
        type: "reuse",
        environmentId: resolved.environmentId,
      },
    },
  });
  return { ...thread, projectId: resolved.clientProjectId };
}

export async function loomSendThreadMessage(
  request: SendMessageRequest & { threadId: string },
): Promise<SendMessageResponse> {
  const { threadId, ...json } = request;
  return loomApiJson("threads.send", {
    param: { id: threadId },
    json,
  });
}

function normalizeThreadProjectWithSidebar(
  thread: ThreadResponse,
  sidebar: SidebarBootstrapResponse,
): ThreadResponse {
  return thread.projectId === sidebar.personalProject.id
    ? { ...thread, projectId: PERSONAL_PROJECT_ID }
    : thread;
}

async function normalizeThreadProject(
  thread: ThreadResponse,
  signal?: AbortSignal,
): Promise<ThreadResponse> {
  return normalizeThreadProjectWithSidebar(thread, await readSidebar(signal));
}

export async function loomGetThread(request: {
  include?: ThreadGetQuery["include"];
  signal?: AbortSignal;
  threadId: string;
}): Promise<ThreadResponse> {
  const thread = await loomApiJson("threads.get", {
    param: { id: request.threadId },
    query: request.include === undefined ? {} : { include: request.include },
    signal: request.signal,
  });
  return normalizeThreadProject(thread, request.signal);
}

export async function loomGetThreadTimeline(
  request: ThreadTimelineQuery & { signal?: AbortSignal; threadId: string },
): Promise<ThreadTimelineResponse> {
  const { signal, threadId, ...query } = request;
  return loomApiJson("threads.timeline", {
    param: { id: threadId },
    query,
    signal,
  });
}

export function loomThreadDefaultExecutionOptions(request: {
  signal?: AbortSignal;
  threadId: string;
}): Promise<ResolvedThreadExecutionOptions | null> {
  return loomApiJson("threads.defaultExecutionOptions", {
    param: { id: request.threadId },
    signal: request.signal,
  });
}

/**
 * How many live children the thread has, over the contract route.
 *
 * The delete-confirmation flow reads this before offering "delete with
 * children" so it can warn about — and ask to confirm — the threads that would
 * go with the parent. `threads.childSummary` was still the fail-closed browser
 * SDK stub, so the app's delete dialog could never see a non-zero count and
 * quietly skipped the child confirmation; the server route already exists,
 * counts non-deleted children, and is contract-tested, so this is the app half.
 */
export function loomThreadChildSummary(request: {
  signal?: AbortSignal;
  threadId: string;
}): Promise<ThreadChildSummaryResponse> {
  return loomApiJson("threads.childSummary", {
    param: { id: request.threadId },
    signal: request.signal,
  });
}

/**
 * Delete a thread through the contract route.
 *
 * `threads.delete` was still the fail-closed browser SDK stub, so confirming
 * the delete dialog threw `BrowserSdkUnavailableError` instead of reaching the
 * server — and the child-summary reader above could never be acted on. The
 * contract route is a `DELETE` on the thread itself whose JSON body carries the
 * confirmation the server demands before it takes threads with children down:
 * an unconfirmed parent comes back as a typed `409` rather than a silent
 * cascade. The server answers `{ ok: true }`, so the parsed body is returned
 * rather than an invented one.
 */
export function loomDeleteThread(
  request: DeleteThreadRequest & { threadId: string },
): Promise<{ ok: true }> {
  const { threadId, ...json } = request;
  return loomApiJson("threads.delete", {
    param: { id: threadId },
    json,
  });
}

/**
 * Stop the thread's in-flight run through the contract route.
 *
 * `threads.stop` was still the fail-closed browser SDK stub, so the app's stop
 * control threw `BrowserSdkUnavailableError` instead of reaching the server and
 * the run kept going. The contract route is a bodyless `POST` on the thread's
 * `stop` path; the server answers `{ ok: true }` and is idempotent — a thread
 * with no run in flight is already in the state the caller asked for — so the
 * parsed body is returned rather than an invented one.
 */
export function loomStopThread(request: {
  threadId: string;
}): Promise<{ ok: true }> {
  return loomApiJson("threads.stop", { param: { id: request.threadId } });
}

export async function loomMarkThreadRead(request: {
  threadId: string;
}): Promise<ThreadResponse> {
  // Resolve the personal-project identity before the mutating request. A
  // sidebar failure must not make a successful read-state write look failed.
  const sidebar = await readSidebar();
  const thread = await loomApiJson("threads.read", {
    param: { id: request.threadId },
  });
  return normalizeThreadProjectWithSidebar(thread, sidebar);
}

/**
 * Asks the server to read a thread's conversation from its agent again.
 *
 * A read serves what is stored and asks for a load behind it, so this is not how
 * a conversation arrives in the first place. It is the explicit ask, for the one
 * case no poll can see — the session moved on somewhere this server cannot
 * observe — and for a failure whose wait has not passed yet. The answer says how
 * the conversation stands now; the rows themselves come from the timeline, which
 * the caller refetches.
 */
export function loomRefreshThreadHistory(request: {
  threadId: string;
}): Promise<{ status: string; reason: string | null }> {
  return loomApiJson("threads.historyRefresh", {
    param: { id: request.threadId },
  });
}

export async function loomMarkThreadUnread(request: {
  threadId: string;
}): Promise<ThreadResponse> {
  const sidebar = await readSidebar();
  const thread = await loomApiJson("threads.unread", {
    param: { id: request.threadId },
  });
  return normalizeThreadProjectWithSidebar(thread, sidebar);
}

/**
 * Pin, unpin and reorder over the contract routes the pinned thread list uses.
 *
 * `threads.pin` was still the fail-closed browser SDK stub, so pinning from the
 * thread menu or the sidebar drag threw `BrowserSdkUnavailableError` instead of
 * reaching the server. Its siblings were unwired the same way: `threads.unpin`
 * is the other half of the toggle, and `threads.pinOrder` is the drag reorder.
 * The contract routes, their handlers and their contract tests already exist,
 * so this is the app half only.
 *
 * These return the server's own thread summary without a sidebar round-trip:
 * `proj_personal` is the reserved id the client addresses the personal scope by
 * (see the personal-scope decision), so there is no minted id left to remap the
 * way the older read-state wiring had to.
 */
export function loomPinThread(request: {
  threadId: string;
}): Promise<ThreadResponse> {
  return loomApiJson("threads.pin", { param: { id: request.threadId } });
}

export function loomUnpinThread(request: {
  threadId: string;
}): Promise<ThreadResponse> {
  return loomApiJson("threads.unpin", { param: { id: request.threadId } });
}

export function loomReorderPinnedThread(
  request: ReorderPinnedThreadRequest & { threadId: string },
): Promise<ThreadListEntry[]> {
  const { threadId, ...json } = request;
  return loomApiJson("threads.pinOrder", {
    param: { id: threadId },
    json,
  });
}

export function loomGetThreadTabs(request: {
  signal?: AbortSignal;
  threadId: string;
}): Promise<ThreadTabsResponse> {
  return loomApiJson("threads.tabs", {
    param: { id: request.threadId },
    signal: request.signal,
  });
}

export function loomUpdateThreadTabs(
  request: UpdateThreadTabsRequest & { threadId: string },
): Promise<ThreadTabsResponse> {
  const { threadId, ...json } = request;
  return loomApiJson("threads.updateTabs", {
    param: { id: threadId },
    json,
  });
}

export async function loomGetEnvironment(request: {
  environmentId: string;
  signal?: AbortSignal;
}): Promise<Environment> {
  return loomApiJson("environments.get", {
    param: { id: request.environmentId },
    signal: request.signal,
  });
}

export async function loomListEnvironmentProviders(
  request: SystemEnvironmentProvidersQuery & { signal?: AbortSignal },
): Promise<SystemEnvironmentProvider[]> {
  const { signal, ...query } = request;
  const response = await loomApiJson("system.environmentProviders", {
    query,
    signal,
  });
  return response.providers;
}

export async function loomProjectDefaultExecutionOptions(request: {
  projectId: string;
  signal?: AbortSignal;
}): Promise<ProjectExecutionDefaults | null> {
  const project = await resolveProject(request.projectId, request.signal);
  return loomApiJson("projects.defaultExecutionOptions", {
    param: { id: project.serverProjectId },
    query: {},
    signal: request.signal,
  });
}

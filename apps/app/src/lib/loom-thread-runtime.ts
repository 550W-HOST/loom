import {
  PERSONAL_PROJECT_ID,
  findLocalPathProjectSourceForHost,
  type Environment,
  type Host,
  type ProjectExecutionDefaults,
} from "@bb/domain";
import type {
  CreateThreadRequest,
  SendMessageRequest,
  SendMessageResponse,
  SidebarBootstrapResponse,
  SystemEnvironmentProvider,
  SystemEnvironmentProvidersQuery,
  ThreadGetQuery,
  ThreadResponse,
  ThreadTimelineQuery,
  ThreadTimelineResponse,
} from "@bb/server-contract";
import { appSurfaceRequestInit } from "@/lib/app-surface";
import {
  loomApiJson,
  loomApiOrigin,
  throwLoomHttpError,
} from "@/lib/loom-http";

export const PERSONAL_WORKSPACE_PROVIDER_ID = "personal-workspace";
export const PROJECT_CHECKOUT_PROVIDER_ID = "project-checkout";

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

async function resolveProviderEnvironment(
  environment: Extract<CreateThreadRequest["environment"], { type: "provider" }>,
  resolvedProject: ResolvedProject,
  options?: EnvironmentWaitOptions,
): Promise<string> {
  if (environment.inputs !== null) {
    throw new LoomThreadRuntimeError(
      `Environment provider ${environment.environmentProviderId} does not accept inputs in loom`,
    );
  }
  const hostId = environment.machine.hostId;
  await requireConnectedHost(hostId);

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
    throw new LoomThreadRuntimeError(
      "Managed Git worktrees are not implemented by loom yet",
    );
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

async function normalizeThreadProject(
  thread: ThreadResponse,
  signal?: AbortSignal,
): Promise<ThreadResponse> {
  const sidebar = await readSidebar(signal);
  return thread.projectId === sidebar.personalProject.id
    ? { ...thread, projectId: PERSONAL_PROJECT_ID }
    : thread;
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

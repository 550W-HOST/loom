import { afterEach, describe, expect, it, vi } from "vitest";
import { PERSONAL_PROJECT_ID, type Environment, type Host } from "@bb/domain";
import type {
  CreateThreadRequest,
  SidebarBootstrapResponse,
  ThreadResponse,
} from "@bb/server-contract";
import { BrowserSdkUnavailableError } from "@bb/sdk/browser";
import { sdk } from "@/lib/sdk";
import {
  LoomThreadRuntimeError,
  PERSONAL_WORKSPACE_PROVIDER_ID,
  PROJECT_CHECKOUT_PROVIDER_ID,
  loomListEnvironmentProviders,
  loomMarkThreadRead,
  loomMarkThreadUnread,
  loomGetThreadTabs,
  loomSpawnThread,
  loomThreadChildSummary,
  loomThreadDefaultExecutionOptions,
  loomUpdateThreadTabs,
  resolveLoomThreadEnvironment,
} from "@/lib/loom-thread-runtime";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function host(id: string, status: Host["status"] = "connected"): Host {
  return {
    id,
    name: id,
    type: "persistent",
    status,
    maxPermissionMode: "full",
    lastSeenAt: 1,
    lastRejectedProtocolVersion: null,
    createdAt: 1,
    updatedAt: 1,
  };
}

function environment(args: {
  hostId: string;
  id: string;
  path: string | null;
  projectId: string;
  status: Environment["status"];
}): Environment {
  return {
    id: args.id,
    name: null,
    projectId: args.projectId,
    hostId: args.hostId,
    path: args.path,
    isGitRepo: false,
    isWorktree: false,
    branchName: null,
    baseBranch: null,
    defaultBranch: null,
    mergeBaseBranch: null,
    status: args.status,
    environmentProviderId: null,
    lifecycle: { phase: "active", retireAt: null, teardown: null },
    environmentProviderSelection: null,
    environmentProviderInstanceKey: null,
    managed: true,
    workspaceProvisionType: "personal",
    createdAt: 1,
    updatedAt: 1,
  };
}

function sidebar(args?: {
  project?: { id: string; hostId: string; path: string };
}): SidebarBootstrapResponse {
  const project = args?.project;
  return {
    sections: [],
    projects:
      project === undefined
        ? []
        : [
            {
              id: project.id,
              kind: "standard",
              name: "Work",
              gitRemoteUrl: null,
              createdAt: 1,
              updatedAt: 1,
              sources: [
                {
                  id: "src_1",
                  projectId: project.id,
                  type: "local_path",
                  hostId: project.hostId,
                  path: project.path,
                  isDefault: true,
                  createdAt: 1,
                  updatedAt: 1,
                },
              ],
              threads: [],
              defaultExecutionOptions: null,
            },
          ],
    personalProject: {
      id: "proj_actual_personal",
      kind: "personal",
      name: "Personal",
      gitRemoteUrl: null,
      createdAt: 1,
      updatedAt: 1,
      sources: [],
      threads: [],
      defaultExecutionOptions: null,
    },
  };
}

function thread(projectId: string, environmentId: string): ThreadResponse {
  return {
    id: "thr_01M2N89ZD7FWEVRXKR15XQT3ZM",
    projectId,
    environmentId,
    providerId: "pi",
    title: null,
    titleFallback: null,
    sectionId: null,
    status: "active",
    parentThreadId: null,
    sourceThreadId: null,
    originKind: null,
    originPluginId: null,
    visibility: "visible",
    archivedAt: null,
    pinnedAt: null,
    deletedAt: null,
    lastReadAt: null,
    latestAttentionAt: 1,
    createdAt: 1,
    updatedAt: 1,
    runtime: { displayStatus: "working", hostReconnectGraceExpiresAt: null },
    activeBackgroundAgentCount: 0,
    canSpawnChild: true,
    queuedMessageCount: 0,
  };
}

function personalRequest(): CreateThreadRequest {
  return {
    projectId: PERSONAL_PROJECT_ID,
    origin: "app",
    input: [{ type: "text", text: "hello" }],
    environment: {
      type: "provider",
      environmentProviderId: PERSONAL_WORKSPACE_PROVIDER_ID,
      machine: { type: "existing", hostId: "host_personal" },
      inputs: null,
    },
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("loom New Thread runtime", () => {
  it("provisions a personal environment on the selected host before creating the thread", async () => {
    const requests: Array<{ body: unknown; method: string; path: string }> = [];
    let environmentReads = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: URL | RequestInfo, init?: RequestInit) => {
        const url = new URL(String(input));
        const method = init?.method ?? "GET";
        const body = typeof init?.body === "string" ? JSON.parse(init.body) : null;
        requests.push({ body, method, path: url.pathname });
        if (url.pathname === "/api/v1/sidebar-bootstrap") {
          return jsonResponse(sidebar());
        }
        if (url.pathname === "/api/v1/hosts") {
          return jsonResponse([host("host_personal")]);
        }
        if (url.pathname === "/api/v1/environments" && method === "POST") {
          return jsonResponse({
            environment: { id: "env_personal", path: null, status: "creating" },
            event_id: "evt_1",
          });
        }
        if (url.pathname === "/api/v1/environments/env_personal") {
          environmentReads += 1;
          return jsonResponse(
            environment({
              id: "env_personal",
              projectId: "proj_actual_personal",
              hostId: "host_personal",
              path: environmentReads === 1 ? null : "/data/env_personal",
              status: environmentReads === 1 ? "provisioning" : "ready",
            }),
          );
        }
        if (url.pathname === "/api/v1/threads" && method === "POST") {
          return jsonResponse(thread("proj_actual_personal", "env_personal"), 201);
        }
        throw new Error(`unexpected ${method} ${url.pathname}`);
      }),
    );

    const result = await loomSpawnThread(personalRequest(), {
      pollIntervalMs: 0,
      sleep: async () => {},
    });

    expect(result.projectId).toBe(PERSONAL_PROJECT_ID);
    expect(environmentReads).toBe(2);
    expect(
      requests.find(
        (request) =>
          request.path === "/api/v1/environments" && request.method === "POST",
      )?.body,
    ).toEqual({
      kind: "managed",
      project_id: "proj_actual_personal",
      host_id: "host_personal",
    });
    expect(
      requests.find(
        (request) =>
          request.path === "/api/v1/threads" && request.method === "POST",
      )?.body,
    ).toMatchObject({
      projectId: "proj_actual_personal",
      input: [{ type: "text", text: "hello" }],
      environment: { type: "reuse", environmentId: "env_personal" },
    });
  });

  it("binds project checkout to the exact source path on the selected host", async () => {
    let createBody: unknown;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: URL | RequestInfo, init?: RequestInit) => {
        const url = new URL(String(input));
        if (url.pathname === "/api/v1/sidebar-bootstrap") {
          return jsonResponse(
            sidebar({
              project: {
                id: "proj_work",
                hostId: "host_work",
                path: "/work/project",
              },
            }),
          );
        }
        if (url.pathname === "/api/v1/hosts") {
          return jsonResponse([host("host_work"), host("host_other")]);
        }
        if (url.pathname === "/api/v1/environments") {
          createBody = JSON.parse(String(init?.body));
          return jsonResponse({
            environment: { id: "env_work", path: "/work/project", status: "ready" },
            event_id: "evt_2",
          });
        }
        if (url.pathname === "/api/v1/environments/env_work") {
          return jsonResponse(
            environment({
              id: "env_work",
              projectId: "proj_work",
              hostId: "host_work",
              path: "/work/project",
              status: "ready",
            }),
          );
        }
        throw new Error(`unexpected ${url.pathname}`);
      }),
    );

    const result = await resolveLoomThreadEnvironment(
      "proj_work",
      {
        type: "provider",
        environmentProviderId: PROJECT_CHECKOUT_PROVIDER_ID,
        machine: { type: "existing", hostId: "host_work" },
        inputs: null,
      },
      { pollIntervalMs: 0, sleep: async () => {} },
    );

    expect(result.environmentId).toBe("env_work");
    expect(createBody).toEqual({
      kind: "unmanaged",
      project_id: "proj_work",
      host_id: "host_work",
      path: "/work/project",
    });
  });

  it("refuses a disconnected selected host before creating anything", async () => {
    const fetchMock = vi.fn(async (input: URL | RequestInfo) => {
      const url = new URL(String(input));
      if (url.pathname === "/api/v1/sidebar-bootstrap") {
        return jsonResponse(sidebar());
      }
      if (url.pathname === "/api/v1/hosts") {
        return jsonResponse([host("host_personal", "disconnected")]);
      }
      throw new Error(`unexpected request after host validation: ${url.pathname}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      resolveLoomThreadEnvironment(
        PERSONAL_PROJECT_ID,
        personalRequest().environment,
      ),
    ).rejects.toBeInstanceOf(LoomThreadRuntimeError);
    expect(
      fetchMock.mock.calls.some(([input, init]) => {
        const url = new URL(String(input));
        return (init as RequestInit | undefined)?.method === "POST" || url.pathname === "/api/v1/threads";
      }),
    ).toBe(false);
  });

  it("uses typed thread-detail support routes for defaults and read state", async () => {
    const requests: Array<{ method: string; path: string }> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: URL | RequestInfo, init?: RequestInit) => {
        const url = new URL(String(input));
        const method = init?.method ?? "GET";
        requests.push({ method, path: url.pathname });
        if (
          url.pathname ===
          "/api/v1/threads/thr_1/default-execution-options"
        ) {
          return jsonResponse(null);
        }
        if (
          url.pathname === "/api/v1/threads/thr_1/read" ||
          url.pathname === "/api/v1/threads/thr_1/unread"
        ) {
          return jsonResponse(thread("proj_actual_personal", "env_1"));
        }
        if (url.pathname === "/api/v1/threads/thr_1/tabs") {
          return jsonResponse({ revision: method === "PUT" ? 1 : 0, tabs: [] });
        }
        if (url.pathname === "/api/v1/sidebar-bootstrap") {
          return jsonResponse(sidebar());
        }
        throw new Error(`unexpected ${method} ${url.pathname}`);
      }),
    );

    await expect(
      loomThreadDefaultExecutionOptions({ threadId: "thr_1" }),
    ).resolves.toBeNull();
    await expect(loomMarkThreadRead({ threadId: "thr_1" })).resolves.toMatchObject(
      { projectId: PERSONAL_PROJECT_ID },
    );
    await expect(
      loomMarkThreadUnread({ threadId: "thr_1" }),
    ).resolves.toMatchObject({ projectId: PERSONAL_PROJECT_ID });
    await expect(loomGetThreadTabs({ threadId: "thr_1" })).resolves.toEqual({
      revision: 0,
      tabs: [],
    });
    await expect(
      loomUpdateThreadTabs({
        threadId: "thr_1",
        expectedRevision: 0,
        tabs: [],
      }),
    ).resolves.toEqual({ revision: 1, tabs: [] });

    expect(requests).toContainEqual({
      method: "GET",
      path: "/api/v1/threads/thr_1/default-execution-options",
    });
    expect(requests).toContainEqual({
      method: "POST",
      path: "/api/v1/threads/thr_1/read",
    });
    expect(requests).toContainEqual({
      method: "POST",
      path: "/api/v1/threads/thr_1/unread",
    });
    expect(requests).toContainEqual({
      method: "GET",
      path: "/api/v1/threads/thr_1/tabs",
    });
    expect(requests).toContainEqual({
      method: "PUT",
      path: "/api/v1/threads/thr_1/tabs",
    });
  });

  it("does not mutate read state when personal-project normalization fails", async () => {
    const requests: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: URL | RequestInfo) => {
        const url = new URL(String(input));
        requests.push(url.pathname);
        return jsonResponse({ code: "internal_error", message: "offline" }, 500);
      }),
    );

    await expect(loomMarkThreadRead({ threadId: "thr_1" })).rejects.toThrow();
    expect(requests).toEqual(["/api/v1/sidebar-bootstrap"]);
  });

  it("wires only the scoped browser SDK operations", async () => {
    expect(sdk.threads.spawn).toBe(loomSpawnThread);
    expect(sdk.threads.childSummary).toBe(loomThreadChildSummary);
    expect(sdk.threads.defaultExecutionOptions).toBe(
      loomThreadDefaultExecutionOptions,
    );
    expect(sdk.threads.markRead).toBe(loomMarkThreadRead);
    expect(sdk.threads.markUnread).toBe(loomMarkThreadUnread);
    expect(sdk.threads.tabs.get).toBe(loomGetThreadTabs);
    expect(sdk.threads.tabs.update).toBe(loomUpdateThreadTabs);
    expect(sdk.environments.listProviders).toBe(loomListEnvironmentProviders);
    await expect(sdk.threads.stop({ threadId: "thr_missing" })).rejects.toBeInstanceOf(
      BrowserSdkUnavailableError,
    );
  });
});

describe("loom thread child summary", () => {
  it("reads the child count from the contract route with no body", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({ nonDeletedChildCount: 3 }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      loomThreadChildSummary({ threadId: "thr_1" }),
    ).resolves.toEqual({ nonDeletedChildCount: 3 });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [URL, RequestInit];
    expect(init.method).toBe("GET");
    expect(init.body).toBeUndefined();
    expect(url.pathname).toBe("/api/v1/threads/thr_1/child-summary");
    expect(url.search).toBe("");
  });

  it("encodes the thread id as a single path segment", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({ nonDeletedChildCount: 0 }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await loomThreadChildSummary({ threadId: "a/b" });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/threads/a%2Fb/child-summary");
  });

  it("surfaces a route failure as a loom HTTP error", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse({ code: "thread_not_found", message: "gone" }, 404),
      ),
    );

    await expect(
      loomThreadChildSummary({ threadId: "thr_missing" }),
    ).rejects.toMatchObject({
      status: 404,
      code: "thread_not_found",
    });
  });

  it("is reachable through the browser SDK surface", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse({ nonDeletedChildCount: 2 }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(
      sdk.threads.childSummary({ threadId: "thr_1" }),
    ).resolves.toEqual({ nonDeletedChildCount: 2 });

    const [url] = fetchMock.mock.calls[0] as unknown as [URL];
    expect(url.pathname).toBe("/api/v1/threads/thr_1/child-summary");
  });
});

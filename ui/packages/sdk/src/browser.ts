import { BbHttpError, BbRequestTimeoutError } from "./response.js";

export interface CreateBrowserBbSdkArgs {
  baseUrl?: string;
  context?: Record<string, unknown>;
  fetch?: typeof fetch;
  realtimeUrl?: string;
  websocket?: unknown;
}

export interface EnvironmentDiffArgs {
  environmentId: string;
  signal?: AbortSignal;
  target: string;
  [key: string]: unknown;
}

export interface EnvironmentDiffFileArgs extends EnvironmentDiffArgs {
  path: string;
  side: "old" | "new";
}

export interface ProjectBranchesArgs {
  projectId: string;
  signal?: AbortSignal;
  [key: string]: unknown;
}

// W-593 replaces this compile-only result with contract-derived response types.
type CompileOnlyResult = any;
export type ProjectBranchesResult = CompileOnlyResult;

type UnavailableMethod = (
  ...args: readonly unknown[]
) => Promise<CompileOnlyResult>;

const BROWSER_SDK_SURFACE = {
  subscribe: null,
  environments: {
    archiveThreads: null,
    commit: null,
    delete: null,
    diff: null,
    diffBranches: null,
    diffFile: null,
    diffFiles: null,
    diffPatch: null,
    get: null,
    list: null,
    listProviders: null,
    markPullRequestDraft: null,
    markPullRequestReady: null,
    mergePullRequest: null,
    paths: null,
    pullRequest: null,
    status: null,
    update: null,
  },
  files: {
    createPreview: null,
    mkdir: null,
    read: null,
  },
  hosts: {
    cloneDefaultPath: null,
    createJoinCode: null,
    delete: null,
    directory: null,
    installProviderCli: null,
    list: null,
    pathsExist: null,
    pickFolder: null,
    providerCliStatus: null,
    retryUpdate: null,
    update: null,
  },
  plugins: {
    callRpc: null,
    list: null,
  },
  projects: {
    attachments: {
      copy: null,
      read: null,
      upload: null,
    },
    branches: null,
    commands: null,
    create: null,
    defaultExecutionOptions: null,
    delete: null,
    fileContent: null,
    files: null,
    get: null,
    list: null,
    paths: null,
    promptHistory: null,
    reorder: null,
    sources: {
      add: null,
      delete: null,
      list: null,
      update: null,
    },
    update: null,
  },
  providers: {
    list: null,
  },
  skills: {
    getContent: null,
    list: null,
    listFiles: null,
    registry: {
      detail: null,
      entries: null,
      get: null,
      repositoryStars: null,
      search: null,
    },
    remove: null,
  },
  status: {
    get: null,
  },
  system: {
    cliSkillsStatus: null,
    config: null,
    executionOptions: null,
    installCliSkills: null,
    providerStates: null,
    uiPreferences: {
      list: null,
      set: null,
    },
    updateExperiments: null,
    updateGeneralSettings: null,
    updateKeyboardSettings: null,
    usageLimits: null,
    version: null,
  },
  terminals: {
    close: null,
    create: null,
    list: null,
    rename: null,
  },
  theme: {
    resolve: null,
    set: null,
  },
  threads: {
    archiveAll: null,
    cancelPlan: null,
    childSummary: null,
    clearGoal: null,
    conversationOutline: null,
    defaultExecutionOptions: null,
    delete: null,
    editMessage: null,
    get: null,
    interactions: {
      cancel: null,
      list: null,
      resolve: null,
      respond: null,
    },
    list: null,
    markRead: null,
    markUnread: null,
    pin: null,
    promptHistory: null,
    queuedMessages: {
      create: null,
      delete: null,
      list: null,
      reorder: null,
      send: null,
      setGroupBoundary: null,
      update: null,
    },
    reorderPinned: null,
    resolveMentions: null,
    search: null,
    send: null,
    spawn: null,
    stop: null,
    storageFiles: null,
    storageLocation: null,
    storagePaths: null,
    tabs: {
      get: null,
      update: null,
    },
    timeline: null,
    timelineTurnSummaryDetails: null,
    unarchive: null,
    unpin: null,
    update: null,
  },
  threadSections: {
    create: null,
    delete: null,
    update: null,
  },
} as const;

type BrowserSdkNode<T> = {
  readonly [K in keyof T]: T[K] extends null
    ? UnavailableMethod
    : BrowserSdkNode<T[K]>;
};

export type BrowserBbSdk = BrowserSdkNode<typeof BROWSER_SDK_SURFACE>;

export class BrowserSdkUnavailableError extends Error {
  readonly code = "browser_sdk_unavailable";

  constructor(readonly operation: string) {
    super(`Browser SDK operation is not wired to loom yet: ${operation}`);
    this.name = "BrowserSdkUnavailableError";
  }
}

function materializeNode(
  definition: Record<string, unknown>,
  prefix: string[] = [],
): Record<string, unknown> {
  return Object.fromEntries(
    Object.entries(definition).map(([name, child]) => {
      const operation = [...prefix, name];
      if (child === null) {
        const unavailable: UnavailableMethod = async () => {
          throw new BrowserSdkUnavailableError(operation.join("."));
        };
        return [name, unavailable];
      }
      return [
        name,
        materializeNode(child as Record<string, unknown>, operation),
      ];
    }),
  );
}

export function createBrowserBbSdk(
  _args: CreateBrowserBbSdkArgs = {},
): BrowserBbSdk {
  return materializeNode(BROWSER_SDK_SURFACE) as BrowserBbSdk;
}

export const bb = createBrowserBbSdk();

export { BbHttpError, BbRequestTimeoutError };
export type { BbHttpErrorArgs } from "./response.js";

import { createBrowserBbSdk } from "@bb/sdk/browser";
import { fetchWithAppSurface } from "./app-surface";
import {
  loomDeleteThread,
  loomGetEnvironment,
  loomGetThread,
  loomGetThreadTabs,
  loomGetThreadTimeline,
  loomListEnvironmentProviders,
  loomMarkThreadRead,
  loomMarkThreadUnread,
  loomPinThread,
  loomProjectDefaultExecutionOptions,
  loomReorderPinnedThread,
  loomSendThreadMessage,
  loomSpawnThread,
  loomStopThread,
  loomThreadChildSummary,
  loomThreadDefaultExecutionOptions,
  loomUnpinThread,
  loomUpdateThreadTabs,
} from "./loom-thread-runtime";
import {
  loomCancelThreadInteraction,
  loomGetThreadInteraction,
  loomListThreadInteractions,
  loomResolveThreadInteraction,
} from "./loom-interactions";
import {
  loomThreadStorageFiles,
  loomThreadStorageLocation,
  loomThreadStoragePaths,
} from "./loom-thread-storage";
import {
  loomCreateQueuedMessage,
  loomDeleteQueuedMessage,
  loomListQueuedMessages,
  loomReorderQueuedMessage,
  loomSendQueuedMessage,
  loomSetQueuedMessageGroupBoundary,
  loomUpdateQueuedMessage,
} from "./loom-thread-queue";
import { loomDeleteHost } from "./loom-host-mutations";
import { loomHostDirectory } from "./loom-host-readers";
import {
  loomAddProjectSource,
  loomCreateProject,
  loomDeleteProject,
  loomDeleteProjectSource,
  loomReorderProject,
  loomUpdateProject,
  loomUpdateProjectSource,
} from "./loom-project-mutations";
import { loomUpdateGeneralSettings } from "./loom-settings-mutations";
import {
  loomCloseTerminal,
  loomCreateTerminal,
  loomListTerminals,
  loomRenameTerminal,
} from "./loom-terminals";
import {
  loomCreateThreadSection,
  loomDeleteThreadSection,
  loomUpdateThreadSection,
} from "./loom-thread-sections";

import {
  loomResetUiPreference,
  loomListUiPreferences,
  loomSetUiPreference,
} from "./loom-ui-preferences";

const BASE_URL =
  typeof window === "undefined" ? "http://localhost" : window.location.origin;

const compileOnlySdk = createBrowserBbSdk({
  baseUrl: BASE_URL,
  fetch: fetchWithAppSurface,
});

/**
 * The browser SDK remains fail-closed for the bb surfaces this fork does not
 * implement. The operations the product app actually issues — the project,
 * thread, section, queue and terminal writes among them — are overridden with
 * loom-native implementations. Object-spread keeps every other method as
 * BrowserSdkUnavailableError while preserving the concrete request/response
 * types of these overrides at their call sites.
 */
export const sdk = {
  ...compileOnlySdk,
  environments: {
    ...compileOnlySdk.environments,
    get: loomGetEnvironment,
    listProviders: loomListEnvironmentProviders,
  },
  hosts: {
    ...compileOnlySdk.hosts,
    delete: loomDeleteHost,
    directory: loomHostDirectory,
  },
  projects: {
    ...compileOnlySdk.projects,
    create: loomCreateProject,
    defaultExecutionOptions: loomProjectDefaultExecutionOptions,
    delete: loomDeleteProject,
    reorder: loomReorderProject,
    sources: {
      ...compileOnlySdk.projects.sources,
      add: loomAddProjectSource,
      delete: loomDeleteProjectSource,
      update: loomUpdateProjectSource,
    },
    update: loomUpdateProject,
  },
  system: {
    ...compileOnlySdk.system,
    uiPreferences: {
      ...compileOnlySdk.system.uiPreferences,
      list: loomListUiPreferences,
      reset: loomResetUiPreference,
      set: loomSetUiPreference,
    },
    updateGeneralSettings: loomUpdateGeneralSettings,
  },
  terminals: {
    ...compileOnlySdk.terminals,
    close: loomCloseTerminal,
    create: loomCreateTerminal,
    list: loomListTerminals,
    rename: loomRenameTerminal,
  },
  threads: {
    ...compileOnlySdk.threads,
    childSummary: loomThreadChildSummary,
    defaultExecutionOptions: loomThreadDefaultExecutionOptions,
    delete: loomDeleteThread,
    get: loomGetThread,
    interactions: {
      ...compileOnlySdk.threads.interactions,
      cancel: loomCancelThreadInteraction,
      get: loomGetThreadInteraction,
      list: loomListThreadInteractions,
      resolve: loomResolveThreadInteraction,
    },
    markRead: loomMarkThreadRead,
    markUnread: loomMarkThreadUnread,
    pin: loomPinThread,
    queuedMessages: {
      ...compileOnlySdk.threads.queuedMessages,
      create: loomCreateQueuedMessage,
      delete: loomDeleteQueuedMessage,
      list: loomListQueuedMessages,
      reorder: loomReorderQueuedMessage,
      send: loomSendQueuedMessage,
      setGroupBoundary: loomSetQueuedMessageGroupBoundary,
      update: loomUpdateQueuedMessage,
    },
    reorderPinned: loomReorderPinnedThread,
    send: loomSendThreadMessage,
    spawn: loomSpawnThread,
    stop: loomStopThread,
    storageFiles: loomThreadStorageFiles,
    storageLocation: loomThreadStorageLocation,
    storagePaths: loomThreadStoragePaths,
    tabs: {
      ...compileOnlySdk.threads.tabs,
      get: loomGetThreadTabs,
      update: loomUpdateThreadTabs,
    },
    timeline: loomGetThreadTimeline,
    unpin: loomUnpinThread,
  },
  threadSections: {
    ...compileOnlySdk.threadSections,
    create: loomCreateThreadSection,
    delete: loomDeleteThreadSection,
    update: loomUpdateThreadSection,
  },
};

export { BbHttpError } from "@bb/sdk/browser";

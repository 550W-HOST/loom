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
import { loomDeleteHost } from "./loom-host-mutations";
import { loomHostDirectory } from "./loom-host-readers";
import { loomCreateProject, loomDeleteProject } from "./loom-project-mutations";
import { loomUpdateGeneralSettings } from "./loom-settings-mutations";

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
 * The browser SDK remains fail-closed except for the narrow loom-native
 * operations required by the New Thread success path. Object-spread keeps all
 * other methods as BrowserSdkUnavailableError while preserving the concrete
 * request/response types of these overrides at their call sites.
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
    reorderPinned: loomReorderPinnedThread,
    send: loomSendThreadMessage,
    spawn: loomSpawnThread,
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
};

export { BbHttpError } from "@bb/sdk/browser";

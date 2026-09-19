import { createBrowserBbSdk } from "@bb/sdk/browser";
import { fetchWithAppSurface } from "./app-surface";
import {
  loomGetEnvironment,
  loomGetThread,
  loomGetThreadTabs,
  loomGetThreadTimeline,
  loomListEnvironmentProviders,
  loomMarkThreadRead,
  loomMarkThreadUnread,
  loomProjectDefaultExecutionOptions,
  loomSendThreadMessage,
  loomSpawnThread,
  loomThreadDefaultExecutionOptions,
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
  projects: {
    ...compileOnlySdk.projects,
    defaultExecutionOptions: loomProjectDefaultExecutionOptions,
  },
  system: {
    ...compileOnlySdk.system,
    uiPreferences: {
      ...compileOnlySdk.system.uiPreferences,
      list: loomListUiPreferences,
      reset: loomResetUiPreference,
      set: loomSetUiPreference,
    },
  },
  threads: {
    ...compileOnlySdk.threads,
    defaultExecutionOptions: loomThreadDefaultExecutionOptions,
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
  },
};

export { BbHttpError } from "@bb/sdk/browser";

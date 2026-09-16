import { createBrowserBbSdk } from "@bb/sdk/browser";
import { fetchWithAppSurface } from "./app-surface";
import {
  loomGetEnvironment,
  loomGetThread,
  loomGetThreadTimeline,
  loomListEnvironmentProviders,
  loomProjectDefaultExecutionOptions,
  loomSendThreadMessage,
  loomSpawnThread,
} from "./loom-thread-runtime";

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
  threads: {
    ...compileOnlySdk.threads,
    get: loomGetThread,
    send: loomSendThreadMessage,
    spawn: loomSpawnThread,
    timeline: loomGetThreadTimeline,
  },
};

export { BbHttpError } from "@bb/sdk/browser";

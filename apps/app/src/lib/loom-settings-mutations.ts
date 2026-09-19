import type { AppSettings, AppSettingsUpdate } from "@bb/domain";
import { loomApiJson } from "@/lib/loom-http";

/**
 * The Settings mutations the product app issues over loom.
 *
 * `system.updateGeneralSettings` was still the fail-closed browser SDK stub, so
 * saving the General settings pane threw `BrowserSdkUnavailableError` instead of
 * reaching the server. The contract route (`system.generalSettings`, a `PUT` to
 * `/settings/general`) and its server handler already exist, so this is the app
 * half only.
 *
 * It follows the loom-native writer pattern (`loom-host-mutations.ts`,
 * `loom-ui-preferences.ts`): a contract `PUT` with its typed JSON body, wired
 * into `src/lib/sdk.ts`.
 */

/**
 * The contract response is the stored settings plus loom's
 * `showUnhandledProviderEvents` flag, which matches the SDK area's own
 * `SystemUpdateGeneralSettingsResult`.
 */
export type LoomUpdateGeneralSettingsResult = AppSettings & {
  showUnhandledProviderEvents?: boolean;
};

export function loomUpdateGeneralSettings(
  settings: AppSettingsUpdate,
): Promise<LoomUpdateGeneralSettingsResult> {
  return loomApiJson("system.generalSettings", { json: settings });
}

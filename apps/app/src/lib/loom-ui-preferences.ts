import type {
  UiPreferenceKey,
  UiPreferenceValue,
} from "@bb/domain";
import type {
  UiPreferenceResponse,
  UiPreferencesResponse,
} from "@bb/server-contract";
import { loomApiJson } from "./loom-http";

export function loomListUiPreferences(args?: {
  signal?: AbortSignal;
}): Promise<UiPreferencesResponse> {
  return loomApiJson("system.uiPreferences", {
    signal: args?.signal,
  });
}

export async function loomSetUiPreference<Key extends UiPreferenceKey>(args: {
  expectedRevision: number;
  key: Key;
  value: UiPreferenceValue<Key>;
}): Promise<UiPreferenceResponse<Key>> {
  const response = await loomApiJson("system.updateUiPreference", {
    param: { key: args.key },
    json: {
      expectedRevision: args.expectedRevision,
      value: args.value,
    },
  });
  return response as UiPreferenceResponse<Key>;
}

export async function loomResetUiPreference<Key extends UiPreferenceKey>(args: {
  key: Key;
}): Promise<UiPreferenceResponse<Key>> {
  const response = await loomApiJson("system.resetUiPreference", {
    param: { key: args.key },
  });
  return response as UiPreferenceResponse<Key>;
}

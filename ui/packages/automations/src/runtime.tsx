import {
  createContext,
  useContext,
  useMemo,
  type ComponentType,
  type ReactNode,
} from "react";
import type { AutomationsClient } from "./client.js";
import type {
  PermissionMode,
  ReasoningLevel,
  ServiceTier,
} from "./rpc-types.js";

export interface AutomationProviderModelValue {
  providerId: string;
  model: string;
  reasoningLevel: ReasoningLevel;
  serviceTier?: ServiceTier;
}

export type AutomationProviderModelRouting =
  | { kind: "environment"; environmentId: string }
  | { kind: "host"; hostId: string };

export interface AutomationProviderModelPickerProps {
  value: AutomationProviderModelValue;
  onChange: (value: AutomationProviderModelValue) => void;
  routing?: AutomationProviderModelRouting;
  allowProviderChange?: boolean;
  disabled?: boolean;
  className?: string;
}

export interface AutomationPermissionModePickerProps {
  providerId: string;
  value: PermissionMode;
  onChange: (value: PermissionMode) => void;
  routing?: AutomationProviderModelRouting;
  disabled?: boolean;
  className?: string;
}

export interface AutomationEditorAdapters {
  ProviderModelPicker: ComponentType<AutomationProviderModelPickerProps>;
  PermissionModePicker: ComponentType<AutomationPermissionModePickerProps>;
}

export interface AutomationsNavigation {
  toCompose(args: { focusPrompt: boolean; initialPrompt: string }): void;
  toThread(threadId: string): void;
  toPanel(subPath: string): void;
}

interface AutomationsRuntime {
  client: AutomationsClient;
  navigation: AutomationsNavigation;
  editorAdapters: AutomationEditorAdapters;
}

function UnavailableProviderModelPicker({
  value,
  className,
}: AutomationProviderModelPickerProps) {
  return (
    <button
      type="button"
      disabled
      className={className}
      title="Provider selection is unavailable until loom automation APIs are connected"
    >
      {value.providerId} / {value.model}
    </button>
  );
}

function UnavailablePermissionModePicker({
  value,
  className,
}: AutomationPermissionModePickerProps) {
  return (
    <button
      type="button"
      disabled
      className={className}
      title="Permission selection is unavailable until loom automation APIs are connected"
    >
      {value}
    </button>
  );
}

const defaultEditorAdapters: AutomationEditorAdapters = {
  ProviderModelPicker: UnavailableProviderModelPicker,
  PermissionModePicker: UnavailablePermissionModePicker,
};

const RuntimeContext = createContext<AutomationsRuntime | null>(null);

export function AutomationsRuntimeProvider({
  client,
  navigation,
  editorAdapters = defaultEditorAdapters,
  children,
}: {
  client: AutomationsClient;
  navigation: AutomationsNavigation;
  editorAdapters?: AutomationEditorAdapters;
  children: ReactNode;
}) {
  const value = useMemo(
    () => ({ client, navigation, editorAdapters }),
    [client, navigation, editorAdapters],
  );
  return (
    <RuntimeContext.Provider value={value}>{children}</RuntimeContext.Provider>
  );
}

function useAutomationsRuntime(): AutomationsRuntime {
  const runtime = useContext(RuntimeContext);
  if (runtime === null) {
    throw new Error("Automations UI requires AutomationsRuntimeProvider");
  }
  return runtime;
}

export function useAutomationsClient(): AutomationsClient {
  return useAutomationsRuntime().client;
}

export function useAutomationsNavigation(): AutomationsNavigation {
  return useAutomationsRuntime().navigation;
}

export function ProviderModelPicker(
  props: AutomationProviderModelPickerProps,
) {
  const Picker = useAutomationsRuntime().editorAdapters.ProviderModelPicker;
  return <Picker {...props} />;
}

export function PermissionModePicker(
  props: AutomationPermissionModePickerProps,
) {
  const Picker = useAutomationsRuntime().editorAdapters.PermissionModePicker;
  return <Picker {...props} />;
}

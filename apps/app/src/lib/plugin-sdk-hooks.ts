import { useMemo } from "react";
import {
  useOptionalPluginComposerView,
  usePluginComposerHost,
  type ComposerView,
} from "@/components/plugin/plugin-composer-host";

export function isAutomationEditRoutePath(): boolean {
  return false;
}

export function getComposerInputLock(_storageKey: string | null): boolean {
  return false;
}

export function useComposerInputLock(_storageKey: string | null): boolean {
  return false;
}

export function useBbContext() {
  return { projectId: null, threadId: null };
}

export function useBbNavigate(): any {
  return useMemo(
    () => ({
      toThread() {},
      toProject() {},
      toPluginPanel() {},
      toCompose() {},
      openThreadPanel() {
        return false;
      },
      openUrl() {
        return false;
      },
      experimental_openUrl() {
        return false;
      },
      experimental_openFilePreview() {
        return false;
      },
      experimental_openFileExternally() {
        return false;
      },
      experimental_openAppPanel() {
        return false;
      },
    }),
    [],
  );
}

export function useComposer(): any {
  const host = usePluginComposerHost();
  return useMemo(
    () => ({
      focus: () => host?.focus(),
      getDraft: () => host?.getCurrent() ?? null,
      setDraft: (draft: any) => host?.setDraft(draft),
      submit: (options: { sendAt: number }) => host?.submit?.(options),
      setInputLock() {},
      setTextEffect() {},
    }),
    [host],
  );
}

export function useComposerView(): ComposerView | undefined {
  return useOptionalPluginComposerView();
}

export function useRpc(): any {
  return useMemo(
    () => ({
      call(method: string): Promise<never> {
        return Promise.reject(
          new Error(`Generic plugin RPC is unavailable: ${method}`),
        );
      },
    }),
    [],
  );
}

export function useRealtime(): void {}

export function useRealtimeConnectionState(): "disconnected" {
  return "disconnected";
}

export function useSettings() {
  return { values: undefined, isLoading: false };
}

export function useProviders() {
  return { status: "ready" as const, providers: [] };
}

export function useAppPanel(): null {
  return null;
}

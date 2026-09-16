export interface PluginComposerThreadRowStatus {
  tone: "running" | "success" | "error" | "muted";
  icon: string;
  label: string;
}

export function getPluginThreadRowStatus(
  _threadId: string,
): PluginComposerThreadRowStatus | null {
  return null;
}

export function setPluginThreadRowStatus(
  _threadId: string | null,
  _pluginId: string,
  _status: PluginComposerThreadRowStatus | null,
  _owner?: string | symbol,
): void {}

export function clearPluginThreadRowStatuses(_pluginId: string): void {}

export function clearPluginThreadRowStatusesByOwner(
  _owner: string | symbol,
): void {}

export function subscribePluginThreadRowStatus(
  _threadId: string,
  _listener: () => void,
): () => void {
  return () => {};
}

export function usePluginThreadRowStatus(
  _threadId: string,
): PluginComposerThreadRowStatus | null {
  return null;
}

export function usePluginThreadRowStatusForThreads(
  _threads: readonly { id: string }[],
): PluginComposerThreadRowStatus | null {
  return null;
}

export function resetPluginThreadRowStatusesForTest(): void {}

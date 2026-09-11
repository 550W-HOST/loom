export interface RelayScope {
  kind: string;
  id?: string;
}

export interface RelayEventFrame {
  type: "event";
  event_id: string;
  scope: RelayScope;
  payload: string;
  created_at_ms: number;
}

export interface ReplayPage {
  frames: unknown[];
  has_more: boolean;
}

export type LoomThreadStatus = "idle" | "working" | "waiting" | "error" | "archived";

export interface LoomThread {
  id: string;
  project_id: string;
  environment_id: string | null;
  parent_thread_id: string | null;
  title: string | null;
  status: LoomThreadStatus;
  created_at_ms: number;
  updated_at_ms: number;
  active_run_id?: string;
}

export interface LoomThreadsResponse {
  threads: LoomThread[];
}

export interface LoomCreateThreadResponse {
  thread: LoomThread;
  event_id: string;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function isRelayEventFrame(value: unknown): value is RelayEventFrame {
  return (
    isRecord(value) &&
    value.type === "event" &&
    typeof value.event_id === "string" &&
    isRecord(value.scope) &&
    typeof value.scope.kind === "string" &&
    typeof value.payload === "string" &&
    typeof value.created_at_ms === "number"
  );
}

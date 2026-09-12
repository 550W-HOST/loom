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

/**
 * A thread as bb's contract types it (`threadResponseSchema` / the list row
 * variant). The server projects the domain record into this shape, so the UI
 * reads camelCase exactly like any other bb client.
 */
export interface LoomThread {
  id: string;
  projectId: string;
  environmentId: string | null;
  parentThreadId: string | null;
  title: string | null;
  status: LoomThreadStatus;
  createdAt: number;
  updatedAt: number;
}

/** `GET /api/v1/threads` returns a bare array of list rows. */
export type LoomThreadsResponse = LoomThread[];

export type LoomProjectKind = "standard" | "personal";

export interface LoomProjectSource {
  id: string;
  projectId: string;
  hostId: string;
  path: string;
  isDefault: boolean;
  createdAt: number;
  updatedAt: number;
  type: string;
}

export interface LoomProject {
  id: string;
  kind: LoomProjectKind;
  name: string;
  gitRemoteUrl: string | null;
  sources: LoomProjectSource[];
  createdAt: number;
  updatedAt: number;
}

/** `GET /api/v1/projects` returns a bare array of `projectSchema`. */
export type LoomProjectsResponse = LoomProject[];

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

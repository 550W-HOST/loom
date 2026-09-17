import type { ChangedMessage, RealtimeSubscriptionTarget } from "@bb/server-contract";
import type {
  AutomationOperation,
  AutomationOperationMap,
  AutomationSignal,
  AutomationsClient,
} from "bb-plugin-automations/client";
import { loomNativeJson } from "./loom-http";
import { wsManager } from "./ws";

/**
 * The Automations panel's client, wired to loom-server's own routes.
 *
 * The pinned panel (`bb-plugin-automations`) takes an `AutomationsClient` and
 * nothing else: the ten operations by name, and a signal subscription it uses
 * to refetch what a mutation changed. W-610 shipped it against an
 * "unavailable" client that rejected every call, so the panel rendered its
 * empty state; this is the real implementation behind the same interface.
 *
 * The routes are **loom-native**: automations are a product surface this fork
 * added on top of bb's API, so they are not in bb's exported contract and have
 * no entry in the contract route table (`loom-api-routes.ts`). That is why this
 * module builds its own paths and goes through `loomNativeJson` — the same
 * transport, same-origin and same error mapping, without claiming a contract
 * route that does not exist.
 */

/** The loom-native mount every automations route lives under. */
const AUTOMATIONS_MOUNT = "/api/v1";

/** The panel's own target: automations belong to a project, and a project
 * change is what tells a client to refetch them. */
const PROJECT_LIST_TARGET = {
  kind: "project-list",
} satisfies RealtimeSubscriptionTarget;

function automationsPath(projectId: string): string {
  return `${AUTOMATIONS_MOUNT}/projects/${encodeURIComponent(projectId)}/automations`;
}

function automationPath(projectId: string, automationId: string): string {
  return `${automationsPath(projectId)}/${encodeURIComponent(automationId)}`;
}

/**
 * The kinds the panel's signal carries.
 *
 * The server publishes one frame per project change — its public vocabulary has
 * no automation entity (see `docs/automations.md` § Invalidation) — and the
 * panel filters on these two kinds: the list and detail views refetch on
 * either, and the run history only on `automation-runs-changed`. A coarse frame
 * therefore has to be reported as both, or a settled run would never reach the
 * history a user is looking at.
 */
const SIGNAL_KINDS = ["automations-changed", "automation-runs-changed"] as const;

/** A project change from the public socket, as the panel's signal. */
function signalsFor(message: ChangedMessage): AutomationSignal[] {
  if (message.entity !== "project") {
    return [];
  }
  const projectId = message.id;
  if (typeof projectId !== "string" || projectId.length === 0) {
    return [];
  }
  return SIGNAL_KINDS.map((kind) => ({ projectId, kind }));
}

export function createLoomAutomationsClient(): AutomationsClient {
  const listeners = new Set<(signal: AutomationSignal) => void>();
  /** The projects this client has been asked about, so a reconnect can tell
   * their views to refetch. */
  const servedProjects = new Set<string>();
  let teardown: (() => void) | null = null;

  function emit(signal: AutomationSignal): void {
    for (const listener of listeners) {
      listener(signal);
    }
  }

  function subscribe(listener: (signal: AutomationSignal) => void): () => void {
    listeners.add(listener);
    if (teardown === null) {
      // One subscription per client, not per listener: the socket target is
      // refcounted in the manager, and this client is one consumer of it.
      const unsubscribeChanged = wsManager.onChanged((message) => {
        for (const signal of signalsFor(message)) {
          emit(signal);
        }
      });
      // A reconnect is a gap: whatever the panel holds may have moved while the
      // socket was down, so the projects it was reading are re-announced. The
      // app's own cache effects do the same for the queries they own.
      const unsubscribeConnected = wsManager.onConnected(() => {
        for (const projectId of servedProjects) {
          for (const kind of SIGNAL_KINDS) {
            emit({ projectId, kind });
          }
        }
      });
      wsManager.subscribe(PROJECT_LIST_TARGET);
      teardown = () => {
        unsubscribeChanged();
        unsubscribeConnected();
        wsManager.unsubscribe(PROJECT_LIST_TARGET);
      };
    }
    return () => {
      listeners.delete(listener);
      if (listeners.size === 0 && teardown !== null) {
        teardown();
        teardown = null;
      }
    };
  }

  async function call<M extends AutomationOperation>(
    method: M,
    ...args: AutomationOperationMap[M]["input"] extends null
      ? [input?: null]
      : [input: AutomationOperationMap[M]["input"]]
  ): Promise<AutomationOperationMap[M]["output"]> {
    const input = (args[0] ?? null) as Record<string, unknown> | null;
    const projectId = typeof input?.["projectId"] === "string" ? input["projectId"] : null;
    const automationId =
      typeof input?.["automationId"] === "string" ? input["automationId"] : null;
    if (projectId !== null) {
      servedProjects.add(projectId);
    }
    switch (method) {
      case "automations_overview":
        return loomNativeJson(`${AUTOMATIONS_MOUNT}/automations`);
      case "automations_list":
        return loomNativeJson(automationsPath(requireId(projectId, "projectId")));
      case "automations_get":
        return loomNativeJson(
          automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId")),
        );
      case "automations_create": {
        const { projectId: _projectId, ...body } = input ?? {};
        return loomNativeJson(automationsPath(requireId(projectId, "projectId")), {
          method: "POST",
          json: body,
        });
      }
      case "automations_update": {
        const { projectId: _projectId, automationId: _automationId, ...patch } = input ?? {};
        return loomNativeJson(
          automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId")),
          { method: "PATCH", json: patch },
        );
      }
      case "automations_delete":
        return loomNativeJson(
          automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId")),
          { method: "DELETE" },
        );
      case "automations_pause":
        return loomNativeJson(
          `${automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId"))}/pause`,
          { method: "POST" },
        );
      case "automations_resume":
        return loomNativeJson(
          `${automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId"))}/resume`,
          { method: "POST" },
        );
      case "automations_run": {
        const idempotencyKey = input?.["idempotencyKey"];
        return loomNativeJson(
          `${automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId"))}/run`,
          {
            method: "POST",
            json: typeof idempotencyKey === "string" ? { idempotencyKey } : {},
          },
        );
      }
      case "automations_runs": {
        const limit = input?.["limit"];
        const cursor = input?.["cursor"];
        const query = new URLSearchParams();
        if (typeof limit === "number") {
          query.set("limit", String(limit));
        }
        if (typeof cursor === "string" && cursor.length > 0) {
          query.set("cursor", cursor);
        }
        const suffix = query.size === 0 ? "" : `?${query.toString()}`;
        return loomNativeJson(
          `${automationPath(requireId(projectId, "projectId"), requireId(automationId, "automationId"))}/runs${suffix}`,
        );
      }
      default: {
        // A new operation in the pinned map is a compile error here rather than
        // a runtime rejection, which is what keeps "unavailable" from coming
        // back quietly.
        const exhaustive: never = method;
        return Promise.reject(new Error(`unhandled automations operation: ${String(exhaustive)}`));
      }
    }
  }

  return {
    call: call as AutomationsClient["call"],
    subscribe,
  };
}

function requireId(value: string | null, name: string): string {
  if (value === null || value.length === 0) {
    throw new Error(`automations operation requires ${name}`);
  }
  return value;
}

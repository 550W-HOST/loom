import type { z } from "zod";
import type {
  automationListResponseSchema,
  automationReadResultSchema,
  automationResponseSchema,
  automationRunListResponseSchema,
  automationRunRpcResponseSchema,
  automationRunsInputSchema,
  automationsOverviewResponseSchema,
  createAutomationInputSchema,
  listAutomationsInputSchema,
  projectAutomationInputSchema,
  runAutomationInputSchema,
  updateAutomationInputSchema,
} from "./rpc-types.js";

export interface AutomationOperationMap {
  automations_overview: {
    input: null;
    output: z.output<typeof automationsOverviewResponseSchema>;
  };
  automations_list: {
    input: z.input<typeof listAutomationsInputSchema>;
    output: z.output<typeof automationListResponseSchema>;
  };
  automations_get: {
    input: z.input<typeof projectAutomationInputSchema>;
    output: z.output<typeof automationReadResultSchema>;
  };
  automations_create: {
    input: z.input<typeof createAutomationInputSchema>;
    output: z.output<typeof automationResponseSchema>;
  };
  automations_update: {
    input: z.input<typeof updateAutomationInputSchema>;
    output: z.output<typeof automationResponseSchema>;
  };
  automations_delete: {
    input: z.input<typeof projectAutomationInputSchema>;
    output: { ok: true };
  };
  automations_pause: {
    input: z.input<typeof projectAutomationInputSchema>;
    output: z.output<typeof automationResponseSchema>;
  };
  automations_resume: {
    input: z.input<typeof projectAutomationInputSchema>;
    output: z.output<typeof automationResponseSchema>;
  };
  automations_run: {
    input: z.input<typeof runAutomationInputSchema>;
    output: z.output<typeof automationRunRpcResponseSchema>;
  };
  automations_runs: {
    input: z.input<typeof automationRunsInputSchema>;
    output: z.output<typeof automationRunListResponseSchema>;
  };
}

export type AutomationOperation = keyof AutomationOperationMap;
export type AutomationSignalKind =
  | "automations-changed"
  | "automation-runs-changed";

export interface AutomationSignal {
  projectId: string;
  kind: AutomationSignalKind;
}

type AutomationCallArgs<M extends AutomationOperation> =
  AutomationOperationMap[M]["input"] extends null
    ? [input?: null]
    : [input: AutomationOperationMap[M]["input"]];

export interface AutomationsClient {
  call<M extends AutomationOperation>(
    method: M,
    ...args: AutomationCallArgs<M>
  ): Promise<AutomationOperationMap[M]["output"]>;
  subscribe(listener: (signal: AutomationSignal) => void): () => void;
}

export class AutomationsUnavailableError extends Error {
  readonly code = "automations_unavailable";

  constructor(readonly operation: AutomationOperation) {
    super(`Automations operation is not wired to loom yet: ${operation}`);
    this.name = "AutomationsUnavailableError";
  }
}

export function createUnavailableAutomationsClient(): AutomationsClient {
  return {
    call<M extends AutomationOperation>(
      method: M,
      ..._args: AutomationCallArgs<M>
    ): Promise<AutomationOperationMap[M]["output"]> {
      return Promise.reject(new AutomationsUnavailableError(method));
    },
    subscribe(): () => void {
      return () => {};
    },
  };
}

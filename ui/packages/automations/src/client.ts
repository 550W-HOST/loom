import type {
  AutomationListResponse,
  AutomationReadResult,
  AutomationResponse,
  AutomationRunListResponse,
  AutomationRunRpcResponse,
  AutomationRunsInput,
  AutomationsOverviewResponse,
  CreateAutomationInput,
  ListAutomationsInput,
  ProjectAutomationInput,
  RunAutomationInput,
  UpdateAutomationRequest,
} from "./rpc-types.js";

export interface AutomationOperationMap {
  automations_overview: {
    input: null;
    output: AutomationsOverviewResponse;
  };
  automations_list: {
    input: ListAutomationsInput;
    output: AutomationListResponse;
  };
  automations_get: {
    input: ProjectAutomationInput;
    output: AutomationReadResult;
  };
  automations_create: {
    input: CreateAutomationInput;
    output: AutomationResponse;
  };
  automations_update: {
    input: UpdateAutomationRequest;
    output: AutomationResponse;
  };
  automations_delete: {
    input: ProjectAutomationInput;
    output: { ok: true };
  };
  automations_pause: {
    input: ProjectAutomationInput;
    output: AutomationResponse;
  };
  automations_resume: {
    input: ProjectAutomationInput;
    output: AutomationResponse;
  };
  automations_run: {
    input: RunAutomationInput;
    output: AutomationRunRpcResponse;
  };
  automations_runs: {
    input: AutomationRunsInput;
    output: AutomationRunListResponse;
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

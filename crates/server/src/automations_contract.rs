//! The automations contract, as schemas.
//!
//! bb's exported contract has no automations entries, so these schemas are
//! loom's own: they mirror `ui/packages/automations/src/rpc-types.ts`, the
//! pinned UI tree W-610 imported, and they are the oracle in **both**
//! directions. [`validate_request`] runs on the way in — the write routes are
//! strict, so a key the contract does not name is a `422` exactly as it is for
//! a bb route — and the conformance tests validate responses against the same
//! objects on the way out.
//!
//! They are written by hand on purpose. Deriving them from the serde types
//! would put the strictness in the wrong place twice over: serde's
//! `deny_unknown_fields` cannot express `.strict()` for an internally tagged
//! union (it does not reach a unit variant), and the *stored* row has to stay
//! tolerant of a field a newer build added — a row this build cannot read is
//! reported as invalid stored data, which is a worse outcome for an additive
//! field than reading it and ignoring it.
//!
//! The validator is the same one bb's routes use (`loom_contract`), so the
//! supported keywords are exactly the subset it implements.

use serde_json::{json, Value};

/// A violation rendered the way an API error message needs it.
pub fn describe(violations: &[loom_contract::Violation]) -> String {
    loom_contract::describe(violations)
}

/// Validates a request body against the schema for one write operation.
pub fn validate_request(
    operation: WriteOperation,
    instance: &Value,
) -> Vec<loom_contract::Violation> {
    loom_contract::validate(&json!({}), &request_schema(operation), instance)
}

/// Validates a response body against one of the contract's response schemas.
pub fn validate_response(schema: &Value, instance: &Value) -> Vec<loom_contract::Violation> {
    loom_contract::validate(&json!({}), schema, instance)
}

/// The write operations whose body the contract describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOperation {
    /// `POST /api/v1/projects/{id}/automations`
    Create,
    /// `PATCH /api/v1/projects/{id}/automations/{automationId}`
    Update,
    /// `POST /api/v1/projects/{id}/automations/{automationId}/run`
    Run,
}

/// Matches a request against the write routes that carry a JSON body.
///
/// Paths are matched segment by segment rather than by pattern, because the
/// router's patterns are what they are: the project id and the automation id
/// are opaque here.
pub fn write_operation(method: &axum::http::Method, path: &str) -> Option<WriteOperation> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method.as_str(), segments.as_slice()) {
        ("POST", ["api", "v1", "projects", _, "automations"]) => Some(WriteOperation::Create),
        ("PATCH", ["api", "v1", "projects", _, "automations", _]) => Some(WriteOperation::Update),
        ("POST", ["api", "v1", "projects", _, "automations", _, "run"]) => {
            Some(WriteOperation::Run)
        }
        _ => None,
    }
}

/// The request schema for one write operation.
pub fn request_schema(operation: WriteOperation) -> Value {
    match operation {
        WriteOperation::Create => create_request(),
        WriteOperation::Update => update_request(),
        WriteOperation::Run => run_request(),
    }
}

fn string(min: u64) -> Value {
    json!({ "type": "string", "minLength": min })
}

pub fn branch_spec() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "name"],
                "properties": { "kind": { "const": "existing" }, "name": string(1) }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "baseBranch"],
                "properties": { "kind": { "const": "new" }, "baseBranch": string(1) }
            }
        ]
    })
}

pub fn workspace() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "path"],
                "properties": {
                    "type": { "const": "unmanaged" },
                    "path": { "type": ["string", "null"] },
                    "branch": branch_spec()
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "baseBranch"],
                "properties": {
                    "type": { "const": "managed-worktree" },
                    "baseBranch": {
                        "oneOf": [
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["kind", "name"],
                                "properties": { "kind": { "const": "named" }, "name": string(1) }
                            },
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["kind"],
                                "properties": { "kind": { "const": "default" } }
                            }
                        ]
                    }
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["type"],
                "properties": { "type": { "const": "personal" } }
            }
        ]
    })
}

pub fn environment() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "environmentId"],
                "properties": { "type": { "const": "reuse" }, "environmentId": string(1) }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "workspace"],
                "properties": {
                    "type": { "const": "host" },
                    "hostId": string(1),
                    "workspace": workspace()
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["type"],
                "properties": { "type": { "const": "project-default" } }
            }
        ]
    })
}

pub fn trigger() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["triggerType", "cron", "timezone"],
                "properties": {
                    "triggerType": { "const": "schedule" },
                    "cron": { "type": "string", "minLength": 1, "maxLength": 100 },
                    "timezone": { "type": "string", "minLength": 1, "maxLength": 100 }
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["triggerType", "runAt"],
                "properties": {
                    "triggerType": { "const": "once" },
                    "runAt": { "type": "integer", "minimum": 1 }
                }
            }
        ]
    })
}

pub fn execution() -> Value {
    execution_with_prompt(1)
}

/// The agent execution schema with a chosen prompt bound.
///
/// The legacy `missing-agent-prompt` variant is the same shape with an
/// empty prompt allowed: a row that reads is still readable.
fn execution_with_prompt(prompt_min_length: u64) -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "mode", "prompt", "providerId", "model", "reasoningLevel",
                    "permissionMode", "environment"
                ],
                "properties": {
                    "mode": { "const": "agent" },
                    "prompt": { "type": "string", "minLength": prompt_min_length },
                    "providerId": string(1),
                    "model": string(1),
                    "reasoningLevel": {
                        "enum": ["none", "low", "medium", "high", "xhigh", "ultracode", "max", "ultra"]
                    },
                    "serviceTier": { "enum": ["default", "fast"] },
                    "permissionMode": { "enum": ["accept-edits", "auto", "full"] },
                    "environment": environment(),
                    "targetThreadId": string(1)
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["mode", "timeoutMs"],
                "properties": {
                    "mode": { "const": "script" },
                    "script": { "type": "string", "minLength": 1, "maxLength": 262144 },
                    "scriptFile": { "type": "string", "minLength": 1, "maxLength": 200 },
                    "interpreter": { "enum": ["bash", "sh", "node", "python3"] },
                    "timeoutMs": { "type": "integer", "minimum": 1, "maximum": 900000 },
                    "env": { "type": "object" }
                }
            }
        ]
    })
}

fn nullable_string() -> Value {
    json!({ "type": ["string", "null"] })
}

fn nullable_number() -> Value {
    json!({ "type": ["integer", "null"] })
}

pub fn response() -> Value {
    response_with_prompt(1)
}

/// The response schema with a chosen prompt bound, so the legacy empty
/// prompt variant can be described without duplicating the shape.
fn response_with_prompt(prompt_min_length: u64) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "id", "projectId", "name", "enabled", "trigger", "execution", "origin",
            "createdByThreadId", "nextRunAt", "lastRunAt", "runCount", "lastRunStatus",
            "lastRunThreadId", "lastError", "createdAt", "updatedAt"
        ],
        "properties": {
            "id": string(1),
            "projectId": string(1),
            "name": string(1),
            "enabled": { "type": "boolean" },
            "trigger": trigger(),
            "execution": execution_with_prompt(prompt_min_length),
            "origin": { "enum": ["human", "app", "agent"] },
            "createdByThreadId": nullable_string(),
            "nextRunAt": nullable_number(),
            "lastRunAt": nullable_number(),
            "runCount": { "type": "integer", "minimum": 0 },
            "lastRunStatus": { "type": ["string", "null"], "enum": ["running", "succeeded", "failed", "skipped", null] },
            "lastRunThreadId": nullable_string(),
            "lastError": nullable_string(),
            "createdAt": { "type": "integer" },
            "updatedAt": { "type": "integer" }
        }
    })
}

pub fn read_result() -> Value {
    json!({
        "anyOf": [
            response(),
            {
                "allOf": [
                    response_with_prompt(0),
                    {
                        "type": "object",
                        "required": ["problem"],
                        "properties": { "problem": { "const": "missing-agent-prompt" } }
                    }
                ]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "projectId", "name", "problem"],
                "properties": {
                    "id": { "type": "string" },
                    "projectId": { "type": "string" },
                    "name": { "type": "string" },
                    "problem": { "const": "invalid-stored-data" }
                }
            }
        ]
    })
}

pub fn run_response() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "id", "automationId", "runMode", "threadId", "status", "trigger",
            "skipReason", "error", "output", "exitCode", "scheduledFor", "startedAt",
            "finishedAt"
        ],
        "properties": {
            "id": string(1),
            "automationId": string(1),
            "runMode": { "enum": ["agent", "script"] },
            "threadId": nullable_string(),
            "status": { "enum": ["running", "succeeded", "failed", "skipped"] },
            "trigger": { "enum": ["schedule", "manual"] },
            "skipReason": nullable_string(),
            "error": nullable_string(),
            "output": nullable_string(),
            "exitCode": nullable_number(),
            "scheduledFor": { "type": "integer" },
            "startedAt": { "type": "integer" },
            "finishedAt": nullable_number()
        }
    })
}

pub fn create_request() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["name", "trigger", "execution", "origin"],
        "properties": {
            "name": { "type": "string", "minLength": 1, "maxLength": 200 },
            "enabled": { "type": "boolean" },
            "trigger": trigger(),
            "execution": execution(),
            "origin": { "enum": ["human", "app", "agent"] },
            "createdByThreadId": string(1)
        }
    })
}

pub fn update_request() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": { "type": "string", "minLength": 1, "maxLength": 200 },
            "trigger": trigger(),
            "execution": execution(),
            "agent": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prompt": string(1),
                    "providerId": string(1),
                    "model": string(1),
                    "reasoningLevel": {
                        "enum": ["none", "low", "medium", "high", "xhigh", "ultracode", "max", "ultra"]
                    },
                    "serviceTier": { "type": ["string", "null"], "enum": ["default", "fast", null] },
                    "permissionMode": { "enum": ["accept-edits", "auto", "full"] },
                    "target": {
                        "oneOf": [
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["type", "threadId"],
                                "properties": { "type": { "const": "target-thread" }, "threadId": string(1) }
                            },
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["type", "environment"],
                                "properties": { "type": { "const": "environment" }, "environment": environment() }
                            }
                        ]
                    }
                }
            }
        }
    })
}

pub fn run_request() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "idempotencyKey": string(1) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn only_the_write_routes_are_validated() {
        // A typo here would make the middleware a silent no-op, which is the
        // failure mode this whole module exists to close.
        assert_eq!(
            write_operation(&Method::POST, "/api/v1/projects/proj_1/automations"),
            Some(WriteOperation::Create)
        );
        assert_eq!(
            write_operation(&Method::PATCH, "/api/v1/projects/proj_1/automations/auto_1"),
            Some(WriteOperation::Update)
        );
        assert_eq!(
            write_operation(
                &Method::POST,
                "/api/v1/projects/proj_1/automations/auto_1/run"
            ),
            Some(WriteOperation::Run)
        );
        for (method, path) in [
            (Method::GET, "/api/v1/automations"),
            (Method::GET, "/api/v1/projects/proj_1/automations"),
            (Method::GET, "/api/v1/projects/proj_1/automations/auto_1"),
            (
                Method::GET,
                "/api/v1/projects/proj_1/automations/auto_1/runs",
            ),
            (
                Method::POST,
                "/api/v1/projects/proj_1/automations/auto_1/pause",
            ),
            (
                Method::POST,
                "/api/v1/projects/proj_1/automations/auto_1/resume",
            ),
            (Method::DELETE, "/api/v1/projects/proj_1/automations/auto_1"),
            (Method::POST, "/api/v1/projects"),
            (Method::PATCH, "/api/v1/projects/proj_1"),
        ] {
            assert_eq!(write_operation(&method, path), None, "{method} {path}");
        }
    }

    #[test]
    fn the_write_schemas_reject_a_key_the_contract_does_not_name() {
        // The union cases are the ones serde cannot express: each is a valid
        // body with one extra key, and each must fail on the way in.
        assert!(!validate_request(
            WriteOperation::Run,
            &json!({ "idempotencyKey": "key", "extra": 1 })
        )
        .is_empty());
        assert!(!validate_request(
            WriteOperation::Update,
            &json!({ "agent": { "target": { "type": "environment", "environment": { "type": "project-default" }, "extra": 1 } } })
        )
        .is_empty());
        assert!(!validate_request(
            WriteOperation::Create,
            &json!({
                "name": "strict",
                "trigger": { "triggerType": "once", "runAt": 1, "extra": 1 },
                "execution": { "mode": "project-default" },
                "origin": "human"
            })
        )
        .is_empty());

        // …and the same bodies without it are accepted, so the rejection is the
        // key and not the shape.
        assert!(
            validate_request(WriteOperation::Run, &json!({ "idempotencyKey": "key" })).is_empty()
        );
        assert!(validate_request(WriteOperation::Run, &json!({})).is_empty());
        let accepted = validate_request(
            WriteOperation::Update,
            &json!({ "agent": { "target": { "type": "environment", "environment": { "type": "project-default" } } } }),
        );
        assert!(accepted.is_empty(), "{accepted:?}");
    }
}

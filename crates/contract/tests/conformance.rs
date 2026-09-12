//! Conformance tests for the exported bb contract.
//!
//! These tests guard the *artifacts*: they must load, their `$ref`s must
//! resolve, and the validator must accept contract-shaped values while
//! rejecting malformed ones. That is the foundation a route-level conformance
//! test builds on.
//!
//! ## Wiring a newly implemented route
//!
//! When `loom-server` implements one of the contract routes, add a test here:
//!
//! ```ignore
//! #[test]
//! fn system_version_matches_bb() {
//!     let contract = loom_contract::Contract::load();
//!     let route = contract.http_route("GET", "/api/v1/system/version").unwrap();
//!     // `body` is what the handler actually returned, e.g. captured from a
//!     // `tower::ServiceExt::oneshot` call in loom-server's test harness.
//!     let body = serde_json::json!({ /* ... */ });
//!     let violations = contract.validate_response(route, 200, &body);
//!     assert!(violations.is_empty(), "{violations:?}");
//! }
//! ```
//!
//! `validate_response` reports every mismatch with a JSON path, so a failure
//! names the exact field the Rust shape got wrong.

use loom_contract::Contract;
use serde_json::{json, Value};

fn all_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                out.push(reference.clone());
            }
            for child in map.values() {
                all_refs(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                all_refs(item, out);
            }
        }
        _ => {}
    }
}

fn unresolved_refs(document: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    all_refs(document, &mut refs);
    refs.into_iter()
        .filter(|reference| {
            let pointer = reference.strip_prefix('#').unwrap_or(reference);
            document.pointer(pointer).is_none()
        })
        .collect()
}

#[test]
fn artifacts_load_and_are_stamped() {
    let contract = Contract::load();
    assert!(
        contract.routes().len() > 100,
        "expected the full bb route surface, found {}",
        contract.routes().len()
    );
    assert!(
        contract.source_commit().is_some(),
        "manifest must record the bb revision it was generated from"
    );
}

#[test]
fn every_declared_schema_resolves() {
    let contract = Contract::load();
    for (label, document) in [
        ("client", contract.client_ws()),
        ("host-daemon", contract.host_daemon()),
        ("thread-event", contract.thread_event()),
    ] {
        let dangling = unresolved_refs(document);
        assert!(
            dangling.is_empty(),
            "{label} has dangling refs: {dangling:?}"
        );
    }
    assert!(contract.error_codes().get("codes").is_some());
}

#[test]
fn every_json_route_has_a_response_schema() {
    let contract = Contract::load();
    let missing: Vec<&str> = contract
        .routes()
        .iter()
        .filter(|route| {
            route
                .responses
                .iter()
                .any(|response| response.format == "json" && response.schema.is_none())
        })
        .map(|route| route.id.as_str())
        .collect();
    assert!(
        missing.is_empty(),
        "JSON routes without a response schema: {missing:?}"
    );
}

#[test]
fn http_route_lookup_by_mounted_and_relative_path() {
    let contract = Contract::load();
    let mounted = contract.http_route("GET", "/api/v1/system/version");
    let relative = contract.http_route("get", "/system/version");
    assert_eq!(
        mounted.map(|route| route.id.as_str()),
        Some("system.version")
    );
    assert_eq!(
        relative.map(|route| route.id.as_str()),
        Some("system.version")
    );
}

/// The skeleton in action: a real implementation asserts its response against
/// the contract, and the assertion actually bites when a field is wrong.
#[test]
fn response_validation_accepts_and_rejects() {
    let contract = Contract::load();
    let route = contract
        .route_by_id("system.version")
        .expect("system.version is part of the contract");

    let good = json!({
        "currentVersion": "1.2.3",
        "latestVersion": null,
        "source": "npm",
        "updateAvailable": false,
        "isDevelopment": false,
        "upgradeCommand": "npm i -g bb",
    });
    assert!(
        contract.validate_response(route, 200, &good).is_empty(),
        "the sample response should conform"
    );

    let missing_field = json!({
        "latestVersion": null,
        "source": "npm",
        "updateAvailable": false,
        "isDevelopment": false,
        "upgradeCommand": "npm i -g bb",
    });
    let violations = contract.validate_response(route, 200, &missing_field);
    assert!(
        violations
            .iter()
            .any(|v| v.message.contains("currentVersion")),
        "a missing required field must be reported: {violations:?}"
    );

    let wrong_type = json!({
        "currentVersion": 7,
        "latestVersion": null,
        "source": "npm",
        "updateAvailable": false,
        "isDevelopment": false,
        "upgradeCommand": "npm i -g bb",
    });
    assert!(
        !contract
            .validate_response(route, 200, &wrong_type)
            .is_empty(),
        "a wrong field type must be reported"
    );
}

#[test]
fn error_body_shape_is_uniform() {
    let contract = Contract::load();
    let good = json!({ "code": "thread_not_found", "message": "no such thread" });
    assert!(contract.validate_error_body(&good).is_empty());

    let bad = json!({ "code": "thread_not_found" });
    assert!(
        !contract.validate_error_body(&bad).is_empty(),
        "an error body without a message must be rejected"
    );

    assert!(contract.error_statuses("thread_not_found").contains(&404));
}

#[test]
fn client_websocket_messages_conform() {
    let contract = Contract::load();
    let subscribe = json!({
        "type": "subscribe",
        "target": { "kind": "thread-detail", "threadId": "t1" },
    });
    assert!(contract
        .validate_client_message("client", &subscribe)
        .is_empty());

    let unknown_kind = json!({
        "type": "subscribe",
        "target": { "kind": "not-a-target" },
    });
    assert!(!contract
        .validate_client_message("client", &unknown_kind)
        .is_empty());

    let changed = json!({
        "type": "changed",
        "entity": "thread",
        "id": "t1",
        "changes": ["events-appended"],
    });
    assert!(contract
        .validate_server_message("client", &changed)
        .is_empty());

    let ping = json!({ "type": "ping" });
    assert!(contract.validate_client_message("client", &ping).is_empty());
    let pong = json!({ "type": "pong" });
    assert!(contract.validate_server_message("client", &pong).is_empty());
}

#[test]
fn daemon_contract_is_typed() {
    let contract = Contract::load();
    assert_eq!(
        contract
            .host_daemon()
            .pointer("/protocolVersion")
            .and_then(Value::as_u64),
        Some(199)
    );
    let settled = contract
        .host_daemon()
        .pointer("/commands/settledTypes")
        .and_then(Value::as_array)
        .expect("settled command types");
    assert!(settled.iter().any(|value| value == "thread.start"));

    let heartbeat = json!({ "type": "heartbeat" });
    assert!(
        contract.validate_daemon_message(&heartbeat).is_empty(),
        "heartbeat must be a valid daemon -> server frame"
    );
    assert!(!contract
        .validate_daemon_message(&json!({ "type": "not-a-message" }))
        .is_empty());
}

fn resolve_schema<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let mut current = schema;
    for _ in 0..64 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(pointer) = reference.strip_prefix('#') else {
            break;
        };
        let Some(target) = root.pointer(pointer) else {
            break;
        };
        if std::ptr::eq(current, target) {
            break;
        }
        current = target;
    }
    current
}

fn sample_from_schema(root: &Value, schema: &Value, depth: usize) -> Value {
    assert!(
        depth < 64,
        "schema sample generation exceeded recursion limit"
    );
    let schema = resolve_schema(root, schema);

    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if let Some(value) = values.first() {
            return value.clone();
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array) {
            for branch in branches {
                let candidate = sample_from_schema(root, branch, depth + 1);
                if loom_contract::is_valid(root, schema, &candidate) {
                    return candidate;
                }
            }
        }
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        let mut merged = serde_json::Map::new();
        for branch in branches {
            let value = sample_from_schema(root, branch, depth + 1);
            if let Value::Object(object) = value {
                merged.extend(object);
            }
        }
        return Value::Object(merged);
    }

    match schema.get("type") {
        Some(Value::String(kind)) => sample_for_type(root, schema, kind, depth),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find_map(|kind| {
                let candidate = sample_for_type(root, schema, kind, depth);
                loom_contract::is_valid(root, schema, &candidate).then_some(candidate)
            })
            .unwrap_or(Value::Null),
        _ => Value::Object(serde_json::Map::new()),
    }
}

fn sample_for_type(root: &Value, schema: &Value, kind: &str, depth: usize) -> Value {
    match kind {
        "object" => {
            let mut object = serde_json::Map::new();
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            let properties = schema.get("properties").and_then(Value::as_object);
            for name in required {
                if let Some(property) = properties.and_then(|properties| properties.get(name)) {
                    object.insert(
                        name.to_string(),
                        sample_from_schema(root, property, depth + 1),
                    );
                }
            }
            Value::Object(object)
        }
        "array" => {
            let prefix = schema
                .get("prefixItems")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut items: Vec<Value> = prefix
                .iter()
                .map(|item| sample_from_schema(root, item, depth + 1))
                .collect();
            let min_items = schema
                .get("minItems")
                .and_then(Value::as_u64)
                .unwrap_or(items.len() as u64) as usize;
            if items.len() < min_items {
                if let Some(item_schema) = schema.get("items") {
                    while items.len() < min_items {
                        items.push(sample_from_schema(root, item_schema, depth + 1));
                    }
                }
            }
            Value::Array(items)
        }
        "string" => Value::String("sample".to_string()),
        "integer" => Value::from(1),
        "number" => Value::from(1),
        "boolean" => Value::Bool(false),
        "null" => Value::Null,
        _ => Value::Object(serde_json::Map::new()),
    }
}

#[test]
fn every_thread_event_type_has_valid_and_invalid_samples() {
    let contract = Contract::load();
    let types = contract.thread_event_types();
    assert!(
        types.len() >= 35,
        "expected the complete ThreadEvent union, found {} types",
        types.len()
    );
    let schemas = contract
        .thread_event()
        .pointer("/schemasByType")
        .and_then(Value::as_object)
        .expect("ThreadEvent schemasByType");
    assert_eq!(schemas.len(), types.len());

    for event_type in types {
        let schema = contract
            .thread_event_schema(event_type)
            .expect("every event type has a schema");
        let sample = sample_from_schema(contract.thread_event(), schema, 0);
        assert_eq!(sample["type"], event_type);
        assert!(
            contract
                .validate_thread_event_type(event_type, &sample)
                .is_empty(),
            "valid sample for {event_type} was rejected: {:?}",
            contract.validate_thread_event_type(event_type, &sample)
        );

        let mut missing_required = sample.clone();
        let removed = missing_required
            .as_object_mut()
            .expect("event sample object")
            .remove("threadId");
        assert!(removed.is_some(), "{event_type} must require threadId");
        assert!(
            !contract
                .validate_thread_event_type(event_type, &missing_required)
                .is_empty(),
            "missing threadId should be rejected for {event_type}"
        );
    }
}

#[test]
fn rust_serialized_thread_event_conformance_skeleton() {
    let contract = Contract::load();
    // Replace this representative JSON with serde_json::to_value of the Rust
    // event once loom implements the corresponding domain event variant.
    let serialized = json!({
        "type": "thread/started",
        "threadId": "thread_sample",
        "scope": { "kind": "thread" },
    });
    assert!(
        contract.validate_thread_event(&serialized).is_empty(),
        "Rust event serialization must stay inside the bb ThreadEvent contract"
    );
}

#[test]
fn validator_rejects_unknown_properties() {
    let contract = Contract::load();
    let route = contract
        .route_by_id("realtimeSubscriptionTarget")
        .or_else(|| contract.route_by_id("system.version"))
        .unwrap();
    let body = json!({
        "currentVersion": "1",
        "latestVersion": null,
        "source": "npm",
        "updateAvailable": false,
        "isDevelopment": false,
        "upgradeCommand": "x",
        "unexpected": true,
    });
    assert!(
        !contract.validate_response(route, 200, &body).is_empty(),
        "additionalProperties: false must be enforced"
    );
}

/// The route matcher used by loom-server's runtime request validation.
///
/// Exact lookup cannot see that a live request reached a parameterized route,
/// so without this the request middleware would never fire on
/// `/api/v1/threads/:id/send`.
#[test]
fn route_matching_tolerates_parameters() {
    let contract = Contract::load();

    let by_id = contract
        .match_route("POST", "/api/v1/threads/thr_01M2A5BQ/send")
        .expect("parameterized path matches");
    assert_eq!(by_id.id, "threads.send");

    let create = contract
        .match_route("post", "/api/v1/threads")
        .expect("relative-free path matches");
    assert_eq!(create.id, "threads.create");

    let scoped = contract
        .match_route("PATCH", "/api/v1/projects/proj_1")
        .expect("project update matches");
    assert_eq!(scoped.id, "projects.update");

    // A query string is not part of the path.
    assert_eq!(
        contract
            .match_route("GET", "/api/v1/threads?limit=10")
            .map(|route| route.id.as_str()),
        Some("threads.list"),
    );

    // A different method is a different route.
    assert_eq!(
        contract
            .match_route("DELETE", "/api/v1/threads/thr_1")
            .map(|route| route.id.as_str()),
        Some("threads.delete")
    );

    // Extra or missing segments must not match.
    assert!(contract
        .match_route("POST", "/api/v1/threads/thr_1/sends")
        .is_none());
    assert!(contract.match_route("POST", "/api/v1/nowhere").is_none());
    assert!(contract
        .match_route("POST", "/api/v1/threads/thr_1/extra/deep")
        .is_none());
}

/// Request validation is the symmetric half of response validation: the exact
/// shape the issue found loom's `POST /api/v1/threads` rejecting must pass.
#[test]
fn request_validation_accepts_contract_shapes() {
    let contract = Contract::load();

    let create = json!({
        "projectId": "proj_1",
        "origin": "app",
        "input": [{ "type": "text", "text": "hello" }],
        "environment": { "type": "project-default" },
    });
    assert!(
        contract
            .validate_request_by_id("threads.create", &create)
            .is_empty(),
        "the contract's threads.create shape must be accepted: {:?}",
        contract.validate_request_by_id("threads.create", &create)
    );

    let send = json!({
        "input": [{ "type": "text", "text": "hello" }],
        "mode": "start",
    });
    assert!(contract
        .validate_request_by_id("threads.send", &send)
        .is_empty());

    let project = json!({
        "name": "loom",
        "source": { "hostId": "host_1", "type": "local_path", "path": "/srv/loom" },
    });
    assert!(contract
        .validate_request_by_id("projects.create", &project)
        .is_empty());

    let source = json!({
        "hostId": "host_1",
        "type": "local_path",
        "path": "/srv/loom",
    });
    assert!(
        contract
            .validate_request_by_id("projects.createSource", &source)
            .is_empty(),
        "projects.createSource must accept the local_path variant: {:?}",
        contract.validate_request_by_id("projects.createSource", &source)
    );

    // The other branch of the discriminated union is also accepted.
    let clone = json!({ "hostId": "host_1", "type": "clone", "remoteUrl": "git@x:y" });
    assert!(contract
        .validate_request_by_id("projects.createSource", &clone)
        .is_empty());

    assert!(contract
        .validate_request_by_id("projects.update", &json!({ "name": "loom-2" }))
        .is_empty());
}

/// The pre-contract snake_case shape is rejected. This is the regression guard
/// for the "response looks right, request is wrong" blind spot the issue
/// describes: loom accepted `{ "project_id": ... }` and the suite stayed green.
#[test]
fn request_validation_rejects_the_legacy_shape() {
    let contract = Contract::load();

    let legacy = json!({ "project_id": "proj_1" });
    let violations = contract.validate_request_by_id("threads.create", &legacy);
    assert!(
        !violations.is_empty(),
        "snake_case must not satisfy threads.create"
    );
    assert!(
        violations.iter().any(|v| v.message.contains("projectId")),
        "the missing required camelCase field must be named: {violations:?}"
    );

    let missing_send_mode = json!({ "input": [{ "type": "text", "text": "hi" }] });
    assert!(!contract
        .validate_request_by_id("threads.send", &missing_send_mode)
        .is_empty());

    // A source without its required `hostId`/`type` is rejected.
    assert!(!contract
        .validate_request_by_id("projects.createSource", &json!({ "path": "/srv/x" }))
        .is_empty());

    // A project update with the wrong type for the only declared field.
    assert!(!contract
        .validate_request_by_id("projects.update", &json!({ "name": 7 }))
        .is_empty());

    let wrong_type = json!({
        "projectId": 7,
        "origin": "app",
        "input": [],
        "environment": { "type": "project-default" },
    });
    assert!(!contract
        .validate_request_by_id("threads.create", &wrong_type)
        .is_empty());

    // An unknown route is a violation, never a silent pass.
    assert!(!contract
        .validate_request_by_id("threads.not-a-route", &json!({}))
        .is_empty());
}

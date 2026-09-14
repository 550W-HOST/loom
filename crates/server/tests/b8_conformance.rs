//! B8 request-shape conformance.
//!
//! The router middleware performs the same validation at runtime. These cases
//! keep the coverage inventory honest by proving each B8 JSON request uses the
//! bb camelCase shape and rejects an extra field. Required route markers are
//! validate_request_by_id("hosts.providerCliInstall") and
//! validate_request_by_id("hosts.updatePermissionCeiling").

use loom_contract::shared;
use serde_json::json;

#[test]
fn b8_json_requests_match_the_contract() {
    assert!(shared()
        .validate_request_by_id("hosts.createJoinCode", &json!({}))
        .is_empty());
    assert!(shared()
        .validate_request_by_id("hosts.pathsExist", &json!({ "paths": ["/tmp"] }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id("hosts.pickFolder", &json!({ "clientHostId": "host_1" }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id(
            "hosts.providerCliInstall",
            &json!({ "provider": "pi", "actionKind": "install" }),
        )
        .is_empty());
    assert!(shared()
        .validate_request_by_id("hosts.update", &json!({ "name": "builder" }))
        .is_empty());
    assert!(shared()
        .validate_request_by_id(
            "hosts.updatePermissionCeiling",
            &json!({ "maxPermissionMode": "full" }),
        )
        .is_empty());

    assert!(!shared()
        .validate_request_by_id(
            "hosts.update",
            &json!({ "name": "builder", "path": "/tmp" }),
        )
        .is_empty());
}

//! bb's wire contract, exported to JSON Schema, as a conformance target.
//!
//! `loom-server` has to speak bb's HTTP and WebSocket contract for bb's UI to
//! work unchanged. This crate embeds the generated artifacts under
//! `contracts/bb/` and turns them into assertions: look up a route or message,
//! then validate a body against its schema.
//!
//! The artifacts are generated, never hand-edited. Regenerate with
//! `scripts/export-bb-contract.sh`; see `docs/contract.md`.

mod schema;

pub use schema::{is_valid, validate, Violation};

use serde::Deserialize;
use serde_json::Value;

const SERVER_API: &str = include_str!("../../../contracts/bb/server-api.json");
const CLIENT_WS: &str = include_str!("../../../contracts/bb/client-ws.json");
const HOST_DAEMON: &str = include_str!("../../../contracts/bb/host-daemon.json");
const ERROR_CODES: &str = include_str!("../../../contracts/bb/error-codes.json");
const THREAD_EVENT: &str = include_str!("../../../contracts/bb/thread-event.json");
const MANIFEST: &str = include_str!("../../../contracts/bb/manifest.json");

/// The HTTP request half of a route: where the input comes from and its shape.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestContract {
    /// `none`, `query`, `json` or `form`.
    pub source: String,
    /// Absent for routes with no parseable input (`none`, `form`).
    pub schema: Option<Value>,
}

/// One declared response: status, representation and shape.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseContract {
    pub status: u16,
    /// `json`, `text` or `binary`.
    pub format: String,
    pub schema: Option<Value>,
}

/// A route exactly as bb declares it in `publicApiRoutes`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRoute {
    /// Stable dotted key, e.g. `system.version`.
    pub id: String,
    /// Upper-case HTTP method.
    pub method: String,
    /// Path relative to the API mount, e.g. `/system/version`.
    pub path: String,
    /// Path including the mount, e.g. `/api/v1/system/version`.
    pub full_path: String,
    pub request: RequestContract,
    pub responses: Vec<ResponseContract>,
}

impl HttpRoute {
    /// The JSON response schema for a status, when the contract types it.
    pub fn response_schema(&self, status: u16) -> Option<&Value> {
        self.responses
            .iter()
            .find(|response| response.status == status)
            .and_then(|response| response.schema.as_ref())
    }
}

/// The whole bb contract, parsed once per process.
#[derive(Clone, Debug)]
pub struct Contract {
    manifest: Value,
    server_api: Value,
    client_ws: Value,
    host_daemon: Value,
    error_codes: Value,
    thread_event: Value,
    /// Routes parsed out of `server_api` for typed access.
    routes: Vec<HttpRoute>,
}

impl Contract {
    /// Parse the embedded artifacts. Panics when an artifact is malformed,
    /// which is a build-time break and should stop the test run immediately.
    pub fn load() -> Self {
        let server_api: Value = serde_json::from_str(SERVER_API).expect("server-api.json");
        let routes: Vec<HttpRoute> = serde_json::from_value(
            server_api
                .get("routes")
                .cloned()
                .expect("server-api.json has routes"),
        )
        .expect("routes deserialize");
        Self {
            manifest: serde_json::from_str(MANIFEST).expect("manifest.json"),
            server_api,
            client_ws: serde_json::from_str(CLIENT_WS).expect("client-ws.json"),
            host_daemon: serde_json::from_str(HOST_DAEMON).expect("host-daemon.json"),
            error_codes: serde_json::from_str(ERROR_CODES).expect("error-codes.json"),
            thread_event: serde_json::from_str(THREAD_EVENT).expect("thread-event.json"),
            routes,
        }
    }

    /// Every HTTP route in the contract.
    pub fn routes(&self) -> &[HttpRoute] {
        &self.routes
    }

    /// Look a route up by method and either the mounted or the relative path.
    pub fn http_route(&self, method: &str, path: &str) -> Option<&HttpRoute> {
        let method = method.to_ascii_uppercase();
        self.routes
            .iter()
            .find(|route| route.method == method && (route.full_path == path || route.path == path))
    }

    /// Look a route up by method and a concrete request path, treating path
    /// parameters as wildcards.
    ///
    /// The contract writes a parameter as `:id`; a live request carries the
    /// value. Exact lookup cannot see that a request reached a route, so a
    /// middleware that wants to validate a request body needs this. Segments
    /// are compared positionally and a parameter segment matches any single,
    /// non-empty segment; a trailing `{.+}` catch-all is matched as one.
    pub fn match_route(&self, method: &str, path: &str) -> Option<&HttpRoute> {
        let method = method.to_ascii_uppercase();
        let request_segments: Vec<&str> = split_path(path);
        self.routes
            .iter()
            .filter(|route| route.method == method)
            .find(|route| segments_match(&split_path(&route.full_path), &request_segments))
    }

    /// Find a route by its dotted id.
    pub fn route_by_id(&self, id: &str) -> Option<&HttpRoute> {
        self.routes.iter().find(|route| route.id == id)
    }

    /// The bb source revision the artifacts were generated from.
    pub fn source_commit(&self) -> Option<&str> {
        self.manifest.pointer("/source/commit")?.as_str()
    }

    /// Raw access to the exported ThreadEvent artifact.
    pub fn thread_event(&self) -> &Value {
        &self.thread_event
    }

    /// Every ThreadEvent discriminator value in bb's declared order.
    pub fn thread_event_types(&self) -> Vec<&str> {
        self.thread_event
            .get("eventTypes")
            .and_then(Value::as_array)
            .map(|types| types.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// The schema for one ThreadEvent discriminator value.
    pub fn thread_event_schema(&self, event_type: &str) -> Option<&Value> {
        self.thread_event
            .get("schemasByType")
            .and_then(Value::as_object)
            .and_then(|schemas| schemas.get(event_type))
    }

    /// Validate a complete ThreadEvent against the union schema.
    pub fn validate_thread_event(&self, instance: &Value) -> Vec<Violation> {
        match self.thread_event.get("schema") {
            Some(schema) => validate(&self.thread_event, schema, instance),
            None => vec![Violation {
                path: "$".to_string(),
                message: "thread-event artifact has no union schema".to_string(),
            }],
        }
    }

    /// Validate a ThreadEvent against the schema selected by its `type`.
    pub fn validate_thread_event_type(&self, event_type: &str, instance: &Value) -> Vec<Violation> {
        match self.thread_event_schema(event_type) {
            Some(schema) => validate(&self.thread_event, schema, instance),
            None => vec![Violation {
                path: "$.type".to_string(),
                message: format!("unknown ThreadEvent type `{event_type}`"),
            }],
        }
    }

    /// Validate a parsed request body or query object for a route.
    pub fn validate_request(&self, route: &HttpRoute, instance: &Value) -> Vec<Violation> {
        match &route.request.schema {
            Some(schema) => validate(&self.server_api, schema, instance),
            None => Vec::new(),
        }
    }

    /// Validate a parsed request body for a route id, the symmetric counterpart
    /// of [`Contract::route_by_id`] + [`Contract::validate_request`].
    ///
    /// An unknown id is a violation rather than an empty pass: a typo in a test
    /// or handler must not silently disable the check.
    pub fn validate_request_by_id(&self, id: &str, instance: &Value) -> Vec<Violation> {
        match self.route_by_id(id) {
            Some(route) => self.validate_request(route, instance),
            None => vec![Violation {
                path: "$".to_string(),
                message: format!("unknown contract route `{id}`"),
            }],
        }
    }

    /// Validate a response body for a route and status.
    pub fn validate_response(
        &self,
        route: &HttpRoute,
        status: u16,
        instance: &Value,
    ) -> Vec<Violation> {
        if !route
            .responses
            .iter()
            .any(|response| response.status == status)
        {
            return vec![Violation {
                path: "$".to_string(),
                message: format!("route {} declares no {} response", route.id, status),
            }];
        }
        match route.response_schema(status) {
            Some(schema) => validate(&self.server_api, schema, instance),
            None => Vec::new(),
        }
    }

    /// The shared error body shape (`apiErrorSchema`).
    pub fn validate_error_body(&self, instance: &Value) -> Vec<Violation> {
        match self.server_api.get("errorResponse") {
            Some(schema) => validate(&self.server_api, schema, instance),
            None => Vec::new(),
        }
    }

    /// Validation for a UI client -> server frame on a named protocol.
    pub fn validate_client_message(&self, protocol: &str, instance: &Value) -> Vec<Violation> {
        match self.protocol_schemas(protocol, "clientToServer") {
            Some(schemas) => self.validate_any(&self.client_ws, schemas, instance),
            None => unknown_protocol(protocol),
        }
    }

    /// Validation for a server -> UI client frame on a named protocol.
    pub fn validate_server_message(&self, protocol: &str, instance: &Value) -> Vec<Violation> {
        match self.protocol_schemas(protocol, "serverToClient") {
            Some(schemas) => self.validate_any(&self.client_ws, schemas, instance),
            None => unknown_protocol(protocol),
        }
    }

    /// Validate a daemon -> server frame (`hostDaemonDaemonWsMessageSchema`).
    pub fn validate_daemon_message(&self, instance: &Value) -> Vec<Violation> {
        match self
            .host_daemon
            .pointer("/websocket/clientToServer/0/schema")
        {
            Some(schema) => validate(&self.host_daemon, schema, instance),
            None => Vec::new(),
        }
    }

    /// Validate a server -> daemon frame (`hostDaemonServerWsMessageSchema`).
    pub fn validate_server_to_daemon_message(&self, instance: &Value) -> Vec<Violation> {
        match self
            .host_daemon
            .pointer("/websocket/serverToClient/0/schema")
        {
            Some(schema) => validate(&self.host_daemon, schema, instance),
            None => Vec::new(),
        }
    }

    /// Validate a settled daemon command (`hostDaemonCommandSchema`).
    pub fn validate_daemon_command(&self, instance: &Value) -> Vec<Violation> {
        match self.host_daemon.pointer("/commands/settled") {
            Some(schema) => validate(&self.host_daemon, schema, instance),
            None => Vec::new(),
        }
    }

    /// Raw access to the daemon artifact for anything not wrapped above.
    pub fn host_daemon(&self) -> &Value {
        &self.host_daemon
    }

    /// Raw access to the client WebSocket artifact.
    pub fn client_ws(&self) -> &Value {
        &self.client_ws
    }

    /// The best-effort inventory of server error codes.
    pub fn error_codes(&self) -> &Value {
        &self.error_codes
    }

    /// Statuses bb pairs with an error code, from the throw-site scan.
    pub fn error_statuses(&self, code: &str) -> Vec<u64> {
        self.error_codes
            .pointer("/codes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|entry| entry.get("code").and_then(Value::as_str) == Some(code))
            .flat_map(|entry| {
                entry
                    .get("statuses")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
            })
            .collect()
    }

    fn protocol_schemas(&self, protocol: &str, direction: &str) -> Option<Vec<Value>> {
        let protocols = self.client_ws.get("protocols")?.as_array()?;
        let entry = protocols
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(protocol))?;
        let schemas = entry.get(direction)?.as_array()?;
        Some(
            schemas
                .iter()
                .filter_map(|schema| schema.get("schema").cloned())
                .collect(),
        )
    }

    fn validate_any(&self, root: &Value, schemas: Vec<Value>, instance: &Value) -> Vec<Violation> {
        if schemas.is_empty() {
            return Vec::new();
        }
        if schemas
            .iter()
            .any(|schema| is_valid(root, schema, instance))
        {
            return Vec::new();
        }
        vec![Violation {
            path: "$".to_string(),
            message: "message matches none of the protocol's declared shapes".to_string(),
        }]
    }
}

fn unknown_protocol(protocol: &str) -> Vec<Violation> {
    vec![Violation {
        path: "$".to_string(),
        message: format!("unknown protocol `{protocol}`"),
    }]
}

/// Split a path into non-empty segments, ignoring the leading slash and a
/// possible query string.
fn split_path(path: &str) -> Vec<&str> {
    path.split('?')
        .next()
        .unwrap_or(path)
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// True when `pattern` segments accept `request` segments.
fn segments_match(pattern: &[&str], request: &[&str]) -> bool {
    let mut pattern_index = 0;
    let mut request_index = 0;
    while pattern_index < pattern.len() {
        let segment = pattern[pattern_index];
        // A trailing `{.+}` capture (`:filePath{.+}`) consumes every remaining
        // segment, including separators; nothing may follow it.
        if segment.contains("{.+}") {
            return request_index < request.len() && pattern_index + 1 == pattern.len();
        }
        // `:id` is a parameter and `{id}` a wildcard capture; both consume
        // exactly one live segment.
        let is_parameter = segment.starts_with(':') || segment.starts_with('{');
        if !is_parameter {
            if request.get(request_index) != Some(&segment) {
                return false;
            }
        } else if request.get(request_index).is_none() {
            return false;
        }
        pattern_index += 1;
        request_index += 1;
    }
    request_index == request.len()
}

impl Default for Contract {
    fn default() -> Self {
        Self::load()
    }
}

static SHARED: std::sync::OnceLock<Contract> = std::sync::OnceLock::new();

/// The process-wide contract, parsed once on first use.
///
/// `Contract::load` parses several megabytes of embedded JSON Schema, which is
/// fine in a test but wasteful on a request path. A server validating live
/// requests uses this; a test that wants a local value can still call
/// [`Contract::load`].
pub fn shared() -> &'static Contract {
    SHARED.get_or_init(Contract::load)
}

/// Violations rendered as one line, for a uniform API error message.
pub fn describe(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

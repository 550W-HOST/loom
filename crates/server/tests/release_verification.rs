//! The release script's HTTP claims, run on every pull request.
//!
//! `scripts/verify-release-binaries.sh` runs only in the `v*` tag pipeline, so
//! a shape change that breaks it stays invisible until a release is cut. That
//! is how W-554 shipped: the routes moved to the contract's shapes, every test
//! stayed green, and the first release failed on `POST /api/v1/projects` —
//! whose request body had gained a required `source` and whose response was no
//! longer wrapped in `{ "project": … }`. The in-crate tests could not catch it
//! because they validate responses against the contract, and the contract was
//! exactly what had changed; what lagged was the consumer.
//!
//! So the requests below are the script's, in its order, over a real listener,
//! and each assertion is one of its jq expressions. The test is the shape
//! contract of a consumer, not of the API: keep the two in step.

use std::time::Duration;

use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Starts a server on an ephemeral port and returns its address and state.
async fn spawn_server() -> (String, AppState) {
    let state = AppState::build(AppConfig::default()).unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("{}:{}", addr.ip(), addr.port()), state)
}

struct Response {
    status: u16,
    body: Value,
}

/// One request, parsed far enough to read its status and JSON body — which is
/// all the release script does with `curl` and `jq`.
async fn request(addr: &str, method: &str, path: &str, body: Option<&Value>) -> Response {
    let payload = body.map(Value::to_string);
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let head = match &payload {
        Some(payload) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    };
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .expect("HTTP response timed out")
        .unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("response had no body separator");
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("no HTTP status line");
    let body =
        serde_json::from_str(body).unwrap_or_else(|error| panic!("bad JSON body: {error}\n{body}"));
    Response { status, body }
}

#[tokio::test]
async fn the_release_script_parses_the_shapes_the_server_answers() {
    let (addr, state) = spawn_server().await;

    // `/health`: the script reads `.status`, `.protocol_version` and `.node_id`
    // off the start-up probe and refuses a server whose protocol disagrees with
    // `--version`.
    let health = request(&addr, "GET", "/health", None).await;
    assert_eq!(health.status, 200, "GET /health: {}", health.body);
    assert_eq!(health.body["status"], "ok", "GET /health: {}", health.body);
    assert!(
        health.body["protocol_version"].is_number(),
        "GET /health no longer reports a protocol version: {}",
        health.body
    );
    assert!(
        health.body["node_id"].is_string(),
        "GET /health no longer reports a node id: {}",
        health.body
    );

    // The host a worker enrols as. Created through the registry rather than the
    // register route, so this test depends only on the endpoints the script
    // itself consumes.
    let host = state
        .registry
        .enroll_host(None, "release-verification".into(), loom_relay::now_ms())
        .unwrap()
        .0;

    // `hosts.list`: a bare array whose rows carry `.status` and `.id`. The
    // script waits on `.[] | select(.status == "connected")` to know the worker
    // enrolled, then takes `.id` from that row.
    let hosts = request(&addr, "GET", "/api/v1/hosts", None).await;
    assert_eq!(hosts.status, 200, "GET /api/v1/hosts: {}", hosts.body);
    let hosts = hosts.body.as_array().unwrap_or_else(|| {
        panic!(
            "GET /api/v1/hosts must answer a bare array, not an envelope: {}",
            hosts.body
        )
    });
    let enrolled = hosts
        .iter()
        .find(|row| row["status"] == "connected")
        .unwrap_or_else(|| panic!("no host is connected in {hosts:?}"));
    assert_eq!(enrolled["id"], host.id.to_string());

    // `projects.create`: `{ name, source }` in, the project itself out at 201.
    // Both halves broke the release script: the body it used to send had no
    // `source`, so the request validator answered 422, and `.project.id` read a
    // field the bare response does not have.
    let created = request(
        &addr,
        "POST",
        "/api/v1/projects",
        Some(&json!({
            "name": "release-verification",
            "source": {
                "type": "local_path",
                "hostId": host.id,
                "path": "/srv/release-verification",
            },
        })),
    )
    .await;
    assert_eq!(
        created.status, 201,
        "POST /api/v1/projects: {}",
        created.body
    );
    assert!(
        created.body.get("project").is_none(),
        "projects.create answers the project itself, not `{{ \"project\": … }}`: {}",
        created.body
    );
    let project_id = created.body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("projects.create answered no project id: {}", created.body))
        .to_string();

    // `projects.list`: the same bare array, carrying the project that was just
    // written. This is the read-back that proves the write reached the state a
    // client sees.
    let projects = request(&addr, "GET", "/api/v1/projects", None).await;
    assert_eq!(
        projects.status, 200,
        "GET /api/v1/projects: {}",
        projects.body
    );
    let projects = projects.body.as_array().unwrap_or_else(|| {
        panic!(
            "GET /api/v1/projects must answer a bare array, not an envelope: {}",
            projects.body
        )
    });
    assert!(
        projects
            .iter()
            .any(|project| project["id"] == project_id.as_str()),
        "the project {project_id} that was just created is not in {projects:?}"
    );

    state.shutdown().unwrap();
}

/// One request whose response is read as raw bytes and headers.
///
/// The install routes are not JSON on the success path (`application/octet-stream`
/// with the digest in a header), so the JSON helper above cannot express them.
/// This is deliberately as dumb as `curl -D`/`-o` in the script: status line,
/// headers, body, nothing interpreted.
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| key.to_ascii_lowercase() == name)
            .map(|(_, value)| value.as_str())
    }
}

async fn raw_request(addr: &str, path: &str, extra_headers: &[(&str, &str)]) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut head = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .expect("HTTP response timed out")
        .unwrap();

    let separator = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response had no header separator");
    let head = String::from_utf8(raw[..separator].to_vec()).unwrap();
    let body = raw[separator + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("no HTTP status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    RawResponse {
        status,
        headers,
        body,
    }
}

/// The install routes, consumed exactly as `scripts/verify-release-binaries.sh`
/// consumes them.
///
/// This is the W-554 guard applied to self-update: the script runs only on a
/// `v*` tag, so a shape change here would stay invisible until a release. The
/// server in this test is wired to a directory holding a stand-in artifact, so
/// what is asserted is the response contract — the two digests' relationship,
/// the header name, the `304` — not the identity of a binary.
#[tokio::test]
async fn the_release_script_parses_the_install_routes_the_server_answers() {
    let artifacts = tempfile::tempdir().unwrap();
    // A stand-in with real length, so the digest is over more than an empty
    // file and the body comparison means something.
    let bytes = b"a stand-in for the released loom-worker binary\n".repeat(64);
    std::fs::write(artifacts.path().join("loom-worker"), &bytes).unwrap();

    let state = AppState::build(AppConfig {
        artifact_dir: Some(artifacts.path().to_path_buf()),
        ..AppConfig::default()
    })
    .unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let addr = format!("{}:{}", addr.ip(), addr.port());

    // `GET /install/version`: the script reads `.protocolVersion` and compares
    // it with the `--version` protocol of the binaries. `install_version` uses
    // `camelCase`, which is what makes that `jq` expression non-null.
    let version = raw_request(&addr, "/install/version", &[]).await;
    assert_eq!(
        version.status, 200,
        "/install/version -> {}",
        version.status
    );
    let version: Value = serde_json::from_slice(&version.body).unwrap();
    assert_eq!(
        version["protocolVersion"].as_u64(),
        Some(u64::from(loom_server::PROTOCOL_VERSION)),
        "/install/version no longer reports `protocolVersion`: {version}"
    );
    assert!(
        version["version"].is_string(),
        "/install/version no longer reports `version`: {version}"
    );

    // `GET /install/loom-worker`: the header the script extracts with `sed`,
    // and the body it hashes. The header must be the digest of the bytes, or a
    // downloading worker would refuse a good artifact.
    let artifact = raw_request(
        &addr,
        &format!("/install/loom-worker?target={}", loom_server::TARGET),
        &[],
    )
    .await;
    assert_eq!(
        artifact.status, 200,
        "/install/loom-worker -> {}",
        artifact.status
    );
    let digest = artifact
        .header("x-loom-artifact-sha256")
        .unwrap_or_else(|| {
            panic!(
                "the script's `sed -n 's/^X-Loom-Artifact-Sha256: *//p'` finds nothing in {:?}",
                artifact.headers
            )
        })
        .to_owned();
    assert!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "the served digest is not lowercase hex: {digest:?}"
    );
    assert_eq!(artifact.body, bytes, "the served body is not the artifact");
    assert_eq!(
        loom_server::artifacts::sha256_hex(&artifact.body),
        digest,
        "the served body does not hash to the served header"
    );

    // The conditional request: the script sends the digest back and requires a
    // `304` with no body, which is what makes a fleet's reconnects cheap.
    let conditional = raw_request(
        &addr,
        &format!("/install/loom-worker?target={}", loom_server::TARGET),
        &[("If-None-Match", &format!("\"sha256-{digest}\"")[..])],
    )
    .await;
    assert_eq!(
        conditional.status,
        304,
        "a conditional artifact request answered {}: {:?}",
        conditional.status,
        String::from_utf8_lossy(&conditional.body)
    );
    assert!(conditional.body.is_empty(), "a 304 carried a body");

    state.shutdown().unwrap();
}

//! Hosted worker artifacts: the server side and the client side of the
//! self-update source of truth.
//!
//! A server hosts the worker binaries that match **its own** protocol version,
//! and a worker fetches the one for its target triple when the two disagree on
//! the wire. Both halves live here so the path, the headers and the digest
//! format cannot drift apart: the worker depends on `loom-server` for
//! [`PROTOCOL_VERSION`] already, so the same module that serves the bytes
//! defines what fetching them means.
//!
//! # What is served
//!
//! | Route | Answer |
//! | --- | --- |
//! | `GET /install/version` | `{"version":"0.1.0","protocolVersion":3}` |
//! | `GET /install/loom-worker?target=<triple>` | the binary, its SHA-256 in `ETag` and `X-Loom-Artifact-Sha256` |//!
//! The artifact directory defaults to the directory the running `loom` was
//! started from, so an install that leaves a `loom-worker` name beside it hosts
//! the worker with no configuration at all. `--artifact-dir` overrides it.
//! Either `loom-worker-<triple>` (the release page's name) or `loom-worker`
//! (a plain name, for the server's own triple) is accepted.
//!
//! # Authentication
//!
//! None, like every other route: the control plane has no authentication layer
//! and the network boundary is the security model (`docs/remote-access.md`).
//! Serving the worker binary adds no exposure the API did not already have —
//! the API already dispatches arbitrary command execution to every enrolled
//! machine — and the bytes are public software. See `docs/upgrades.md`.
//!
//! # Digest, not signature
//!
//! `X-Loom-Artifact-Sha256` is computed by the serving host from the file it
//! read. It proves the download was not truncated or corrupted in transit, and
//! it gives a worker a stable identity for "the artifact I already installed"
//! (the `ETag` a conditional request is made against). It does **not** protect
//! against a compromised server, because the server is also what serves the
//! digest. That is the same trust root the dispatch path already rests on;
//! signing would move the trust root, and is called out as the next step in
//! `docs/upgrades.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

/// Where a worker asks the server about itself.
pub const INSTALL_VERSION_PATH: &str = "/install/version";

/// Where a worker asks for the binary for a target triple.
pub const INSTALL_WORKER_PATH: &str = "/install/loom-worker";

/// Query parameter naming the target triple.
pub const TARGET_QUERY: &str = "target";

/// Response header carrying the artifact's SHA-256 as lowercase hex.
pub const DIGEST_HEADER: &str = "x-loom-artifact-sha256";

/// The version and protocol a server reports to a worker that is deciding
/// whether it needs to update.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstallVersion {
    /// The server's crate version, for the log line.
    pub version: String,
    /// The server's protocol version: the number the worker compares against
    /// its own.
    pub protocol_version: u32,
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether `value` is a lowercase hex SHA-256. Uppercase is refused rather than
/// normalised: the digest is compared byte-for-byte, so accepting two spellings
/// of it would make "the digest matches" mean two things.
pub fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `ETag` for a digest, in the shape the conditional request is sent back
/// in: a strong validator derived from the content.
pub fn etag_for(digest: &str) -> String {
    format!("\"sha256-{digest}\"")
}

/// Extracts a digest from an `If-None-Match` value.
///
/// Accepts the value [`etag_for`] produces, with or without quotes and the
/// `sha256-` prefix, plus a comma-separated list (a client is allowed to offer
/// several validators). Returns the first entry that looks like a digest.
pub fn digest_from_if_none_match(value: &str) -> Option<&str> {
    for candidate in value.split(',') {
        let candidate = candidate.trim();
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        let candidate = candidate.trim_matches('"');
        let candidate = candidate.strip_prefix("sha256-").unwrap_or(candidate);
        if valid_sha256_hex(candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Whether a target triple is one this server will look up.
///
/// A triple is `x86_64-unknown-linux-musl`: ASCII alphanumerics, `-` and `_`
/// only. Refusing `/`, `.` and everything else is what makes the lookup a
/// filename join that cannot escape the artifact directory.
pub fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 64
        && target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One artifact found on disk, with the digest of the bytes as they are now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedArtifact {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Number of bytes in it.
    pub len: u64,
    /// Lowercase hex SHA-256 of its contents.
    pub sha256: String,
}

/// Why an artifact lookup failed.
#[derive(Debug)]
pub enum ArtifactError {
    /// The requested target is not a triple this server will look up.
    UnknownTarget(String),
    /// No artifact directory was resolvable, so nothing can be hosted.
    NotConfigured,
    /// The directory exists but holds no binary for this target.
    Missing {
        /// The requested triple.
        target: String,
        /// The directory that was searched.
        dir: PathBuf,
    },
    /// The file could not be read.
    Io(String),
}

impl ArtifactError {
    /// The HTTP status a client should see.
    pub fn status(&self) -> StatusCode {
        match self {
            // A malformed target is the client's mistake.
            ArtifactError::UnknownTarget(_) => StatusCode::BAD_REQUEST,
            // Everything else is "this server does not have that binary",
            // which is a plain 404 a worker retries on a later attempt.
            ArtifactError::NotConfigured | ArtifactError::Missing { .. } => StatusCode::NOT_FOUND,
            ArtifactError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The JSON error code.
    pub fn code(&self) -> &'static str {
        match self {
            ArtifactError::UnknownTarget(_) => "invalid_request",
            ArtifactError::NotConfigured | ArtifactError::Missing { .. } => "not_found",
            ArtifactError::Io(_) => "internal_error",
        }
    }
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtifactError::UnknownTarget(target) => {
                write!(
                    f,
                    "`{target}` is not a target triple this server will serve"
                )
            }
            ArtifactError::NotConfigured => f.write_str(
                "this server hosts no artifacts: --artifact-dir (or a loom-worker next to the \
                 running loom) is required for worker self-update",
            ),
            ArtifactError::Missing { target, dir } => write!(
                f,
                "no worker binary for {target} under {}; expected loom-worker-{target} or \
                 loom-worker",
                dir.display()
            ),
            ArtifactError::Io(message) => f.write_str(message),
        }
    }
}

/// The directory a server hosts worker binaries from, with a digest cache.
#[derive(Debug)]
pub struct Artifacts {
    dir: Option<PathBuf>,
    /// The running binary, used as the artifact for this server's own target
    /// when no `loom-worker` name sits beside it. One installed file is both
    /// roles, so it is also the worker binary.
    self_exe: Option<PathBuf>,
    server_target: &'static str,
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

#[derive(Clone, Debug)]
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    sha256: String,
}

impl Artifacts {
    /// Resolves the directory: `--artifact-dir` when configured, otherwise the
    /// directory holding the running `loom`.
    ///
    /// That directory is where a deployment would put `loom-worker-<triple>`
    /// files for other architectures. For this server's own target it usually
    /// holds nothing extra, because [`Artifacts::resolve`] falls back to the
    /// running binary — one installed file is both roles.
    pub fn from_config(dir: Option<PathBuf>) -> Self {
        let self_exe = std::env::current_exe().ok();
        let dir = dir.or_else(|| {
            self_exe
                .as_ref()
                .and_then(|exe| exe.parent().map(Path::to_path_buf))
        });
        Self {
            dir,
            self_exe,
            server_target: crate::TARGET,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The triple this server was compiled for, and the default target.
    pub fn server_target(&self) -> &'static str {
        self.server_target
    }

    /// A human-readable description, for the startup log.
    pub fn describe(&self) -> String {
        match &self.dir {
            Some(dir) => format!(
                "worker artifacts from {} (target {})",
                dir.display(),
                self.server_target
            ),
            None => "no worker artifacts (self-update unavailable)".into(),
        }
    }

    /// Finds the worker binary for `target` and digests it.
    ///
    /// The digest is cached against the file's length and mtime, so a fleet of
    /// workers polling does not re-hash the same megabytes, while a redeployed
    /// binary is noticed immediately.
    pub fn resolve(&self, target: &str) -> Result<ResolvedArtifact, ArtifactError> {
        if !valid_target(target) {
            return Err(ArtifactError::UnknownTarget(target.to_owned()));
        }
        let dir = self.dir.as_ref().ok_or(ArtifactError::NotConfigured)?;
        // A named `loom-worker-<target>` or `loom-worker` beside the server
        // wins; for this server's own target, the running binary is the same
        // file in its worker role, so it is the fallback that lets a single
        // installed `loom` host its own self-update with no second name.
        let path = match locate(dir, target, self.server_target) {
            Some(path) => path,
            None if target == self.server_target => self
                .self_exe
                .clone()
                .filter(|path| path.is_file())
                .ok_or_else(|| ArtifactError::Missing {
                    target: target.to_owned(),
                    dir: dir.clone(),
                })?,
            None => {
                return Err(ArtifactError::Missing {
                    target: target.to_owned(),
                    dir: dir.clone(),
                })
            }
        };

        let metadata = std::fs::metadata(&path)
            .map_err(|error| ArtifactError::Io(format!("{path:?}: {error}")))?;
        let len = metadata.len();
        let modified = metadata.modified().ok();

        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.get(&path) {
            if hit.len == len && hit.modified == modified {
                return Ok(ResolvedArtifact {
                    path: path.clone(),
                    len: hit.len,
                    sha256: hit.sha256.clone(),
                });
            }
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| ArtifactError::Io(format!("{}: {error}", path.display())))?;
        let sha256 = sha256_hex(&bytes);
        cache.insert(
            path.clone(),
            Cached {
                len,
                modified,
                sha256: sha256.clone(),
            },
        );
        Ok(ResolvedArtifact { path, len, sha256 })
    }
}

/// The file for a target, preferring the release page's named asset.
///
/// `loom-worker-<triple>` is how a release publishes it; the unnamed
/// `loom-worker` is how `install.sh` lays it down, and it is only a valid answer
/// for the triple this server itself runs on.
fn locate(dir: &Path, target: &str, server_target: &str) -> Option<PathBuf> {
    let named = dir.join(format!("loom-worker-{target}"));
    if named.is_file() {
        return Some(named);
    }
    if target == server_target {
        let unnamed = dir.join("loom-worker");
        if unnamed.is_file() {
            return Some(unnamed);
        }
    }
    None
}

fn artifact_error(error: ArtifactError) -> Response {
    (
        error.status(),
        Json(json!({ "code": error.code(), "message": error.to_string() })),
    )
        .into_response()
}

/// Query fields for the artifact route.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct InstallQuery {
    /// The target triple to serve. Omitted means "this server's own target".
    #[serde(default)]
    pub target: Option<String>,
}

/// Reports the server's version and protocol version.
pub async fn install_version() -> Json<InstallVersion> {
    Json(InstallVersion {
        version: crate::VERSION.to_owned(),
        protocol_version: crate::PROTOCOL_VERSION,
    })
}

/// Serves one worker binary, or a `304` when the caller already has it.
///
/// The conditional request is the point: a worker reconnects whenever it likes,
/// and without `If-None-Match` every reconnect of every machine would move
/// megabytes. The worker remembers the digest it installed and sends it; an
/// unchanged artifact costs a `304` and no body.
pub async fn install_worker(
    State(state): State<AppState>,
    Query(query): Query<InstallQuery>,
    headers: HeaderMap,
) -> Response {
    let target = query
        .target
        .unwrap_or_else(|| state.artifacts.server_target().to_owned());
    let artifact = match state.artifacts.resolve(&target) {
        Ok(artifact) => artifact,
        Err(error) => return artifact_error(error),
    };

    let etag = etag_for(&artifact.sha256);
    let mut response_headers = HeaderMap::new();
    // Both headers on both responses: a client that receives a `304` still
    // learns which digest it just proved it has, and can persist it.
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response_headers.insert(header::ETAG, value);
    }
    if let Ok(value) = HeaderValue::from_str(&artifact.sha256) {
        response_headers.insert(HeaderName::from_static(DIGEST_HEADER), value);
    }

    let already_installed = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(digest_from_if_none_match)
        .is_some_and(|digest| digest == artifact.sha256);
    if already_installed {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }

    let bytes = match tokio::fs::read(&artifact.path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return artifact_error(ArtifactError::Io(format!(
                "{}: {error}",
                artifact.path.display()
            )))
        }
    };
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&bytes.len().to_string()) {
        response_headers.insert(header::CONTENT_LENGTH, value);
    }
    (StatusCode::OK, response_headers, bytes).into_response()
}

/* ------------------------------------------------------------------ */
/* Client side                                                         */
/* ------------------------------------------------------------------ */

/// What `GET /install/loom-worker` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactDownload {
    /// The server's artifact is the digest the caller already has: no body.
    NotModified,
    /// The bytes, plus the digest the server says they have.
    Artifact {
        /// Lowercase hex SHA-256 from [`DIGEST_HEADER`].
        digest: String,
        /// The body, exactly as received.
        bytes: Vec<u8>,
    },
}

/// A minimal HTTP client for the two install routes.
///
/// Plain `http://` only, like the worker's WebSocket transport: neither binary
/// is built with a TLS client, and the deployment shape puts TLS in front
/// (`docs/remote-access.md`). Asking for `https://` says so rather than failing
/// somewhere less obvious.
pub struct ArtifactClient {
    origin: String,
    client: Client<HttpConnector, Body>,
}

impl std::fmt::Debug for ArtifactClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactClient")
            .field("origin", &self.origin)
            .finish()
    }
}

impl ArtifactClient {
    /// Builds a client for a server URL as an operator would write it —
    /// `http://host:port`, or a `ws://` URL, which is normalised.
    pub fn new(server_url: &str) -> Result<Self, String> {
        let origin = http_origin(server_url)?;
        let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build_http();
        Ok(Self { origin, client })
    }

    /// `GET /install/version`.
    pub async fn install_version(&self) -> Result<InstallVersion, String> {
        let response = self
            .get(&format!("{}{}", self.origin, INSTALL_VERSION_PATH), None)
            .await?;
        let status = response.status();
        let bytes = collect(response).await?;
        if !status.is_success() {
            return Err(format!(
                "GET {INSTALL_VERSION_PATH} answered {status}: {}",
                snippet(&bytes)
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("GET {INSTALL_VERSION_PATH} returned invalid JSON: {error}"))
    }

    /// `GET /install/loom-worker?target=<target>`, optionally conditional.
    pub async fn artifact(
        &self,
        target: &str,
        if_none_match: Option<&str>,
    ) -> Result<ArtifactDownload, String> {
        if !valid_target(target) {
            return Err(format!("`{target}` is not a valid target triple"));
        }
        let url = format!(
            "{}{}?{TARGET_QUERY}={target}",
            self.origin, INSTALL_WORKER_PATH
        );
        let response = self.get(&url, if_none_match).await?;
        let status = response.status();

        if status == StatusCode::NOT_MODIFIED {
            return Ok(ArtifactDownload::NotModified);
        }

        let digest = response
            .headers()
            .get(DIGEST_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = collect(response).await?;
        if !status.is_success() {
            return Err(format!("GET {url} answered {status}: {}", snippet(&bytes)));
        }
        let Some(digest) = digest else {
            return Err(format!(
                "the server served an artifact without a {DIGEST_HEADER} header; refusing to \
                 install an unverifiable download"
            ));
        };
        if !valid_sha256_hex(&digest) {
            return Err(format!(
                "the server's {DIGEST_HEADER} is not a SHA-256 digest: {digest:?}"
            ));
        }
        Ok(ArtifactDownload::Artifact { digest, bytes })
    }

    async fn get(
        &self,
        url: &str,
        if_none_match: Option<&str>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let uri: hyper::Uri = url
            .parse()
            .map_err(|error| format!("could not parse {url} as a URL: {error}"))?;
        let mut request = hyper::Request::builder().method("GET").uri(uri);
        if let Some(etag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, etag_for(etag));
        }
        let request = request
            .body(Body::empty())
            .map_err(|error| format!("could not build the request: {error}"))?;
        self.client
            .request(request)
            .await
            .map_err(|error| format!("{url}: {error}"))
    }
}

/// Turns a server URL into an `http`/`https` origin.
fn http_origin(server_url: &str) -> Result<String, String> {
    let trimmed = server_url.trim().trim_end_matches('/');
    let origin = if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_owned()
    };
    let origin = origin
        .strip_suffix("/internal/ws")
        .or_else(|| origin.strip_suffix("/ws"))
        .unwrap_or(&origin)
        .to_owned();

    if origin.starts_with("https://") {
        return Err(
            "this binary has no TLS client, so it cannot fetch an artifact over https; put the \
             server behind a tailnet or a proxy that speaks plain http to the worker"
                .into(),
        );
    }
    // A bare `host:port` is what `WorkerConfig::websocket_url` also accepts, so
    // an operator who typed one for `--server-url` is not asked to type it
    // twice.
    if origin.contains("://") {
        return Ok(origin);
    }
    Ok(format!("http://{origin}"))
}

async fn collect(response: hyper::Response<hyper::body::Incoming>) -> Result<Vec<u8>, String> {
    use http_body_util::BodyExt;
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|error| format!("could not read the response body: {error}"))?;
    Ok(bytes.to_bytes().to_vec())
}

/// A body excerpt for an error message, bounded so a huge body cannot end up in
/// a log line.
fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_lowercase_hex_of_the_right_length() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(valid_sha256_hex(&sha256_hex(b"loom")));
        assert!(!valid_sha256_hex(""), "an empty digest is not one");
        assert!(!valid_sha256_hex(&"A".repeat(64)), "uppercase is refused");
        assert!(!valid_sha256_hex(&"z".repeat(64)));
        assert!(!valid_sha256_hex(&"0".repeat(63)));
    }

    #[test]
    fn the_etag_round_trips_and_tolerates_a_list() {
        let digest = sha256_hex(b"artifact");
        let etag = etag_for(&digest);
        assert_eq!(digest_from_if_none_match(&etag), Some(digest.as_str()));
        // What some proxies and clients send instead.
        assert_eq!(
            digest_from_if_none_match(&format!("\"sha256-{digest}\"")),
            Some(digest.as_str())
        );
        assert_eq!(
            digest_from_if_none_match(&format!("W/\"sha256-{digest}\"")),
            Some(digest.as_str())
        );
        assert_eq!(
            digest_from_if_none_match(&format!("\"other\", \"sha256-{digest}\"")),
            Some(digest.as_str())
        );
        assert_eq!(digest_from_if_none_match("\"unrelated\""), None);
        assert_eq!(digest_from_if_none_match("*"), None);
    }

    #[test]
    fn a_target_cannot_escape_the_artifact_directory() {
        assert!(valid_target("x86_64-unknown-linux-musl"));
        assert!(valid_target("aarch64-unknown-linux-musl"));
        assert!(!valid_target(""));
        assert!(!valid_target("../../etc/passwd"));
        assert!(!valid_target("x86_64/../../etc/passwd"));
        assert!(!valid_target("a.b"));
        assert!(!valid_target(&"x".repeat(65)));
    }

    #[test]
    fn the_named_asset_wins_and_the_unnamed_one_is_only_for_this_server() {
        let dir = tempfile::tempdir().unwrap();
        // The unnamed binary answers only for the running server's own triple,
        // whichever triple the test binary was built for.
        let server_target = Artifacts::from_config(None).server_target();
        std::fs::write(dir.path().join("loom-worker"), b"installed").unwrap();
        assert_eq!(
            locate(dir.path(), server_target, server_target),
            Some(dir.path().join("loom-worker"))
        );
        assert_eq!(
            locate(dir.path(), "aarch64-unknown-linux-musl", server_target),
            None,
            "the unnamed binary is not an answer for another machine"
        );

        std::fs::write(
            dir.path().join("loom-worker-aarch64-unknown-linux-musl"),
            b"arm",
        )
        .unwrap();
        assert_eq!(
            locate(dir.path(), "aarch64-unknown-linux-musl", server_target),
            Some(dir.path().join("loom-worker-aarch64-unknown-linux-musl"))
        );
    }

    #[test]
    fn resolving_a_deployed_binary_digests_it_and_notices_a_change() {
        let dir = tempfile::tempdir().unwrap();
        // A named asset, so the test does not depend on which triple the test
        // binary itself was built for.
        let assets = Artifacts::from_config(Some(dir.path().to_path_buf()));
        let target = assets.server_target();
        let path = dir.path().join(format!("loom-worker-{target}"));
        std::fs::write(&path, b"first").unwrap();

        let first = assets.resolve(target).unwrap();
        assert_eq!(first.sha256, sha256_hex(b"first"));
        assert_eq!(first.len, 5);

        // The cache returns the same answer for an unchanged file.
        assert_eq!(assets.resolve(target).unwrap(), first);

        // A redeployed binary of a different length is re-digested.
        std::fs::write(&path, b"a longer body").unwrap();
        let second = assets.resolve(target).unwrap();
        assert_eq!(second.sha256, sha256_hex(b"a longer body"));
        assert_ne!(second.sha256, first.sha256);
    }

    #[test]
    fn the_running_binary_is_served_when_no_worker_name_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Artifacts::from_config(Some(dir.path().to_path_buf()));
        let target = artifacts.server_target();

        // Nothing named `loom-worker*` is in the directory, so this server's own
        // target is served from the running binary: one installed `loom` is both
        // roles, and a single file hosts its own worker self-update.
        let resolved = artifacts.resolve(target).unwrap();
        assert_eq!(resolved.path, std::env::current_exe().unwrap());
        assert!(resolved.len > 0);

        // Another architecture is a different file and still has to be present.
        let other = if target == "aarch64-unknown-linux-musl" {
            "x86_64-unknown-linux-musl"
        } else {
            "aarch64-unknown-linux-musl"
        };
        assert!(artifacts.resolve(other).is_err());
    }

    #[test]
    fn a_missing_artifact_names_what_was_searched_for() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Artifacts::from_config(Some(dir.path().to_path_buf()));
        let error = artifacts.resolve("aarch64-unknown-linux-musl").unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        let message = error.to_string();
        assert!(message.contains("aarch64-unknown-linux-musl"), "{message}");
        assert!(
            message.contains("loom-worker-aarch64-unknown-linux-musl"),
            "{message}"
        );
    }

    #[test]
    fn http_origins_are_normalised_and_https_is_refused() {
        assert_eq!(
            http_origin("http://127.0.0.1:38886").unwrap(),
            "http://127.0.0.1:38886"
        );
        assert_eq!(http_origin("http://host:1/ws").unwrap(), "http://host:1");
        assert_eq!(
            http_origin("ws://host:1/internal/ws").unwrap(),
            "http://host:1"
        );
        assert_eq!(http_origin("ws://host:1").unwrap(), "http://host:1");
        assert_eq!(http_origin("host:1").unwrap(), "http://host:1");
        assert!(http_origin("https://loom.example.com").is_err());
        assert!(http_origin("wss://loom.example.com").is_err());
    }
}

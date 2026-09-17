//! Static UI hosting.
//!
//! The server is the only thing a client needs to know about: it serves the UI
//! bundle and the API from the same origin, so the UI derives its server
//! address from `window.location.origin` and has nothing to configure. See
//! `docs/ui.md` for the client contract.
//!
//! Two sources, picked by configuration and mutually exclusive — and no
//! default, because there is exactly one client and a server that cannot serve
//! it must say so instead of quietly serving something else:
//!
//! * **Directory** (`LOOM_UI_DIR`) — the built product bundle on disk. This is
//!   the production shape: `deploy/install.sh` installs the app's build at
//!   `<prefix>/share/loom/ui` and points this at it.
//! * **Proxy** (`LOOM_UI_PROXY`) — development only: reverse-proxy to the
//!   frontend dev server, so the UI can be edited with hot reload against the
//!   real server.
//!
//! A buildless reference client used to be compiled in here. It is gone
//! (W-586 / W-588): two clients meant two behaviours to keep in step, and the
//! one that shipped by default was the one nobody used.
//!
//! Everything here is deliberate about one thing: an unmatched *client* path
//! falls back to `index.html`, but an unmatched `/api`, `/ws` or `/internal/ws`
//! path must not,
//! or a mistyped API route would answer with an HTML page.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// Where the server answers a UI request from.
#[derive(Clone)]
pub struct Ui {
    source: UiSource,
}

#[derive(Clone)]
enum UiSource {
    Directory(PathBuf),
    Proxy(Arc<ProxyClient>),
    Disabled,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            UiSource::Directory(path) => f.debug_tuple("Ui::Directory").field(path).finish(),
            UiSource::Proxy(proxy) => f.debug_tuple("Ui::Proxy").field(proxy).finish(),
            UiSource::Disabled => f.write_str("Ui::Disabled"),
        }
    }
}

impl Ui {
    /// Resolves the configured source.
    ///
    /// `dir` and `proxy` are mutually exclusive: two UI sources would make the
    /// behaviour depend on evaluation order, which is not something an operator
    /// should have to reason about.
    ///
    /// Neither configured is an error rather than a built-in client. The one
    /// exception is [`Ui::disabled`], which a library embedding the server —
    /// and a test that asserts on the API alone — asks for explicitly.
    pub fn from_config(dir: Option<PathBuf>, proxy: Option<String>) -> Result<Self, String> {
        let source = match (dir, proxy) {
            (Some(_), Some(_)) => {
                return Err(
                    "LOOM_UI_DIR and LOOM_UI_PROXY are both set; choose one UI source".into(),
                )
            }
            (_, Some(base)) => UiSource::Proxy(Arc::new(ProxyClient::new(&base)?)),
            (Some(path), None) => UiSource::Directory(path),
            (None, None) => {
                return Err(
                    "no UI source is configured: set LOOM_UI_DIR to a built product bundle \
                     (deploy/install.sh installs one under <prefix>/share/loom/ui), or \
                     LOOM_UI_PROXY to a frontend dev server"
                        .into(),
                )
            }
        };
        Ok(Self { source })
    }

    /// Serves nothing. Used by tests that assert on API behaviour only.
    pub fn disabled() -> Self {
        Self {
            source: UiSource::Disabled,
        }
    }

    /// A human-readable description of the active source, for the startup log.
    pub fn describe(&self) -> String {
        match &self.source {
            UiSource::Directory(path) => format!("bundle at {}", path.display()),
            UiSource::Proxy(proxy) => format!("dev-server proxy {}", proxy.base),
            UiSource::Disabled => "disabled".into(),
        }
    }

    /// Answers one request that fell through the API routes.
    pub async fn handle(self, request: Request) -> Response {
        let path = request.uri().path().to_owned();

        // A client route is allowed to fall back to index.html; an API or
        // socket path is not. Otherwise `GET /api/v1/typo` would return the
        // SPA shell with a 200, which is exactly the kind of failure that is
        // hard to diagnose from the browser.
        if path == "/ws" || path == "/internal/ws" || path == "/api" || path.starts_with("/api/") {
            return not_found();
        }

        match self.source {
            UiSource::Disabled => not_found(),
            UiSource::Directory(root) => directory_response(&root, &path, request.method()).await,
            UiSource::Proxy(proxy) => proxy.forward(request).await,
        }
    }
}

/// Axum fallback handler.
pub async fn serve(State(state): State<AppState>, request: Request) -> Response {
    state.ui.clone().handle(request).await
}

/* ------------------------------------------------------------------ */
/* Directory bundle                                                    */
/* ------------------------------------------------------------------ */

async fn directory_response(root: &Path, path: &str, method: &Method) -> Response {
    if !is_read(method) {
        return not_found();
    }
    let Some(relative) = sanitize(path) else {
        return not_found();
    };
    let candidate = root.join(&relative);
    if let Ok(bytes) = tokio::fs::read(&candidate).await {
        let content_type = content_type_for(&candidate);
        return asset_response(Body::from(bytes), content_type, cache_for(path));
    }
    // A client-side route (no file extension) falls back to the SPA shell.
    if is_client_route(path) {
        let index = root.join("index.html");
        if let Ok(bytes) = tokio::fs::read(&index).await {
            return asset_response(Body::from(bytes), HTML, "no-cache");
        }
    }
    not_found()
}

/// Maps a request path to a path under the UI root, rejecting traversal.
///
/// Nothing is served from outside `root`: `..`, absolute paths and prefixes are
/// refused rather than normalised, because a silently normalised path is a
/// sandbox escape waiting for the next refactor.
fn sanitize(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim_start_matches('/');
    let mut out = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    // `/` and `/foo/` resolve to their `index.html`.
    if out.as_os_str().is_empty() || path.ends_with('/') {
        out.push("index.html");
    }
    Some(out)
}

/// Whether a path may fall back to the SPA shell. Files with an extension are
/// requested assets and must 404 honestly.
fn is_client_route(path: &str) -> bool {
    Path::new(path).extension().is_none()
}

fn is_read(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD
}

fn asset_response(body: Body, content_type: &'static str, cache: &'static str) -> Response {
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" | "htm" => HTML,
        "js" | "mjs" => JS,
        "css" => CSS,
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// Content-hashed build assets are immutable; everything else is revalidated so
/// a redeploy is picked up without a hard refresh.
fn cache_for(path: &str) -> &'static str {
    if path.ends_with("index.html") || path.ends_with('/') || path == "/" {
        "no-cache"
    } else if path.contains("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

/* ------------------------------------------------------------------ */
/* Dev-server reverse proxy                                            */
/* ------------------------------------------------------------------ */

/// The development shape: forward unmatched requests to a frontend dev server.
///
/// WebSocket upgrades are tunnelled too, so Vite's HMR socket keeps working
/// while `/api`, `/ws` and `/internal/ws` stay on loom-server. The client therefore needs no
/// CORS configuration and no second origin.
pub struct ProxyClient {
    base: Uri,
    client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        Body,
    >,
}

impl std::fmt::Debug for ProxyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyClient")
            .field("base", &self.base.to_string())
            .finish()
    }
}

impl ProxyClient {
    /// Builds a client for an absolute base URL, e.g. `http://127.0.0.1:5173`.
    pub fn new(base: &str) -> Result<Self, String> {
        let base: Uri = base
            .trim()
            .parse()
            .map_err(|error| format!("invalid LOOM_UI_PROXY url: {error}"))?;
        if base.scheme().is_none() || base.authority().is_none() {
            return Err("LOOM_UI_PROXY must be an absolute URL, e.g. http://127.0.0.1:5173".into());
        }
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http();
        Ok(Self { base, client })
    }

    async fn forward(self: Arc<Self>, request: Request) -> Response {
        // An upgrade request has an empty body, so its parts can be cloned
        // before the request is consumed by `hyper::upgrade::on`.
        if request.headers().get(header::UPGRADE).is_some() {
            let method = request.method().clone();
            let uri = request.uri().clone();
            let headers = request.headers().clone();
            let incoming = hyper::upgrade::on(request);
            return self
                .forward_parts(method, uri, headers, Body::empty(), Some(incoming))
                .await;
        }
        let (parts, body) = request.into_parts();
        self.forward_parts(parts.method, parts.uri, parts.headers, body, None)
            .await
    }

    async fn forward_parts(
        self: &Arc<Self>,
        method: Method,
        uri: Uri,
        headers: axum::http::HeaderMap,
        body: Body,
        incoming: Option<hyper::upgrade::OnUpgrade>,
    ) -> Response {
        let authority = match self.base.authority() {
            Some(authority) => authority.clone(),
            None => return error_response(StatusCode::BAD_GATEWAY, "proxy base has no authority"),
        };
        let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let target = format!(
            "{}{}",
            self.base.to_string().trim_end_matches('/'),
            path_and_query
        );
        let target: Uri = match target.parse() {
            Ok(target) => target,
            Err(error) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("proxy target is not a valid URL: {error}"),
                )
            }
        };

        let mut upstream = Request::new(body);
        *upstream.method_mut() = method;
        *upstream.uri_mut() = target;
        // Dev servers speak HTTP/1.1; a tunnelled upgrade must not be
        // downgraded to HTTP/2, which has no `Upgrade` mechanism.
        *upstream.version_mut() = axum::http::Version::HTTP_11;
        *upstream.headers_mut() = headers;
        upstream.headers_mut().insert(
            header::HOST,
            HeaderValue::from_str(authority.as_str())
                .unwrap_or_else(|_| HeaderValue::from_static("localhost")),
        );

        let response = match self.client.request(upstream).await {
            Ok(response) => response,
            Err(error) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("ui proxy could not reach {}: {error}", self.base),
                )
            }
        };

        let status = response.status();
        let response_headers = response.headers().clone();

        if status == StatusCode::SWITCHING_PROTOCOLS {
            let upstream = hyper::upgrade::on(response);
            if let Some(incoming) = incoming {
                tokio::spawn(tunnel(incoming, upstream));
            }
            let mut out = Response::new(Body::empty());
            *out.status_mut() = status;
            *out.headers_mut() = response_headers;
            return out;
        }

        let mut out = Response::new(Body::new(response.into_body()));
        *out.status_mut() = status;
        *out.headers_mut() = response_headers;
        out
    }
}

/// Copies bytes in both directions until either side closes.
async fn tunnel(incoming: hyper::upgrade::OnUpgrade, upstream: hyper::upgrade::OnUpgrade) {
    let Ok((incoming, upstream)) = tokio::try_join!(incoming, upstream) else {
        return;
    };
    let mut incoming = hyper_util::rt::TokioIo::new(incoming);
    let mut upstream = hyper_util::rt::TokioIo::new(upstream);
    let _ = tokio::io::copy_bidirectional(&mut incoming, &mut upstream).await;
}

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "error": message.into() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;

    async fn body_text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn an_unconfigured_server_refuses_to_serve_a_client() {
        // There is no built-in client to fall back to, so "no source" is a
        // configuration error the operator has to fix — not a quiet default
        // that serves something nobody asked for.
        let error = Ui::from_config(None, None).expect_err("no source must be an error");
        assert!(error.contains("LOOM_UI_DIR"), "{error}");
        assert!(error.contains("LOOM_UI_PROXY"), "{error}");
    }

    #[tokio::test]
    async fn client_routes_fall_back_to_the_shell_but_api_paths_do_not() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            "<title>loom</title><script src=\"/assets/app.js\"></script>",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").unwrap();
        let ui = Ui::from_config(Some(dir.path().to_path_buf()), None).unwrap();

        let client = ui
            .clone()
            .handle(
                Request::builder()
                    .uri("/threads/thr_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(client.status(), StatusCode::OK);
        assert_eq!(
            client.headers()[header::CONTENT_TYPE],
            HeaderValue::from_static(HTML)
        );
        assert!(body_text(client).await.contains("<title>loom</title>"));

        let api = ui
            .handle(
                Request::builder()
                    .uri("/api/v1/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(api.status(), StatusCode::NOT_FOUND);

        // A missing *asset* is a 404, not the shell.
        let ui = Ui::from_config(Some(dir.path().to_path_buf()), None).unwrap();
        let asset = ui
            .handle(
                Request::builder()
                    .uri("/missing.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(asset.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_directory_source_serves_the_built_bundle() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("index.html"), "<title>built</title>").unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/app-abc123.js"), "console.log(1)").unwrap();

        let ui = Ui::from_config(Some(dir.path().to_path_buf()), None).unwrap();
        let index = ui
            .clone()
            .handle(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await;
        assert!(body_text(index).await.contains("built"));

        let asset = ui
            .clone()
            .handle(
                Request::builder()
                    .uri("/assets/app-abc123.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(asset.headers()[header::CONTENT_TYPE], JS);
        assert!(asset.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));

        let route = ui
            .handle(
                Request::builder()
                    .uri("/some/client/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert!(body_text(route).await.contains("built"));
    }

    #[tokio::test]
    async fn traversal_out_of_the_ui_root_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("index.html"), "<title>built</title>").unwrap();
        let secret = dir.path().parent().unwrap().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();

        let ui = Ui::from_config(Some(dir.path().to_path_buf()), None).unwrap();
        let response = ui
            .handle(
                Request::builder()
                    .uri("/../secret.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let _ = std::fs::remove_file(secret);
    }

    #[tokio::test]
    async fn the_two_ui_sources_are_mutually_exclusive() {
        let error =
            Ui::from_config(Some(PathBuf::from("/tmp/ui")), Some("http://x".into())).unwrap_err();
        assert!(error.contains("choose one UI source"));
    }

    #[tokio::test]
    async fn a_relative_proxy_url_is_rejected() {
        assert!(Ui::from_config(None, Some("127.0.0.1:5173".into())).is_err());
    }

    #[tokio::test]
    async fn the_proxy_forwards_http_requests_to_the_dev_server() {
        use axum::routing::get;
        use std::net::SocketAddr;

        let upstream = axum::Router::new().route(
            "/hello.js",
            get(|| async { ([("content-type", JS)], "export const hello = 1;") }),
        );
        let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        let ui = Ui::from_config(None, Some(format!("http://{addr}"))).unwrap();
        let response = ui
            .handle(
                Request::builder()
                    .uri("/hello.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "export const hello = 1;");
    }
}

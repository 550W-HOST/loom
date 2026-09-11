//! End-to-end tests over a real WebSocket and a real HTTP listener.
//!
//! These are the tests that matter for the relay: they exercise the full path
//! producer -> log -> fixed reader -> hub -> socket, with nothing stubbed.

use std::time::Duration;

use loom_relay::Scope;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const TIMEOUT: Duration = Duration::from_secs(3);

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

struct Client {
    socket: Socket,
}

impl Client {
    /// Connects and consumes the welcome frame.
    async fn connect(addr: &str) -> Self {
        let (socket, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let mut client = Self { socket };
        let welcome = client.recv().await;
        assert_eq!(welcome["type"], "welcome");
        client
    }

    async fn send(&mut self, value: Value) {
        use futures_util::SinkExt;
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    async fn subscribe(&mut self, scope: Scope) {
        self.send(json!({ "type": "subscribe", "scope": scope }))
            .await;
        let ack = self.recv().await;
        assert_eq!(ack["type"], "subscribed", "unexpected ack: {ack}");
    }

    async fn unsubscribe(&mut self, scope: Scope) {
        self.send(json!({ "type": "unsubscribe", "scope": scope }))
            .await;
        let ack = self.recv().await;
        assert_eq!(ack["type"], "unsubscribed", "unexpected ack: {ack}");
    }

    async fn recv(&mut self) -> Value {
        let message = tokio::time::timeout(TIMEOUT, {
            use futures_util::StreamExt;
            self.socket.next()
        })
        .await
        .expect("timed out waiting for a frame")
        .expect("socket closed")
        .expect("socket error");
        match message {
            Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    /// Returns `None` when nothing arrives within `window`.
    async fn try_recv(&mut self, window: Duration) -> Option<Value> {
        let message = tokio::time::timeout(window, {
            use futures_util::StreamExt;
            self.socket.next()
        })
        .await
        .ok()?
        .expect("socket closed")
        .expect("socket error");
        match message {
            Message::Text(text) => Some(serde_json::from_str(text.as_str()).unwrap()),
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn two_clients_in_one_scope_both_receive() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());

    let mut a = Client::connect(&addr).await;
    let mut b = Client::connect(&addr).await;
    a.subscribe(scope.clone()).await;
    b.subscribe(scope.clone()).await;

    state.publish(scope, "{\"n\":1}").unwrap();

    for client in [&mut a, &mut b] {
        let event = client.recv().await;
        assert_eq!(event["type"], "event");
        assert_eq!(event["payload"], "{\"n\":1}");
        assert_eq!(event["scope"]["kind"], "thread");
        assert_eq!(event["scope"]["id"], "thr_1");
        assert_eq!(event["event_id"].as_str().unwrap().len(), 26);
    }

    state.shutdown();
}

#[tokio::test]
async fn a_client_only_receives_its_own_scopes() {
    let (addr, state) = spawn_server().await;
    let subscribed = Scope::Thread("thr_1".into());
    let other = Scope::Thread("thr_2".into());

    let mut client = Client::connect(&addr).await;
    client.subscribe(subscribed.clone()).await;

    state.publish(other, "{\"other\":true}").unwrap();
    state.publish(subscribed, "{\"mine\":true}").unwrap();

    // The first frame the client sees must be its own, never the other scope's.
    let event = client.recv().await;
    assert_eq!(event["payload"], "{\"mine\":true}");

    assert!(
        client.try_recv(Duration::from_millis(300)).await.is_none(),
        "a client must not receive another scope's frames"
    );

    state.shutdown();
}

#[tokio::test]
async fn events_arrive_in_publication_order() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());

    let mut client = Client::connect(&addr).await;
    client.subscribe(scope.clone()).await;

    for n in 0..8 {
        state
            .publish(scope.clone(), format!("{{\"n\":{n}}}"))
            .unwrap();
    }

    let mut ids = Vec::new();
    let mut payloads = Vec::new();
    for _ in 0..8 {
        let event = client.recv().await;
        ids.push(event["event_id"].as_str().unwrap().to_string());
        payloads.push(event["payload"].as_str().unwrap().to_string());
    }

    assert_eq!(
        payloads,
        (0..8).map(|n| format!("{{\"n\":{n}}}")).collect::<Vec<_>>()
    );
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        ids, sorted,
        "event ids must be delivered in ascending order"
    );

    state.shutdown();
}

#[tokio::test]
async fn subscribing_twice_delivers_once() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());

    let mut client = Client::connect(&addr).await;
    client.subscribe(scope.clone()).await;
    client.subscribe(scope.clone()).await;

    state.publish(scope, "{\"n\":1}").unwrap();

    let event = client.recv().await;
    assert_eq!(event["payload"], "{\"n\":1}");
    assert!(
        client.try_recv(Duration::from_millis(300)).await.is_none(),
        "a duplicate subscription must not duplicate delivery"
    );

    state.shutdown();
}

#[tokio::test]
async fn unsubscribe_stops_delivery_without_closing_the_socket() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());
    let kept = Scope::Thread("thr_2".into());

    let mut client = Client::connect(&addr).await;
    client.subscribe(scope.clone()).await;
    client.subscribe(kept.clone()).await;
    client.unsubscribe(scope.clone()).await;

    state.publish(scope, "{\"gone\":true}").unwrap();
    state.publish(kept, "{\"kept\":true}").unwrap();

    let event = client.recv().await;
    assert_eq!(event["payload"], "{\"kept\":true}");
    assert!(client.try_recv(Duration::from_millis(300)).await.is_none());

    state.shutdown();
}

#[tokio::test]
async fn replay_returns_the_retained_window_and_honours_a_cursor() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());

    let first = state.publish(scope.clone(), "{\"n\":1}").unwrap();
    state.publish(scope.clone(), "{\"n\":2}").unwrap();
    state.publish(scope.clone(), "{\"n\":3}").unwrap();

    let all = http_json(&addr, "/api/v1/replay?scope_kind=thread&scope_id=thr_1").await;
    let frames = all["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["type"], "event");
    assert_eq!(frames[0]["payload"], "{\"n\":1}");

    let since = format!(
        "/api/v1/replay?scope_kind=thread&scope_id=thr_1&since={}",
        first.event_id
    );
    let newer = http_json(&addr, &since).await;
    let frames = newer["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2, "the cursor is exclusive");
    assert_eq!(frames[0]["payload"], "{\"n\":2}");

    state.shutdown();
}

#[tokio::test]
async fn a_reconnecting_client_can_resume_from_a_cursor() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());

    // While no client is attached, work continues.
    let before = state.publish(scope.clone(), "{\"n\":1}").unwrap();
    state.publish(scope.clone(), "{\"n\":2}").unwrap();
    state.publish(scope.clone(), "{\"n\":3}").unwrap();

    let mut client = Client::connect(&addr).await;
    client.subscribe(scope.clone()).await;

    // Subscribe first, then fetch the backlog, then merge by event id. This is
    // the documented client flow and it cannot miss a frame.
    let path = format!(
        "/api/v1/replay?scope_kind=thread&scope_id=thr_1&since={}",
        before.event_id
    );
    let backlog = http_json(&addr, &path).await;
    let payloads: Vec<&str> = backlog["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|frame| frame["payload"].as_str().unwrap())
        .collect();
    assert_eq!(payloads, vec!["{\"n\":2}", "{\"n\":3}"]);

    // Live traffic resumes from here.
    state.publish(scope, "{\"n\":4}").unwrap();
    let live = client.recv().await;
    assert_eq!(live["payload"], "{\"n\":4}");

    state.shutdown();
}

#[tokio::test]
async fn a_malformed_command_is_reported_without_closing_the_socket() {
    let (addr, state) = spawn_server().await;
    let scope = Scope::Thread("thr_1".into());

    let mut client = Client::connect(&addr).await;
    client.send(json!({ "type": "not-a-command" })).await;
    let error = client.recv().await;
    assert_eq!(error["type"], "error");

    // The socket is still usable.
    client.subscribe(scope.clone()).await;
    state.publish(scope, "{\"n\":1}").unwrap();
    assert_eq!(client.recv().await["payload"], "{\"n\":1}");

    state.shutdown();
}

/// Minimal HTTP GET, so the test suite needs no HTTP client dependency.
async fn http_json(addr: &str, path: &str) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut response))
        .await
        .expect("HTTP response timed out")
        .unwrap();
    let text = String::from_utf8(response).unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .expect("response had no body separator")
        .1;
    serde_json::from_str(body).unwrap_or_else(|error| panic!("bad JSON body: {error}\n{body}"))
}

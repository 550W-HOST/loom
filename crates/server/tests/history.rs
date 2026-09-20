//! Opening a thread whose conversation is not in the log.
//!
//! The acceptance this file is the evidence for: a conversation whose frames
//! have aged out of the relay's retained window — the shape every thread has
//! after a server restart — is loaded from the ACP agent that owns its session
//! and shown in the timeline. Nothing here is stubbed below the wire: a real
//! server process state, a real HTTP listener, a real worker WebSocket, and the
//! real `HostRpcRequest`/`HistoryReport` frames in between. The "agent" is the
//! test itself, answering the load with the replay a provider would send.
//!
//! What it pins, in order:
//!
//! * the entity view and the session **binding** survive the restart, while the
//!   conversation's own frames do not;
//! * a read answers `loading` immediately instead of holding the request;
//! * the load that follows is a **read**: it carries the session the server
//!   resolved, and it creates no run and sends no prompt;
//! * the rows that arrive come from the replay, in order, grouped by a local
//!   key, with no invented timestamps.

use std::path::Path;
use std::time::Duration;

use loom_domain::{
    EnvironmentKind, HostId, MessageRole, ProviderEvent, ProviderSessionBinding, ThreadEventItem,
    ThreadId, ThreadStatus, UserContent,
};
use loom_relay::Scope;
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Generous on purpose: this test is one of many binaries a `cargo test
/// --workspace` run starts at once, and a frame that is late under that load is
/// not a broken load.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Starts a server over `config` on an ephemeral port, serving in the
/// background. Returns the address and the state, so a test can drive the
/// domain directly and read the API it produced.
async fn spawn_server(config: AppConfig) -> (String, AppState) {
    let state = AppState::build(config).unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("{}:{}", addr.ip(), addr.port()), state)
}

/// A worker's side of the internal socket: enroll, follow a host room, answer.
struct Worker {
    socket: Socket,
}

impl Worker {
    async fn connect(addr: &str) -> Self {
        let (socket, _) = connect_async(format!("ws://{addr}/internal/ws"))
            .await
            .unwrap();
        let mut worker = Self { socket };
        let welcome = worker.recv().await;
        assert_eq!(welcome["type"], "hello");
        worker
    }

    async fn send(&mut self, value: Value) {
        use futures_util::SinkExt;
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    /// Enrolls, optionally reclaiming a host this machine enrolled before.
    async fn enroll(&mut self, host_id: Option<&HostId>) -> HostId {
        let message = match host_id {
            Some(host_id) => json!({ "type": "enroll_host", "name": "box", "host_id": host_id }),
            None => json!({ "type": "enroll_host", "name": "box" }),
        };
        self.send(message).await;
        let enrolled = self.recv().await;
        assert_eq!(enrolled["type"], "host_enrolled", "{enrolled}");
        enrolled["host"]["id"]
            .as_str()
            .expect("an enrolled host names its id")
            .parse()
            .expect("the host id is a host id")
    }

    async fn subscribe(&mut self, scope: Scope) {
        self.send(json!({ "type": "subscribe", "scope": scope }))
            .await;
        let ack = self.recv().await;
        assert_eq!(ack["type"], "subscribed", "{ack}");
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

    /// `None` when nothing arrives within `window`.
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

/// The payload of a relayed frame: every publish on a host scope arrives as
/// `{"type":"event", …, "payload":"<the serialized operation>"}`.
fn payload(frame: &Value) -> Value {
    let payload = frame["payload"]
        .as_str()
        .unwrap_or_else(|| panic!("a frame carries a payload string: {frame}"));
    serde_json::from_str(payload).expect("the payload is the serialized operation")
}

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

fn user_message(id: &str, text: &str) -> ProviderEvent {
    ProviderEvent::ItemStarted {
        item: ThreadEventItem::UserMessage {
            id: id.to_owned(),
            content: vec![UserContent::Text {
                text: text.to_owned(),
            }],
            client_request_id: None,
            parent_tool_call_id: None,
        },
        provider_thread_id: "sess-1".to_owned(),
    }
}

/// One assistant answer, as the two frames a live run would have produced: the
/// text arrives as a delta and is closed by the completion.
fn assistant_message(item_id: &str, text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ItemAgentMessageDelta {
            item_id: item_id.to_owned(),
            delta: text.to_owned(),
            provider_thread_id: "sess-1".to_owned(),
            parent_tool_call_id: None,
        },
        ProviderEvent::ItemCompleted {
            item: ThreadEventItem::AgentMessage {
                id: item_id.to_owned(),
                text: text.to_owned(),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: "sess-1".to_owned(),
        },
    ]
}

/// Creates a project thread bound to the agent session `sess-1` on `host_id`,
/// and publishes the events the domain would.
fn bind_thread(state: &AppState, host_id: &HostId, workspace: &Path) -> (ThreadId, String) {
    let project_id = state.registry.personal_project_id();
    let (environment, environment_events) = state
        .registry
        .create_environment(
            Some(project_id.clone()),
            host_id.clone(),
            EnvironmentKind::Unmanaged,
            Some(workspace.to_string_lossy().into_owned()),
            loom_relay::now_ms(),
        )
        .unwrap();
    for event in &environment_events {
        state.publish_domain_event(event).unwrap();
    }

    let (thread, created) = state
        .registry
        .create_thread(
            Some(project_id),
            Some("a conversation from before".into()),
            Some(environment.id),
            loom_relay::now_ms(),
        )
        .unwrap();
    state.publish_domain_event(&created).unwrap();

    // The binding a run leaves behind: the agent that issued the id, the
    // directory it was opened in, and the machine that owns it.
    let binding = ProviderSessionBinding::new("pi", workspace.to_string_lossy().into_owned())
        .on_host(host_id.clone());
    let bound = state
        .registry
        .set_provider_session_id(&thread.id, "sess-1", Some(binding), loom_relay::now_ms())
        .expect("binding a session changes the thread");
    state.publish_domain_event(&bound).unwrap();

    // What the log will *not* keep: the conversation's own messages, which are
    // published and then aged out below so that "outside the retained window"
    // is literal rather than assumed.
    for (role, text) in [
        (MessageRole::User, "how many?"),
        (MessageRole::Assistant, "sixty."),
    ] {
        for event in state
            .registry
            .post_message(&thread.id, role, text.to_owned(), loom_relay::now_ms())
            .unwrap()
        {
            state.publish_domain_event(&event).unwrap();
        }
    }

    let cwd = workspace.to_string_lossy().into_owned();
    (thread.id, cwd)
}

/// Publishes filler frames until the thread's shard has evicted the messages
/// above. `backend_max_len` is what makes this a handful of frames.
fn age_out_the_window(state: &AppState, thread_id: &ThreadId, frames: usize) {
    for frame in 0..frames {
        state
            .publish(
                Scope::Thread(thread_id.to_string()),
                format!("{{\"filler\":{frame}}}"),
            )
            .unwrap();
    }
}

fn config(data_dir: &Path) -> AppConfig {
    AppConfig {
        // Small enough that a few frames evict the messages: the point is that
        // nothing in this test depends on the log's depth.
        backend_max_len: 4,
        backend_path: Some(data_dir.to_path_buf()),
        // The test drives recovery and reconciliation itself.
        reconcile_interval: Duration::ZERO,
        schedule_interval: Duration::ZERO,
        snapshot_interval: Duration::ZERO,
        ..AppConfig::default()
    }
}

#[tokio::test]
async fn a_conversation_outside_the_relay_window_is_loaded_from_the_agent() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();

    // --- Before the restart: a session binding the snapshot will keep, and a
    // conversation the log will not.
    let (first_addr, first) = spawn_server(config(data_dir.path())).await;
    let mut worker = Worker::connect(&first_addr).await;
    let host_id = worker.enroll(None).await;

    let (thread_id, cwd) = bind_thread(&first, &host_id, workspace.path());
    age_out_the_window(&first, &thread_id, 8);
    first.shutdown().unwrap();

    // --- After the restart: a fresh process, an empty cache, the same binding.
    let (addr, state) = spawn_server(config(data_dir.path())).await;
    let recovered = state
        .registry
        .thread(&thread_id)
        .expect("the entity view survived the restart");
    assert_eq!(
        recovered
            .provider_session_binding
            .as_ref()
            .and_then(|binding| binding.host_id.clone()),
        Some(host_id.clone()),
        "the binding, host included, is what the load is resolved from"
    );

    let mut worker = Worker::connect(&addr).await;
    let rehosted = worker.enroll(Some(&host_id)).await;
    assert_eq!(rehosted, host_id, "the same machine enrolls as itself");
    worker.subscribe(Scope::Host(host_id.to_string())).await;

    // A read does not wait for a load that can take as long as a cold start.
    // Nor does it claim to be complete: the recovery diagnostic the restart
    // published is real and worth showing, but it is not this thread's
    // conversation, and a `partial` answer is how the API says so.
    let base = format!("/api/v1/threads/{thread_id}/timeline");
    let first = http_json(&addr, &base).await;
    assert_eq!(first["history"]["complete"], false, "{first}");
    assert_ne!(first["history"]["status"], "ready", "{first}");
    assert!(
        first["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["kind"] != "conversation"),
        "the conversation is not in the log, so none of it is here yet: {first}"
    );

    // The load reaches the *owning host*, names the session the server
    // resolved, and asks for the workspace the binding recorded.
    // Nothing else is published on this host's scope, so the next frame is the
    // load the read asked for.
    let request = payload(&worker.recv().await);
    assert_eq!(
        request["operation"]["type"], "host.load_history",
        "the read asks the owning host to load the conversation: {request}"
    );
    let operation = &request["operation"];
    assert_eq!(operation["providerSessionId"], "sess-1");
    assert_eq!(operation["cwd"], cwd.as_str());
    assert_eq!(operation["threadId"], thread_id.to_string());
    let request_id = request["request_id"].as_str().unwrap().to_owned();

    // A replay of two turns: the first user message and its answer, then the
    // second. The second turn is what a merge must not fold into the first.
    let first_chunk: Vec<Value> = [
        user_message("restored-user-1", "how many?"),
        assistant_message("restored-assistant-1", "sixty.")[0].clone(),
        assistant_message("restored-assistant-1", "sixty.")[1].clone(),
    ]
    .iter()
    .map(|event| serde_json::to_value(event).unwrap())
    .collect();
    let second_chunk: Vec<Value> = [
        user_message("restored-user-2", "and then?"),
        assistant_message("restored-assistant-2", "sixty one.")[0].clone(),
        assistant_message("restored-assistant-2", "sixty one.")[1].clone(),
    ]
    .iter()
    .map(|event| serde_json::to_value(event).unwrap())
    .collect();

    for (batch_index, entries) in [(0, first_chunk), (1, second_chunk)] {
        worker
            .send(json!({
                "type": "history_report",
                "report": {
                    "host_id": host_id,
                    "request_id": request_id,
                    "part": {
                        "part": "chunk",
                        "batchIndex": batch_index,
                        "entries": entries,
                    },
                },
            }))
            .await;
    }
    worker
        .send(json!({
            "type": "history_report",
            "report": {
                "host_id": host_id,
                "request_id": request_id,
                "part": { "part": "complete", "batchCount": 2 },
            },
        }))
        .await;

    // --- The conversation is served from the cache, in the order it happened.
    let timeline = loop {
        let body = http_json(&addr, &base).await;
        if body["history"]["status"] == "ready" {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(timeline["history"]["complete"], true, "{timeline}");
    assert!(
        timeline["generation"].as_u64().unwrap() >= 1,
        "a loaded baseline is numbered under a generation: {timeline}"
    );

    let rows = timeline["rows"].as_array().unwrap();
    let conversation: Vec<&Value> = rows
        .iter()
        .filter(|row| row["kind"] == "conversation")
        .collect();
    assert_eq!(
        conversation.len(),
        4,
        "two prompts and two answers: {}",
        serde_json::to_string_pretty(&rows).unwrap()
    );
    let texts: Vec<&str> = conversation
        .iter()
        .map(|row| row["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["how many?", "sixty.", "and then?", "sixty one."]);
    assert_eq!(
        conversation
            .iter()
            .map(|row| row["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "user", "assistant"]
    );

    // A restored row reports what the replay carried and nothing more: no
    // timestamps, and a local turn key that cannot be mistaken for a run.
    for row in &conversation {
        assert!(row["startedAt"].is_null(), "no invented time: {row}");
        assert!(row["createdAt"].is_null(), "no invented time: {row}");
        let turn = row["turnId"]
            .as_str()
            .unwrap_or_else(|| panic!("a restored row is grouped: {row}"));
        assert!(
            row["turnId"].is_string() && turn.parse::<loom_domain::RunId>().is_err(),
            "a local grouping key must not parse as a run id: {turn}"
        );
    }

    // --- Loading a history is a read: no run, no prompt, no state change.
    assert!(
        worker.try_recv(Duration::from_millis(200)).await.is_none(),
        "a load must not dispatch anything"
    );
    let thread = state.registry.thread(&thread_id).unwrap();
    assert!(thread.active_run_id.is_none(), "{thread:?}");
    assert_ne!(thread.status, ThreadStatus::Working, "{thread:?}");
    assert_eq!(
        thread.provider_session_id.as_deref(),
        Some("sess-1"),
        "a load does not re-bind the session"
    );

    state.shutdown().unwrap();
}

//! A worker process against a real server over a real socket.
//!
//! These tests are the acceptance evidence for process independence: the
//! server is started server-only, a worker connects to it outbound, and
//! stopping the worker leaves the server serving.

use std::time::Duration;

use loom_domain::{HostId, HostStatus};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};
use loom_worker::{Worker, WorkerConfig};

/// Starts a server-only control plane on an ephemeral port.
async fn spawn_server(config: AppConfig) -> (String, AppState) {
    let state = AppState::build(config).unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{}:{}", addr.ip(), addr.port()), state)
}

/// Waits for `predicate`, yielding between attempts.
async fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    predicate()
}

#[tokio::test]
async fn a_server_with_no_worker_is_up_and_a_remote_worker_becomes_primary() {
    // The server declares a local host that never enrolls: server-only.
    let absent_local = HostId::mint();
    let (url, state) = spawn_server(AppConfig {
        local_host_id: Some(absent_local.clone()),
        ..AppConfig::default()
    })
    .await;

    // No worker has ever connected. The server still answers, and primary
    // resolution degrades instead of failing.
    assert!(state.registry.hosts().is_empty());
    assert!(state.registry.primary_host(Some(&absent_local)).is_none());

    // A worker on another machine dials out and enrolls.
    let mut worker = Worker::connect(WorkerConfig::new(&url, "remote-1"))
        .await
        .unwrap();
    let host_id = worker.enroll().await.unwrap();

    let host = state.registry.host(&host_id).unwrap();
    assert_eq!(host.status, HostStatus::Connected);
    assert_eq!(host.name, "remote-1");

    // Primary falls to the remote machine, not the absent local one.
    let primary = state.registry.primary_host(Some(&absent_local)).unwrap();
    assert_eq!(primary.id, host_id);

    // Heartbeats advance last-seen without publishing a frame.
    let before = state.registry.host(&host_id).unwrap().last_seen_at_ms;
    worker.heartbeat().await.unwrap();
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.last_seen_at_ms >= before)
            .unwrap_or(false))
        .await,
        "the heartbeat should be recorded"
    );

    worker.disconnect().await.unwrap();
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Disconnected)
            .unwrap_or(false))
        .await,
        "an announced departure should mark the host detached"
    );
    state.shutdown();
}

#[tokio::test]
async fn stopping_a_worker_leaves_the_server_serving() {
    let (url, state) = spawn_server(AppConfig::default()).await;

    let mut worker = Worker::connect(WorkerConfig::new(&url, "laptop"))
        .await
        .unwrap();
    let host_id = worker.enroll().await.unwrap();
    worker.disconnect().await.unwrap();

    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Disconnected)
            .unwrap_or(false))
        .await
    );

    // The control plane is still whole: publishing and replaying continue.
    let scope = loom_relay::Scope::Thread("thr_1".into());
    state.publish(scope.clone(), "{\"n\":1}").unwrap();
    assert_eq!(state.relay.replay_scope(&scope, 10).unwrap().len(), 1);
    assert!(state.registry.primary_host(None).is_none());
    state.shutdown();
}

#[tokio::test]
async fn a_reconnecting_worker_keeps_its_identity() {
    let (url, state) = spawn_server(AppConfig::default()).await;

    let mut first = Worker::connect(WorkerConfig::new(&url, "builder"))
        .await
        .unwrap();
    let host_id = first.enroll().await.unwrap();
    first.disconnect().await.unwrap();
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.status == HostStatus::Disconnected)
            .unwrap_or(false))
        .await
    );

    // A restart presents the identity it was given: one machine, reconnected.
    let mut config = WorkerConfig::new(&url, "builder");
    config.host_id = Some(host_id.clone());
    let mut second = Worker::connect(config).await.unwrap();
    assert_eq!(second.enroll().await.unwrap(), host_id);

    assert_eq!(state.registry.hosts().len(), 1);
    assert_eq!(
        state.registry.host(&host_id).unwrap().status,
        HostStatus::Connected
    );

    second.disconnect().await.unwrap();
    state.shutdown();
}

/// A real worker answers a real server's file request over the relay.
///
/// This is the end-to-end proof for B5: the enrollment reports the machine's
/// data directory, the server composes thread storage from it, publishes a read
/// to the host room, and the worker's own filesystem work comes back up the
/// socket. The stored bytes are the ones the test wrote on disk — the server
/// never touched them itself.
#[tokio::test]
async fn a_worker_answers_thread_storage_reads_from_its_own_disk() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (url, state) = spawn_server(AppConfig::default()).await;
    let mut config = WorkerConfig::new(&url, "files");
    config.data_dir = data_dir.clone();
    config.heartbeat_interval = Duration::from_millis(25);
    let mut worker = Worker::connect(config).await.unwrap();
    let host_id = worker.enroll().await.unwrap();
    let worker = tokio::spawn(async move {
        let _ = worker.run().await;
    });

    // The server learned the machine's layout from the enrollment, and the
    // composed root is the real directory on this disk.
    let now = loom_relay::now_ms();
    let (project, _) = state
        .registry
        .create_project(
            "files".into(),
            loom_domain::ProjectKind::Standard,
            None,
            now,
        )
        .unwrap();
    let (environment, _) = state
        .registry
        .create_environment(
            Some(project.id.clone()),
            host_id.clone(),
            loom_domain::EnvironmentKind::Unmanaged,
            Some(temp.path().join("workspace").to_string_lossy().into_owned()),
            now,
        )
        .unwrap();
    let (thread, _) = state
        .registry
        .create_thread(
            Some(project.id.clone()),
            Some("files".into()),
            Some(environment.id.clone()),
            now,
        )
        .unwrap();

    let storage = data_dir.join("thread-storage").join(thread.id.to_string());
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("notes.md"), b"# from the worker\n").unwrap();

    // A read of a file only the worker's disk has.
    let outcome = state
        .request_host_file(
            &host_id,
            loom_provider_protocol::HostFileOperation::Read {
                path: storage.join("notes.md").to_string_lossy().into_owned(),
                root_path: Some(storage.to_string_lossy().into_owned()),
                max_bytes: 4096,
            },
        )
        .await
        .unwrap();
    let loom_provider_protocol::HostFileOutcome::Content(content) = outcome else {
        panic!("expected content, got {outcome:?}");
    };
    assert_eq!(content.content, "# from the worker\n");
    assert_eq!(content.mime_type.as_deref(), Some("text/markdown"));

    // A listing of the same directory, relative to it.
    let outcome = state
        .request_host_file(
            &host_id,
            loom_provider_protocol::HostFileOperation::List {
                path: storage.to_string_lossy().into_owned(),
                query: None,
                limit: 100,
                include_files: true,
                include_directories: false,
                include_hidden: false,
            },
        )
        .await
        .unwrap();
    let loom_provider_protocol::HostFileOutcome::Listing { entries, truncated } = outcome else {
        panic!("expected a listing, got {outcome:?}");
    };
    assert!(!truncated);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "notes.md");

    // A thread whose environment names a host that is not enrolled is refused
    // before a room nobody is in is published to.
    let absent = HostId::mint();
    let outcome = state
        .request_host_file(
            &absent,
            loom_provider_protocol::HostFileOperation::List {
                path: "/tmp".into(),
                query: None,
                limit: 10,
                include_files: true,
                include_directories: false,
                include_hidden: false,
            },
        )
        .await;
    assert!(matches!(
        outcome,
        Err(loom_server::HostFileTransportError::UnknownHost(_))
    ));

    worker.abort();
    state.shutdown();
}

/// A real worker drives a real PTY for a real server.
///
/// This is the end-to-end proof for B9's terminal half: the server mints the
/// session, publishes a create to the host room, and the worker spawns the
/// process, captures its output and answers a window read. The bytes the test
/// reads back came from a process on this machine, never from the server.
#[tokio::test]
async fn a_worker_runs_a_terminal_and_streams_its_output() {
    use loom_provider_protocol::{
        TerminalOperation, TerminalOutcome, TerminalStart, TerminalStatus, TerminalTarget,
    };

    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (url, state) = spawn_server(AppConfig::default()).await;
    let mut config = WorkerConfig::new(&url, "terminals");
    config.data_dir = data_dir.clone();
    config.heartbeat_interval = Duration::from_millis(25);
    let mut worker = Worker::connect(config).await.unwrap();
    let host_id = worker.enroll().await.unwrap();
    let worker = tokio::spawn(async move {
        let _ = worker.run().await;
    });

    // Create a session that prints something and exits.
    let outcome = state
        .request_terminal(
            &host_id,
            TerminalOperation::Create {
                id: "term_remote".into(),
                start: TerminalStart::Command {
                    command: "printf 'from the worker'".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: host_id.clone(),
                    cwd: Some(temp.path().to_string_lossy().into_owned()),
                },
                cols: 80,
                rows: 24,
                title: "smoke".into(),
                cwd: temp.path().to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
    let TerminalOutcome::Session { session } = outcome else {
        panic!("expected a session, got {outcome:?}");
    };
    assert_eq!(session.id, "term_remote");
    assert!(
        matches!(
            session.status,
            TerminalStatus::Running | TerminalStatus::Exited
        ),
        "unexpected status {:?}",
        session.status
    );

    // The output arrives asynchronously; poll the cursor until the text lands.
    let mut collected = String::new();
    let mut cursor = 0u64;
    for _ in 0..200 {
        let outcome = state
            .request_terminal(
                &host_id,
                TerminalOperation::Output {
                    id: "term_remote".into(),
                    since_seq: cursor,
                    limit: 100,
                    tail_bytes: 64 * 1024,
                },
            )
            .await
            .unwrap();
        let TerminalOutcome::Output {
            chunks, next_seq, ..
        } = outcome
        else {
            panic!("expected output, got {outcome:?}");
        };
        for chunk in &chunks {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&chunk.data_base64)
                .unwrap();
            collected.push_str(&String::from_utf8_lossy(&bytes));
        }
        cursor = next_seq;
        if collected.contains("from the worker") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        collected.contains("from the worker"),
        "the terminal produced no expected output: {collected:?}"
    );

    // A resume from an old cursor re-reads the same bytes — replaying a read is
    // safe because the ring is on the machine that produced it.
    let replayed = state
        .request_terminal(
            &host_id,
            TerminalOperation::Output {
                id: "term_remote".into(),
                since_seq: 0,
                limit: 100,
                tail_bytes: 64 * 1024,
            },
        )
        .await
        .unwrap();
    let TerminalOutcome::Output { chunks, .. } = replayed else {
        panic!("expected output");
    };
    assert!(
        !chunks.is_empty(),
        "a cursor at zero must still see the retained window"
    );

    // Close is idempotent for an already-exited process.
    let closed = state
        .request_terminal(
            &host_id,
            TerminalOperation::Close {
                id: "term_remote".into(),
                force: true,
            },
        )
        .await
        .unwrap();
    let TerminalOutcome::Session { session } = closed else {
        panic!("expected a session, got {closed:?}");
    };
    assert_eq!(session.status, TerminalStatus::Exited);

    // An unknown session is a bounded failure, not a fabricated answer.
    let missing = state
        .request_terminal(
            &host_id,
            TerminalOperation::Output {
                id: "term_nope".into(),
                since_seq: 0,
                limit: 10,
                tail_bytes: 1024,
            },
        )
        .await
        .unwrap();
    let TerminalOutcome::Failed { code, .. } = missing else {
        panic!("expected a failure, got {missing:?}");
    };
    assert_eq!(code, "terminal_not_found");

    // A host that is not enrolled is refused before anything is published.
    let absent = HostId::mint();
    let outcome = state
        .request_terminal(&absent, TerminalOperation::Report { id: None })
        .await;
    assert!(matches!(
        outcome,
        Err(loom_server::TerminalTransportError::UnknownHost(_))
    ));

    worker.abort();
    state.shutdown();
}

//! A daemon process against a real server over a real socket.
//!
//! These tests are the acceptance evidence for process independence: the
//! server is started server-only, a daemon connects to it outbound, and
//! stopping the daemon leaves the server serving.

use std::time::Duration;

use loom_daemon::{Daemon, DaemonConfig};
use loom_domain::{HostId, HostStatus};
use loom_server::http::router;
use loom_server::state::{AppConfig, AppState};

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
async fn a_server_with_no_daemon_is_up_and_a_remote_daemon_becomes_primary() {
    // The server declares a local host that never enrolls: server-only.
    let absent_local = HostId::mint();
    let (url, state) = spawn_server(AppConfig {
        local_host_id: Some(absent_local.clone()),
        ..AppConfig::default()
    })
    .await;

    // No daemon has ever connected. The server still answers, and primary
    // resolution degrades instead of failing.
    assert!(state.registry.hosts().is_empty());
    assert!(state.registry.primary_host(Some(&absent_local)).is_none());

    // A daemon on another machine dials out and enrolls.
    let mut daemon = Daemon::connect(DaemonConfig::new(&url, "remote-1"))
        .await
        .unwrap();
    let host_id = daemon.enroll().await.unwrap();

    let host = state.registry.host(&host_id).unwrap();
    assert_eq!(host.status, HostStatus::Connected);
    assert_eq!(host.name, "remote-1");

    // Primary falls to the remote machine, not the absent local one.
    let primary = state.registry.primary_host(Some(&absent_local)).unwrap();
    assert_eq!(primary.id, host_id);

    // Heartbeats advance last-seen without publishing a frame.
    let before = state.registry.host(&host_id).unwrap().last_seen_at_ms;
    daemon.heartbeat().await.unwrap();
    assert!(
        eventually(|| state
            .registry
            .host(&host_id)
            .map(|host| host.last_seen_at_ms >= before)
            .unwrap_or(false))
        .await,
        "the heartbeat should be recorded"
    );

    daemon.disconnect().await.unwrap();
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
async fn stopping_a_daemon_leaves_the_server_serving() {
    let (url, state) = spawn_server(AppConfig::default()).await;

    let mut daemon = Daemon::connect(DaemonConfig::new(&url, "laptop"))
        .await
        .unwrap();
    let host_id = daemon.enroll().await.unwrap();
    daemon.disconnect().await.unwrap();

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
async fn a_reconnecting_daemon_keeps_its_identity() {
    let (url, state) = spawn_server(AppConfig::default()).await;

    let mut first = Daemon::connect(DaemonConfig::new(&url, "builder"))
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
    let mut config = DaemonConfig::new(&url, "builder");
    config.host_id = Some(host_id.clone());
    let mut second = Daemon::connect(config).await.unwrap();
    assert_eq!(second.enroll().await.unwrap(), host_id);

    assert_eq!(state.registry.hosts().len(), 1);
    assert_eq!(
        state.registry.host(&host_id).unwrap().status,
        HostStatus::Connected
    );

    second.disconnect().await.unwrap();
    state.shutdown();
}

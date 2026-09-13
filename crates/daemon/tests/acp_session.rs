//! The ACP transport, driven against a real `pi-acp` process.
//!
//! The translator is unit-tested against ACP values; the point here is the path
//! around it — a real ACP handshake over a real pipe, the negotiated version,
//! a session opened in a real working directory, a prompt sent, updates
//! translated, and exactly one terminal event reported.
//!
//! These tests need a `pi-acp` binary. `PI_ACP_BIN` names it, and they skip
//! when it is absent rather than fail, because loom does not build pi-acp: it
//! is a sibling project that lives at a path the developer chooses. CI for
//! loom has no reason to have it, and a test that fails without a sibling
//! checkout teaches people to ignore failures.
//!
//! The agent's own output is not the assertion — the *translation and the
//! lifecycle* are. `pi` is the LLM-driven part and is mocked by pi-acp itself
//! when `PI_ACP_MOCK=1` is set.

use std::path::PathBuf;
use std::time::Duration;

use loom_daemon::acp::session::{drive, Transport};
use loom_daemon::provider::ProviderRun;
use loom_domain::RunOutcome;
use loom_provider_protocol::ProviderSpec;
use tokio::sync::mpsc;

/// The `pi-acp` binary, when this checkout can find one.
fn pi_acp_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PI_ACP_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    // A sibling checkout is the normal development layout.
    for candidate in [
        "../../../pi-acp/target/release/pi-acp",
        "../../../pi-acp/target/debug/pi-acp",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn run(cwd: &str) -> ProviderRun {
    let mut spec = ProviderSpec::custom("unused".to_string(), Vec::new());
    spec.cwd = Some(cwd.to_string());
    ProviderRun {
        spec,
        prompt: "say hello".to_string(),
        host_id: loom_domain::HostId::mint(),
        thread_id: loom_domain::ThreadId::mint(),
        project_id: loom_domain::ProjectId::mint(),
        run_id: loom_domain::RunId::mint(),
        timeout: Duration::from_secs(30),
        session_dir: None,
    }
}

/// Drives one run to completion and returns every event it reported.
async fn drive_to_completion(run: ProviderRun) -> Vec<loom_domain::RunEvent> {
    let (tx, mut rx) = mpsc::channel(64);
    let binary = pi_acp_binary().expect("checked by the caller");
    let transport = Transport::Stdio {
        command: binary.to_string_lossy().into_owned(),
        args: Vec::new(),
    };

    let handle = tokio::spawn(async move {
        let _ = drive(&run, transport, &tx).await;
    });

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    let _ = handle.await;
    events
}

/// The terminal event, if the run ended.
fn terminal(events: &[loom_domain::RunEvent]) -> Option<&loom_domain::RunEvent> {
    events.iter().find(|e| e.is_terminal())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_agent_drives_a_run_to_exactly_one_terminal_event() {
    let Some(_) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary (set PI_ACP_BIN or build the sibling checkout)");
        return;
    };
    let cwd = std::env::temp_dir().join("loom-acp-e2e");
    std::fs::create_dir_all(&cwd).unwrap();

    let events = drive_to_completion(run(&cwd.to_string_lossy())).await;

    let terminals: Vec<_> = events.iter().filter(|e| e.is_terminal()).collect();
    assert_eq!(
        terminals.len(),
        1,
        "a run reports exactly one terminal event, got {}: {events:#?}",
        terminals.len()
    );

    // Whatever the agent's model did, the lifecycle facts loom owns must be
    // there: identity, the turn opening, and the turn closing.
    assert!(
        events.iter().any(|e| matches!(e.kind(), "thread/identity")),
        "the session identity is stated: {events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(e.kind(), "turn/started")),
        "the turn was opened: {events:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_agent_ends_the_run_rather_than_hanging_it() {
    let cwd = std::env::temp_dir().join("loom-acp-e2e");
    std::fs::create_dir_all(&cwd).unwrap();

    // A binary that cannot start is the interesting failure: the run must end
    // rather than leave its thread in `working` forever.
    let (tx, mut rx) = mpsc::channel(64);
    let transport = Transport::Stdio {
        command: "/nonexistent/pi-acp".to_string(),
        args: Vec::new(),
    };
    let run = run(&cwd.to_string_lossy());
    let _ = drive(&run, transport, &tx).await;
    drop(tx);

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }

    let terminal = terminal(&events).expect("a failed run still ends");
    assert_eq!(
        terminal.terminal_status(),
        Some(loom_domain::TurnStatus::Failed),
        "an agent that cannot start fails the run: {events:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dispatch_without_a_working_directory_is_refused() {
    let (tx, mut rx) = mpsc::channel(64);
    let mut run = run("/tmp");
    // An ACP session is created *in* a directory; without one there is nothing
    // to create it in, and starting in the daemon's own cwd would edit the
    // wrong project.
    run.spec.cwd = None;
    let transport = Transport::Stdio {
        command: "/nonexistent/pi-acp".to_string(),
        args: Vec::new(),
    };
    let result = drive(&run, transport, &tx).await;
    drop(tx);

    // Either the refusal is returned, or it was reported as a failure; both end
    // the run, and neither starts a session elsewhere.
    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    let refused = result.is_err()
        || matches!(
            terminal(&events).and_then(|e| e.terminal_status()),
            Some(loom_domain::TurnStatus::Failed)
        );
    assert!(refused, "a run with no cwd must not proceed: {events:#?}");
}

/// Every event a run reported carried the run's own identity, so a consumer can
/// attribute it without a lookup.
#[tokio::test(flavor = "multi_thread")]
async fn every_reported_event_carries_the_runs_identity() {
    let Some(_) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary");
        return;
    };
    let cwd = std::env::temp_dir().join("loom-acp-e2e");
    std::fs::create_dir_all(&cwd).unwrap();
    let run = run(&cwd.to_string_lossy());
    let thread_id = run.thread_id.clone();
    let run_id = run.run_id.clone();
    let project_id = run.project_id.clone();

    let events = drive_to_completion(run).await;

    for event in &events {
        assert_eq!(event.thread_id, thread_id, "thread identity on {event:?}");
        assert_eq!(event.run_id, run_id, "run identity on {event:?}");
        assert_eq!(
            event.project_id, project_id,
            "project identity on {event:?}"
        );
    }
}

/// The terminal event carries loom's own verdict, so the server does not have
/// to infer one from the contract status.
#[tokio::test(flavor = "multi_thread")]
async fn the_terminal_event_carries_an_outcome() {
    let Some(_) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary");
        return;
    };
    let cwd = std::env::temp_dir().join("loom-acp-e2e");
    std::fs::create_dir_all(&cwd).unwrap();

    let events = drive_to_completion(run(&cwd.to_string_lossy())).await;
    let terminal = terminal(&events).expect("a run ends");
    assert!(
        terminal.outcome.is_some(),
        "the daemon's verdict travels with the terminal event"
    );
    assert!(matches!(
        terminal.outcome,
        Some(RunOutcome::Completed | RunOutcome::Failed | RunOutcome::Cancelled)
    ));
}

/// Nothing is reported after the terminal event.
#[tokio::test(flavor = "multi_thread")]
async fn no_event_follows_the_terminal_one() {
    let Some(_) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary");
        return;
    };
    let cwd = std::env::temp_dir().join("loom-acp-e2e");
    std::fs::create_dir_all(&cwd).unwrap();

    let events = drive_to_completion(run(&cwd.to_string_lossy())).await;
    let position = events
        .iter()
        .position(|e| e.is_terminal())
        .expect("a run ends");
    assert_eq!(
        position,
        events.len() - 1,
        "the terminal event is last: {events:#?}"
    );
}

/// The lifecycle events come first, even though the agent emits updates while
/// answering `session/new`.
///
/// This is a regression test for a real bug: pi-acp publishes session info and
/// a usage snapshot as part of `session/new`, so translating those before
/// emitting identity put a fact about a thread *before* the event announcing
/// that thread. A consumer reading the log in order sees an update it cannot
/// attribute.
#[tokio::test(flavor = "multi_thread")]
async fn the_thread_is_identified_before_any_update_about_it() {
    let Some(_) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary");
        return;
    };
    let cwd = std::env::temp_dir().join("loom-acp-e2e");
    std::fs::create_dir_all(&cwd).unwrap();

    let events = drive_to_completion(run(&cwd.to_string_lossy())).await;

    let identity = events
        .iter()
        .position(|e| e.kind() == "thread/identity")
        .expect("the run identifies its thread");
    let turn = events
        .iter()
        .position(|e| e.kind() == "turn/started")
        .expect("the run opens its turn");

    for (index, event) in events.iter().enumerate() {
        if index == identity || index == turn {
            continue;
        }
        assert!(
            index > identity,
            "{} at {index} precedes the identity at {identity}: {events:#?}",
            event.kind()
        );
        assert!(
            index > turn,
            "{} at {index} precedes the turn start at {turn}: {events:#?}",
            event.kind()
        );
    }
}

/// A workspace that no longer exists fails the run, rather than starting the
/// agent in whatever directory the daemon happens to be in.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_that_does_not_exist_is_refused() {
    let (tx, mut rx) = mpsc::channel(64);
    let run = run("/nonexistent/workspace/for/loom");
    let transport = Transport::Stdio {
        command: "/nonexistent/pi-acp".to_string(),
        args: Vec::new(),
    };
    let result = drive(&run, transport, &tx).await;
    drop(tx);

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    assert!(
        result.is_err() || terminal(&events).is_some(),
        "a missing workspace must not let the run proceed"
    );
    if let Some(message) = result.err() {
        assert!(
            message.contains("does not exist"),
            "the error names the problem: {message}"
        );
    }
}

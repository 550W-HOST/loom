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

use loom_domain::RunOutcome;
use loom_provider_protocol::ProviderSpec;
use loom_worker::acp::permission::PermissionRegistry;
use loom_worker::acp::session::{drive, Transport};
use loom_worker::provider::ProviderRun;
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
    let mut spec = ProviderSpec::acp("unused".to_string(), Vec::new());
    spec.cwd = Some(cwd.to_string());
    ProviderRun {
        spec,
        prompt: "say hello".to_string(),
        host_id: loom_domain::HostId::mint(),
        thread_id: loom_domain::ThreadId::mint(),
        project_id: loom_domain::ProjectId::mint(),
        run_id: loom_domain::RunId::mint(),
        timeout: Duration::from_secs(30),
        permission_timeout: Duration::from_secs(5),
        permission_ceiling: loom_domain::HostPermissionMode::Full,
        provider_session_id: None,
        model: None,
        reasoning_level: None,
    }
}

/// Drives one run to completion with an explicitly named ACP agent.
async fn drive_with_agent(run: ProviderRun, agent: PathBuf) -> Vec<loom_domain::RunEvent> {
    let (tx, mut rx) = mpsc::channel(64);
    let transport = Transport::Stdio {
        command: agent.to_string_lossy().into_owned(),
        args: Vec::new(),
    };

    let (interactions, _requests) = mpsc::channel(8);
    let handle = tokio::spawn(async move {
        let _ = drive(
            &run,
            transport,
            &tx,
            PermissionRegistry::new(),
            interactions,
        )
        .await;
    });

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    let _ = handle.await;
    events
}

/// Drives one run against the real pi-acp binary when the sibling checkout is
/// available.
async fn drive_to_completion(run: ProviderRun) -> Vec<loom_domain::RunEvent> {
    let binary = pi_acp_binary().expect("checked by the caller");
    drive_with_agent(run, binary).await
}

/// A minimal ACP agent that records whether the client restored its session.
fn write_resuming_agent(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("resuming-agent.sh");
    let script = r#"#!/bin/sh
session_id=resumable-session
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n' "$id"
      ;;
    session/new)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$session_id"
      ;;
    session/load)
      touch "$0.loaded"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"history"}}}}\n' "$session_id"
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    session/prompt)
      if [ -f "$0.loaded" ]; then
        text=resumed
      else
        text=fresh
      fi
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"%s"}}}}\n' "$session_id" "$text"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
    session/cancel)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
  esac
done
"#;
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A minimal ACP v2 agent that streams a patch and reports completion through
/// the idle state. Its resume notification deliberately looks like history,
/// so the second run proves that loom does not replay it.
fn write_v2_resuming_agent(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("v2-resuming-agent.sh");
    let script = r#"#!/bin/sh
session_id=v2-resumable-session
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":2,"info":{"name":"fake-v2-agent","version":"1"},"capabilities":{"session":{}}}}'
      ;;
    session/new)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"'"$session_id"'"}}'
      ;;
    session/resume)
      touch "$0.resumed"
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"agent_message_chunk","messageId":"history-message","content":{"type":"text","text":"history"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
    session/prompt)
      if [ -f "$0.resumed" ]; then
        text=resumed
      else
        text=fresh
      fi
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"agent_message_chunk","messageId":"answer-message","content":{"type":"text","text":"'"$text"'"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"agent_message","messageId":"answer-message","content":[{"type":"text","text":"'"$text"' answer"}]}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}}}'
      ;;
  esac
done
"#;
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn run_with_agent(
    cwd: &str,
    agent: &std::path::Path,
    thread_id: loom_domain::ThreadId,
    provider_session_id: Option<&str>,
) -> ProviderRun {
    let mut spec = ProviderSpec::acp(agent.to_string_lossy().into_owned(), Vec::new());
    spec.cwd = Some(cwd.to_owned());
    ProviderRun {
        spec,
        prompt: "say hello".to_string(),
        host_id: loom_domain::HostId::mint(),
        thread_id,
        project_id: loom_domain::ProjectId::mint(),
        run_id: loom_domain::RunId::mint(),
        timeout: Duration::from_secs(30),
        permission_timeout: Duration::from_secs(5),
        permission_ceiling: loom_domain::HostPermissionMode::Full,
        provider_session_id: provider_session_id.map(str::to_owned),
        model: None,
        reasoning_level: None,
    }
}

/// The terminal event, if the run ended.
fn terminal(events: &[loom_domain::RunEvent]) -> Option<&loom_domain::RunEvent> {
    events.iter().find(|e| e.is_terminal())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_loads_the_same_acp_session() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = write_resuming_agent(tmp.path());
    let thread_id = loom_domain::ThreadId::mint();

    let first = drive_with_agent(
        run_with_agent(
            &workspace.to_string_lossy(),
            &agent,
            thread_id.clone(),
            None,
        ),
        agent.clone(),
    )
    .await;
    let session_id = first
        .iter()
        .find(|event| event.kind() == "thread/identity")
        .and_then(loom_domain::RunEvent::provider_thread_id)
        .expect("the first run reports the ACP session id")
        .to_owned();
    assert_eq!(session_id, "resumable-session");
    assert_eq!(
        first
            .iter()
            .filter_map(|event| match &event.event.body {
                loom_domain::ProviderEvent::ItemAgentMessageDelta { delta, .. } => {
                    Some(delta.as_str())
                }
                _ => None,
            })
            .collect::<String>(),
        "fresh"
    );

    let second = drive_with_agent(
        run_with_agent(
            &workspace.to_string_lossy(),
            &agent,
            thread_id,
            Some(&session_id),
        ),
        agent,
    )
    .await;
    assert_eq!(
        second
            .iter()
            .find(|event| event.kind() == "thread/identity")
            .and_then(loom_domain::RunEvent::provider_thread_id),
        Some(session_id.as_str())
    );
    assert_eq!(
        second
            .iter()
            .filter_map(|event| match &event.event.body {
                loom_domain::ProviderEvent::ItemAgentMessageDelta { delta, .. } => {
                    Some(delta.as_str())
                }
                _ => None,
            })
            .collect::<String>(),
        "resumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v2_agent_resumes_without_replay_and_completes_once() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = write_v2_resuming_agent(tmp.path());
    let thread_id = loom_domain::ThreadId::mint();

    let first = drive_with_agent(
        run_with_agent(
            &workspace.to_string_lossy(),
            &agent,
            thread_id.clone(),
            None,
        ),
        agent.clone(),
    )
    .await;
    let session_id = first
        .iter()
        .find(|event| event.kind() == "thread/identity")
        .and_then(loom_domain::RunEvent::provider_thread_id)
        .expect("the v2 run reports its session id")
        .to_owned();
    assert_eq!(session_id, "v2-resumable-session");
    assert_eq!(
        first
            .iter()
            .filter_map(|event| match &event.event.body {
                loom_domain::ProviderEvent::ItemAgentMessageDelta { delta, .. } => {
                    Some(delta.as_str())
                }
                _ => None,
            })
            .collect::<String>(),
        "fresh answer"
    );
    assert_eq!(
        first.iter().filter(|event| event.is_terminal()).count(),
        1,
        "the first v2 prompt has one terminal event: {first:#?}"
    );

    let second = drive_with_agent(
        run_with_agent(
            &workspace.to_string_lossy(),
            &agent,
            thread_id,
            Some(&session_id),
        ),
        agent,
    )
    .await;
    assert_eq!(
        second
            .iter()
            .filter_map(|event| match &event.event.body {
                loom_domain::ProviderEvent::ItemAgentMessageDelta { delta, .. } => {
                    Some(delta.as_str())
                }
                _ => None,
            })
            .collect::<String>(),
        "resumed answer",
        "resume history must not be written as a new timeline delta: {second:#?}"
    );
    assert_eq!(
        second.iter().filter(|event| event.is_terminal()).count(),
        1,
        "duplicate idle notifications cannot duplicate completion: {second:#?}"
    );
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
    let (interactions, _requests) = mpsc::channel(8);
    let _ = drive(
        &run,
        transport,
        &tx,
        PermissionRegistry::new(),
        interactions,
    )
    .await;
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
    // to create it in, and starting in the worker's own cwd would edit the
    // wrong project.
    run.spec.cwd = None;
    let transport = Transport::Stdio {
        command: "/nonexistent/pi-acp".to_string(),
        args: Vec::new(),
    };
    let (interactions, _requests) = mpsc::channel(8);
    let result = drive(
        &run,
        transport,
        &tx,
        PermissionRegistry::new(),
        interactions,
    )
    .await;
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
        "the worker's verdict travels with the terminal event"
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
/// agent in whatever directory the worker happens to be in.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_that_does_not_exist_is_refused() {
    let (tx, mut rx) = mpsc::channel(64);
    let run = run("/nonexistent/workspace/for/loom");
    let transport = Transport::Stdio {
        command: "/nonexistent/pi-acp".to_string(),
        args: Vec::new(),
    };
    let (interactions, _requests) = mpsc::channel(8);
    let result = drive(
        &run,
        transport,
        &tx,
        PermissionRegistry::new(),
        interactions,
    )
    .await;
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

/// A resume whose workspace is gone fails with the path named, rather than
/// silently starting a fresh session.
///
/// This is the failure bb's own bridge found worth guarding: a resumed session
/// belongs to the directory it was created in, so if that directory is gone the
/// conversation cannot continue — and starting a new one in the worker's own cwd
/// would edit the wrong project while looking like success.
#[tokio::test(flavor = "multi_thread")]
async fn a_resume_in_a_missing_workspace_fails_and_names_the_path() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = write_resuming_agent(tmp.path());
    let missing = tmp.path().join("workspace-gone");
    let missing = missing.to_string_lossy().into_owned();

    let (tx, mut rx) = mpsc::channel(64);
    let run = run_with_agent(
        &missing,
        &agent,
        loom_domain::ThreadId::mint(),
        Some("resumable-session"),
    );
    let transport = Transport::Stdio {
        command: agent.to_string_lossy().into_owned(),
        args: Vec::new(),
    };
    let (interactions, _requests) = mpsc::channel(8);
    let result = drive(
        &run,
        transport,
        &tx,
        PermissionRegistry::new(),
        interactions,
    )
    .await;
    drop(tx);

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    // The refusal is returned before any session exists, and nothing was
    // reported as a resumed identity: no session was opened at all. `drive`'s
    // contract is that a pre-terminal failure is returned and `spawn` — the
    // worker's path — turns it into the one terminal event, so the assertion
    // here is on the reason rather than on a terminal event.
    let message = result.expect_err("a missing workspace must be refused");
    assert!(
        message.contains(&missing),
        "the reason names the missing directory: {message}"
    );
    assert!(
        !events.iter().any(|event| event.kind() == "thread/identity"),
        "a refused resume must not report an identity: {events:#?}"
    );
    // The agent was never asked to load: the refusal happens before a session.
    assert!(
        !tmp.path().join("resuming-agent.sh.loaded").exists(),
        "a missing workspace must be refused before `session/load` is sent"
    );
}

/// An agent that cannot load a session fails an explicit resume rather than
/// quietly starting a fresh conversation.
///
/// Silently starting over is the other half of the cwd guard: a user who asked
/// to continue a conversation must be told it cannot be continued, not handed a
/// new one that looks like the old one's answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_resume_against_an_agent_without_load_session_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // The agent advertises no `loadSession`, so the id it is handed cannot be
    // honoured.
    let agent = write_agent_without_load(tmp.path());

    let (tx, mut rx) = mpsc::channel(64);
    let transport = Transport::Stdio {
        command: agent.to_string_lossy().into_owned(),
        args: Vec::new(),
    };
    let run = run_with_agent(
        &workspace.to_string_lossy(),
        &agent,
        loom_domain::ThreadId::mint(),
        Some("acp-session-from-another-agent"),
    );
    let (interactions, _requests) = mpsc::channel(8);
    let _ = drive(
        &run,
        transport,
        &tx,
        PermissionRegistry::new(),
        interactions,
    )
    .await;
    drop(tx);

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    let terminal = terminal(&events).expect("the run still ends");
    assert_eq!(
        terminal.terminal_status(),
        Some(loom_domain::TurnStatus::Failed)
    );
    assert!(
        terminal
            .terminal_error()
            .is_some_and(|message| message.contains("session/load")),
        "the reason names the missing capability: {:?}",
        terminal.terminal_error()
    );
    // Nothing was created in its place: a fresh session would have reported the
    // new identity.
    assert!(
        !events.iter().any(|event| event.kind() == "thread/identity"),
        "a refused resume must not open a replacement session: {events:#?}"
    );
}

/// An ACP agent that answers `initialize` without advertising `session/load`.
///
/// Its `session/new` would work; only resuming would not, which is what makes it
/// the fixture for the "cannot honour this id" case.
fn write_agent_without_load(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("no-load.sh");
    // A single-quoted heredoc keeps the shell template literal, so the JSON
    // printf formats below stay exactly what the agent must emit.
    let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":false}}}'
      ;;
    session/new)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"fresh"}}'
      ;;
    session/prompt)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}'
      ;;
  esac
done
"#;
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// `session/list` against a real `pi-acp`, capability-gated.
///
/// The unit tests in `crate::acp::sessions` use stubs; this confirms the same
/// two outcomes against the real adapter — which advertises the capability, so
/// the important half is that the probe follows it rather than failing closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_agent_is_probed_for_its_session_capabilities() {
    let Some(agent) = pi_acp_binary() else {
        eprintln!("skipping: no pi-acp binary (set PI_ACP_BIN or build the sibling checkout)");
        return;
    };
    use loom_worker::acp::sessions::{list_sessions, SessionListOutcome};

    let outcome = list_sessions(
        Transport::Stdio {
            command: agent.to_string_lossy().into_owned(),
            args: Vec::new(),
        },
        None,
        Duration::from_secs(30),
    )
    .await;

    match outcome {
        // `pi-acp` advertises `session/list`, so a capable agent must land here
        // rather than on `Unsupported`. What it lists depends on the machine, so
        // the session count is not asserted.
        SessionListOutcome::Listed { capabilities, .. } => {
            assert!(
                capabilities.list_sessions,
                "reaching `Listed` means the capability was advertised"
            );
            assert!(
                capabilities.load_session,
                "pi-acp advertises session/load too, which is what resume depends on"
            );
        }
        // The opposite is also a legitimate result on a machine where the pinned
        // `pi-acp` does not advertise listing, and it must be `Unsupported`
        // rather than an empty `Listed`: "cannot list" and "nothing to list" are
        // different facts.
        SessionListOutcome::Unsupported { capabilities } => {
            assert!(!capabilities.list_sessions);
        }
        SessionListOutcome::Failed { error } => {
            panic!("the real adapter must be reachable: {error}");
        }
    }
}

//! Real Pi, through the embedded adapter: a first turn, and a cross-run resume.
//!
//! Every other ACP test in this crate uses a stub, which is what makes them
//! deterministic. This one is the opposite: it is the end-to-end evidence that
//! the real `pi` binary, driven through `pi-acp` linked into the worker, streams
//! a real turn and that a second run continues the same conversation. It needs a
//! model (or rather, credentials for whatever provider `pi` is configured with),
//! so it is `#[ignore]`d by default and run explicitly:
//!
//! ```text
//! cargo test -p loom-worker --test real_pi -- --ignored --nocapture
//! ```
//!
//! `PI_BIN` overrides the executable; the default is `pi` on `PATH`, which is
//! what `pi-acp`'s own default resolves to.
//!
//! The assertions deliberately stop at what is *observable*: exactly one
//! terminal event, a turn that completed, an identity that came from the agent,
//! and a second turn that ran against the same session id and produced an answer.
//! Asserting the model's exact words would make this a test of the model; the
//! property under test is the transport and the session mapping.

use std::path::PathBuf;
use std::time::Duration;

use loom_domain::{ProviderEvent, RunOutcome, ThreadEventItem};
use loom_provider_protocol::ProviderSpec;
use loom_worker::acp::permission::PermissionRegistry;
use loom_worker::acp::session::{drive, Transport};
use loom_worker::provider::ProviderRun;
use tokio::sync::mpsc;

/// How long one real turn may take. A cold Pi process plus a model round trip
/// is seconds, not milliseconds; this is generous so a slow network is not
/// mistaken for a broken bridge.
const TURN_BUDGET: Duration = Duration::from_secs(180);

/// The `pi` executable this test drives.
fn pi_binary() -> String {
    std::env::var("PI_BIN").unwrap_or_else(|_| "pi".to_owned())
}

/// A run for one prompt, optionally continuing an existing agent session.
fn run(cwd: &str, prompt: &str, provider_session_id: Option<&str>) -> ProviderRun {
    let mut spec = ProviderSpec::acp_pi();
    spec.command = pi_binary();
    spec.cwd = Some(cwd.to_owned());
    ProviderRun {
        spec,
        prompt: prompt.to_owned(),
        host_id: loom_domain::HostId::mint(),
        thread_id: loom_domain::ThreadId::mint(),
        project_id: loom_domain::ProjectId::mint(),
        run_id: loom_domain::RunId::mint(),
        timeout: TURN_BUDGET,
        ceiling: loom_worker::DEFAULT_RUN_CEILING,
        // If the agent asks for permission the test has no UI, so the request
        // must settle as cancelled rather than block the turn to its deadline.
        permission_timeout: Duration::from_secs(15),
        settle_timeout: loom_worker::DEFAULT_SETTLE_TIMEOUT,
        permission_ceiling: loom_domain::HostPermissionMode::Full,
        provider_session_id: provider_session_id.map(str::to_owned),
        model: None,
        reasoning_level: None,
    }
}

/// Drives one run to completion, returning every event it reported.
async fn drive_one(run: ProviderRun) -> Vec<loom_domain::RunEvent> {
    let (tx, mut rx) = mpsc::channel(256);
    let (interactions, mut requests) =
        mpsc::channel::<loom_provider_protocol::InteractionRequest>(8);
    let (catalogs, _catalog_reports) = mpsc::channel(8);
    let (commands, _command_reports) = mpsc::channel(8);
    let transport = Transport::EmbeddedPi {
        command: run.spec.command.clone(),
        args: Vec::new(),
    };
    // Drain permission requests so the broker is never back-pressured; not
    // answering is what the broker's own timeout is for, and this test wants to
    // observe that an unanswered request ends as a cancellation rather than a
    // grant.
    let collector = tokio::spawn(async move {
        let mut seen: Vec<String> = Vec::new();
        while let Some(request) = requests.recv().await {
            seen.push(request.request_id);
        }
        seen
    });
    let handle = tokio::spawn(async move {
        let _ = drive(
            &run,
            transport,
            &tx,
            &catalogs,
            &commands,
            PermissionRegistry::new(),
            interactions,
            &loom_worker::steer::SteerRegistry::new(),
        )
        .await;
    });

    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }
    let _ = handle.await;
    drop(collector);
    events
}

/// The assistant text a run streamed, concatenated.
fn streamed_text(events: &[loom_domain::RunEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.event.body {
            ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn terminal(events: &[loom_domain::RunEvent]) -> Option<&loom_domain::RunEvent> {
    events.iter().find(|event| event.is_terminal())
}

fn identity(events: &[loom_domain::RunEvent]) -> Option<String> {
    events
        .iter()
        .find(|event| event.kind() == "thread/identity")
        .and_then(loom_domain::RunEvent::provider_thread_id)
        .map(str::to_owned)
}

/// Exactly one terminal event, whatever else happened.
fn assert_one_terminal(events: &[loom_domain::RunEvent]) {
    let terminals = events
        .iter()
        .filter(|event| event.is_terminal())
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "a run must report exactly one terminal event, got {}: {terminals:#?}",
        terminals.len()
    );
}

/// A real first turn, then a real resume of the same conversation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `pi` CLI and needs model credentials"]
async fn real_pi_runs_a_first_turn_and_resumes_it() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();

    // --- first turn -------------------------------------------------------
    let first = drive_one(run(&cwd, "Reply with exactly the single word: pong", None)).await;
    assert_one_terminal(&first);
    let finish = terminal(&first).expect("a terminal event");
    assert_eq!(
        finish.terminal_status(),
        Some(loom_domain::TurnStatus::Completed),
        "the first turn must complete: {:?}\n{first:#?}",
        finish.terminal_error()
    );
    assert_eq!(
        finish.outcome,
        Some(RunOutcome::Completed),
        "loom's own verdict is on the terminal event"
    );

    // The identity comes from the agent, not from loom: it is the ACP session id
    // a later `session/load` is keyed by.
    let session_id = identity(&first).expect("the agent names its session");
    assert!(!session_id.is_empty());
    assert!(
        first.iter().any(|event| event.kind() == "turn/started"),
        "the adapter synthesizes `turn/started`, since ACP has no turn concept"
    );

    let answer = streamed_text(&first);
    assert!(
        answer.to_ascii_lowercase().contains("pong"),
        "the real model must answer through the bridge; got {answer:?}"
    );

    // --- second turn: same conversation ----------------------------------
    let second = drive_one(run(
        &cwd,
        "What single word did you just reply with? Reply with just that word.",
        Some(&session_id),
    ))
    .await;
    assert_one_terminal(&second);
    let finish = terminal(&second).expect("a terminal event");
    assert_eq!(
        finish.terminal_status(),
        Some(loom_domain::TurnStatus::Completed),
        "the resumed turn must complete: {:?}\n{second:#?}",
        finish.terminal_error()
    );
    // The resume is visible as the same identity being reported again: that is
    // what proves `session/load` restored this conversation rather than opening
    // a new one.
    assert_eq!(
        identity(&second).as_deref(),
        Some(session_id.as_str()),
        "a resumed run reports the same agent session id"
    );

    let follow_up = streamed_text(&second);
    assert!(
        follow_up.to_ascii_lowercase().contains("pong"),
        "the resumed conversation must remember the first turn; got {follow_up:?}"
    );
}

/// The paths a worker actually depends on, against the real agent: no session
/// reported for a failed run, and one terminal event when an agent that cannot
/// be started is dispatched.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `pi` CLI and needs model credentials"]
async fn real_pi_reports_a_usable_session_mapping() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();
    let run = run(&cwd, "Reply with exactly the single word: pong", None);

    // A dispatch whose command does not exist must still end, and must not claim
    // an identity it never received.
    let mut broken = run.clone();
    broken.spec.command = PathBuf::from("/nonexistent/pi-binary")
        .to_string_lossy()
        .into_owned();
    let events = drive_one(broken).await;
    assert_one_terminal(&events);
    assert!(
        identity(&events).is_none(),
        "a run that never opened a session must not report one: {events:#?}"
    );
    assert_eq!(
        terminal(&events).and_then(|event| event.terminal_status()),
        Some(loom_domain::TurnStatus::Failed)
    );

    // The same workspace with the real binary still works after that, so the
    // failure above did not leave anything behind.
    let events = drive_one(run).await;
    assert_one_terminal(&events);
    assert!(identity(&events).is_some());
}

/// The assistant text a replay carried, concatenated per message.
fn replayed_user_texts(entries: &[ProviderEvent]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ItemStarted {
                item: ThreadEventItem::UserMessage { content, .. },
                ..
            } => content.iter().find_map(|part| match part {
                loom_domain::UserContent::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

fn replayed_assistant_text(entries: &[ProviderEvent]) -> String {
    entries
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

/// A real conversation, replayed to a real history load.
///
/// This is the real-agent half of the history acceptance. Two real turns write
/// a conversation; then `load_history` opens a connection of its own, asks for
/// a replay, and must get both turns back in order, with no prompt sent — the
/// same path the server drives when a thread outside the relay window is
/// opened. The stub tests cover the lifecycle; this one covers the agent, whose
/// session storage is the only durable copy of the conversation there is.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `pi` CLI and needs model credentials"]
async fn real_pi_replays_a_conversation_to_a_history_load() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();

    let first = drive_one(run(
        &cwd,
        "Remember the number 41. Reply with just: ok",
        None,
    ))
    .await;
    assert_one_terminal(&first);
    let session_id = identity(&first).expect("the agent names its session");

    let second = drive_one(run(
        &cwd,
        "Add one to the number I asked you to remember. Reply with just the number.",
        Some(&session_id),
    ))
    .await;
    assert_one_terminal(&second);
    assert_eq!(
        terminal(&second).and_then(|event| event.terminal_status()),
        Some(loom_domain::TurnStatus::Completed),
        "the second turn must complete: {:?}",
        terminal(&second).and_then(loom_domain::RunEvent::terminal_error)
    );

    // The load runs on its own connection, exactly as the server runs it.
    let entries = loom_worker::acp::history::load_history(
        Transport::EmbeddedPi {
            command: pi_binary(),
            args: Vec::new(),
        },
        cwd,
        loom_domain::ThreadId::mint(),
        session_id,
        loom_worker::acp::history::HistoryLimits {
            max_total_bytes: 8 * 1024 * 1024,
            budget: TURN_BUDGET,
        },
    )
    .await
    .expect("the agent replays the session it just wrote");

    let prompts = replayed_user_texts(&entries);
    assert!(
        prompts.len() >= 2,
        "both prompts are part of the conversation, in order: {prompts:#?}"
    );
    assert!(
        prompts[0].contains("41"),
        "the first prompt is the first thing replayed: {prompts:#?}"
    );
    assert!(
        prompts[1].contains("Add one"),
        "the second prompt follows it: {prompts:#?}"
    );

    let answer = replayed_assistant_text(&entries);
    assert!(
        answer.contains("42"),
        "a resumed conversation remembers what the first turn was told; got {answer:?}"
    );
}

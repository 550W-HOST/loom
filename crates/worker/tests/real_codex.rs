//! Real Codex, through its ACP bridge (`codex-acp`).
//!
//! Codex has no native ACP: `codex-acp` is a stdio ACP agent server that starts
//! Codex's own app server and translates between the two protocols. loom's
//! discovery probes that bridge like any other ACP agent
//! (`crates/worker/src/discovery.rs`), so this file is the end-to-end evidence
//! that a real bridge — not a stub — completes the handshake loom's admission
//! depends on, publishes a catalogue loom can read, streams a real turn, and
//! restores the same session for a resume and a history load.
//!
//! It needs the bridge on `PATH` and whatever credentials the bridge's Codex
//! uses, so it is `#[ignore]`d by default and run explicitly:
//!
//! ```text
//! cargo test -p loom-worker --test real_codex -- --ignored --nocapture
//! ```
//!
//! `CODEX_ACP_BIN` overrides the executable (the default is `codex-acp` on
//! `PATH`). The agent's own environment decides authentication — `CODEX_HOME`
//! for a logged-in Codex, `OPENAI_API_KEY`/`CODEX_API_KEY` for a key — and this
//! test does not touch either; it drives whatever the bridge finds.
//!
//! The assertions deliberately stop at what is *observable*: exactly one
//! terminal event, a turn that completed, an identity that came from the agent,
//! a second turn that ran against the same session id, and a catalogue with at
//! least one model and its reasoning ladder. Asserting the model's exact words
//! would make this a test of the model, not of the transport.

use std::time::Duration;

use loom_domain::{ProviderEvent, RunOutcome, ThreadEventItem};
use loom_provider_protocol::{ProviderLaunch, ProviderSpec};
use loom_worker::acp::catalog::{read_catalog, CatalogProbeOutcome};
use loom_worker::acp::history::{load_history, HistoryLimits};
use loom_worker::acp::permission::PermissionRegistry;
use loom_worker::acp::session::{drive, Transport};
use loom_worker::acp::sessions::{list_sessions, SessionListOutcome};
use loom_worker::provider::ProviderRun;
use tokio::sync::mpsc;

/// How long one real turn may take. A cold bridge (Node plus a bundled Codex)
/// plus a model round trip is seconds, not milliseconds; this is generous so a
/// slow network is not mistaken for a broken bridge.
const TURN_BUDGET: Duration = Duration::from_secs(180);

/// How long a catalogue or session-list probe may take. Opening a session is a
/// real handshake, but no model is called, so it is much tighter than a turn.
const PROBE_BUDGET: Duration = Duration::from_secs(60);

/// The `codex-acp` executable this test drives.
fn codex_acp_binary() -> String {
    std::env::var("CODEX_ACP_BIN").unwrap_or_else(|_| "codex-acp".to_owned())
}

/// The spec discovery builds for this machine's `codex-acp`.
fn spec() -> ProviderSpec {
    ProviderSpec {
        name: "codex".to_owned(),
        launch: ProviderLaunch::AcpStdio,
        command: codex_acp_binary(),
        args: Vec::new(),
        cwd: None,
    }
}

/// The stdio transport for that spec: the bridge, and only the bridge, is the
/// child process — Codex's app server is started by the bridge.
fn transport() -> Transport {
    Transport::Stdio {
        command: codex_acp_binary(),
        args: Vec::new(),
    }
}

/// A run for one prompt, optionally continuing an existing agent session.
fn run(cwd: &str, prompt: &str, provider_session_id: Option<&str>) -> ProviderRun {
    let mut spec = spec();
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
        permission_mode: loom_domain::automation::PermissionMode::Full,
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
    let transport = transport();
    // Drain any permission requests so the test harness cannot be
    // back-pressured. Full Access auto-answers ACP allow options before they
    // need an interaction frame.
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
#[ignore = "runs the real `codex-acp` bridge and needs Codex credentials"]
async fn real_codex_runs_a_first_turn_and_resumes_it() {
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
    // a later `session/load` is keyed by. Codex's bridge reports its own
    // session/thread id here, not an id loom invented.
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
    // what proves a restore of this conversation rather than a new one.
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

/// The bridge's models, read the way enrollment reads them.
///
/// This is the admission probe (`catalog_probe_requires_session: true` for
/// codex): a throwaway session, opened only to read the config options it
/// publishes. Codex names its reasoning option `reasoning_effort` rather than
/// `thought_level`, so the ladder assertion is also the check that loom's
/// category fallback works for this agent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `codex-acp` bridge and needs Codex credentials"]
async fn real_codex_publishes_a_model_catalogue() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();

    let outcome = read_catalog(transport(), cwd, PROBE_BUDGET).await;
    let CatalogProbeOutcome::Read(catalog) = outcome else {
        panic!("the bridge must answer the catalogue probe: {outcome:#?}");
    };

    assert!(
        !catalog.models.is_empty(),
        "codex offers models; a catalogue with none is a failed read: {catalog:#?}"
    );
    let current = catalog
        .current_model
        .as_deref()
        .expect("a session names the model it is on");
    assert!(
        catalog.models.iter().any(|model| model.id == current),
        "the current model must be one of the offered models: {catalog:#?}"
    );
    for model in &catalog.models {
        assert!(!model.id.is_empty(), "a model id is not empty");
        assert!(
            !model.name.is_empty(),
            "{} has no display name, so the picker would show blank",
            model.id
        );
    }
    let mut ids = catalog
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let unique = ids.len();
    ids.dedup();
    assert_eq!(unique, ids.len(), "model ids must be distinct");

    // The reasoning ladder rides the separate `thought_level` category, and
    // codex's own id for it is `reasoning_effort`. A catalogue that looked up
    // the id alone would find no option and leave the ladder empty.
    let current_models = catalog
        .models
        .iter()
        .filter(|model| Some(model.id.as_str()) == catalog.current_model.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(
        current_models.len(),
        1,
        "exactly one offered model is the session's current one: {catalog:#?}"
    );
    let ladder = &current_models[0].thinking_levels;
    assert!(
        !ladder.is_empty(),
        "the current model's reasoning ladder must be read from the \
         thought_level category, whatever id the agent chose for it: {catalog:#?}"
    );
    println!(
        "codex catalogue: {} models, current {current:?}, ladder {:?}",
        catalog.models.len(),
        ladder
            .iter()
            .map(|level| level.id.as_str())
            .collect::<Vec<_>>(),
    );
}

/// A real conversation, listed and replayed through the bridge.
///
/// The two capabilities loom's import and history paths are gated on: Codex's
/// bridge advertises `session/list` and `loadSession`, and this is the evidence
/// that a session written by a run is visible to both — without loom reading a
/// single file under `CODEX_HOME`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `codex-acp` bridge and needs Codex credentials"]
async fn real_codex_lists_and_replays_its_sessions() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();

    let first = drive_one(run(
        &cwd,
        "Remember the number 41. Reply with just: ok",
        None,
    ))
    .await;
    assert_one_terminal(&first);
    assert_eq!(
        terminal(&first).and_then(|event| event.terminal_status()),
        Some(loom_domain::TurnStatus::Completed),
        "the turn that writes the session must complete: {:?}",
        terminal(&first).and_then(loom_domain::RunEvent::terminal_error)
    );
    let session_id = identity(&first).expect("the agent names its session");

    // The session is enumerable in its own workspace, by the agent's own list.
    let listed = list_sessions(transport(), Some(cwd.clone()), PROBE_BUDGET).await;
    let SessionListOutcome::Listed { sessions, .. } = &listed else {
        panic!("the bridge advertises `session/list`, so it must be Listed: {listed:#?}");
    };
    assert!(
        sessions.iter().any(|s| s.session_id == session_id),
        "the session the run just wrote must be listed; got {sessions:#?}"
    );

    // The history load runs on its own connection, exactly as the server runs
    // it, and replays the conversation without sending a prompt.
    let entries = load_history(
        transport(),
        cwd,
        loom_domain::ThreadId::mint(),
        session_id.clone(),
        HistoryLimits {
            max_total_bytes: 8 * 1024 * 1024,
            budget: TURN_BUDGET,
        },
    )
    .await
    .expect("the bridge replays the session it just wrote");

    let prompts = replayed_user_texts(&entries);
    assert!(
        prompts.iter().any(|prompt| prompt.contains("41")),
        "the prompt the run sent is part of the replay: {prompts:#?}"
    );
}

/// The user text a replay carried.
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

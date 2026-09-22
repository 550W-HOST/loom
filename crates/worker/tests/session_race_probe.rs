//! Measurement, not a gate: what happens when a history load and a running
//! turn use one ACP session at the same time.
//!
//! `docs/acp-history-plan.md` §10.4 asked for evidence rather than an
//! assumption: a history load opens a connection of its own, and for pi that
//! means another `pi` process pointed at the same session file the running turn
//! is appending to. Nothing in pi-acp or loom defines what two connections on
//! one session do.
//!
//! This probe answers three questions with the real agent:
//!
//! 1. does a load started mid-turn return, and with what?
//! 2. does the turn finish normally while the load runs?
//! 3. is the conversation intact afterwards (a fresh load still sees both turns)?
//!
//! Run it explicitly; it needs credentials:
//!
//! ```text
//! cargo test -p loom-worker --test session_race_probe -- --ignored --nocapture
//! ```
//!
//! It prints an `OBS` line per observation and only asserts the one thing that
//! means real harm: that every turn is still in the conversation afterwards.
//!
//! **What it measured (2026-09-20, pi, two runs).** Two connections on one
//! session are tolerated: in both orderings the load succeeded (1.3-2.2s, well
//! inside the running turn's ~10s), the turn completed, nothing was lost, and
//! pi's own session file stayed valid (10 lines, 0 malformed). The one real
//! interaction is that the load returns a **snapshot without the in-flight
//! turn** — 3 entries / 1 user turn while the turn was streaming, and the full
//! 6 entries once it had finished. That is why the server refuses to install a
//! replay whose overlay moved under it: the replay is a baseline, not a live
//! view. It is also why loom does *not* build worker-side load/prompt
//! serialization, a cancel protocol or token fencing on this evidence — there is
//! no corruption to prevent, and the stale snapshot is handled where it is
//! observed.

use std::time::{Duration, Instant};

use loom_domain::{ProviderEvent, ThreadEventItem};
use loom_provider_protocol::ProviderSpec;
use loom_worker::acp::history::{load_history, HistoryLimits};
use loom_worker::acp::permission::PermissionRegistry;
use loom_worker::acp::session::{drive, Transport};
use loom_worker::provider::ProviderRun;
use tokio::sync::mpsc;

const TURN_BUDGET: Duration = Duration::from_secs(180);

fn pi_binary() -> String {
    std::env::var("PI_BIN").unwrap_or_else(|_| "pi".to_owned())
}

/// A long-answering prompt, so a load can plausibly land mid-stream.
const SLOW_PROMPT: &str = "Write the numbers 1 to 30, one per line, and nothing else.";
const SEED_PROMPT: &str = "Remember the number 41. Reply with just: ok";

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
        permission_timeout: Duration::from_secs(15),
        settle_timeout: loom_worker::DEFAULT_SETTLE_TIMEOUT,
        permission_ceiling: loom_domain::HostPermissionMode::Full,
        provider_session_id: provider_session_id.map(str::to_owned),
        model: None,
        reasoning_level: None,
    }
}

async fn drive_one(run: ProviderRun) -> Vec<loom_domain::RunEvent> {
    let (tx, mut rx) = mpsc::channel(256);
    let (interactions, mut requests) =
        mpsc::channel::<loom_provider_protocol::InteractionRequest>(8);
    let (catalogs, _catalog_reports) = mpsc::channel(8);
    let transport = Transport::EmbeddedPi {
        command: run.spec.command.clone(),
        args: Vec::new(),
    };
    let collector = tokio::spawn(async move { while requests.recv().await.is_some() {} });
    let handle = tokio::spawn(async move {
        let _ = drive(
            &run,
            transport,
            &tx,
            &catalogs,
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

fn streamed_text(events: &[loom_domain::RunEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.event.body {
            ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn terminal_status(events: &[loom_domain::RunEvent]) -> String {
    events
        .iter()
        .find(|event| event.is_terminal())
        .map(|event| {
            format!(
                "{:?}{}",
                event.terminal_status(),
                event
                    .terminal_error()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_else(|| "NO TERMINAL EVENT".to_owned())
}

fn identity(events: &[loom_domain::RunEvent]) -> Option<String> {
    events
        .iter()
        .find(|event| event.kind() == "thread/identity")
        .and_then(loom_domain::RunEvent::provider_thread_id)
        .map(str::to_owned)
}

fn user_texts(entries: &[ProviderEvent]) -> Vec<String> {
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

fn assistant_text(entries: &[ProviderEvent]) -> String {
    entries
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

async fn load(cwd: &str, session_id: &str) -> Result<Vec<ProviderEvent>, String> {
    load_history(
        Transport::EmbeddedPi {
            command: pi_binary(),
            args: Vec::new(),
        },
        cwd.to_owned(),
        loom_domain::ThreadId::mint(),
        session_id.to_owned(),
        HistoryLimits {
            max_total_bytes: 8 * 1024 * 1024,
            budget: TURN_BUDGET,
        },
    )
    .await
    .map_err(|failure| format!("{failure:?}"))
}

fn describe(label: &str, result: &Result<Vec<ProviderEvent>, String>, elapsed: Duration) {
    match result {
        Ok(entries) => println!(
            "OBS {label}: ok in {:.1}s, {} entries, {} user turns, {} chars of assistant text",
            elapsed.as_secs_f32(),
            entries.len(),
            user_texts(entries).len(),
            assistant_text(entries).len(),
        ),
        Err(error) => println!(
            "OBS {label}: FAILED in {:.1}s: {error}",
            elapsed.as_secs_f32()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `pi` CLI and needs model credentials"]
async fn a_load_and_a_turn_on_one_session() {
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();

    // Seed the conversation.
    let seed = Instant::now();
    let first = drive_one(run(&cwd, SEED_PROMPT, None)).await;
    let session_id = identity(&first).expect("the agent names its session");
    println!(
        "OBS seed turn: {} in {:.1}s, session {session_id}",
        terminal_status(&first),
        seed.elapsed().as_secs_f32()
    );

    // --- Case A: a load lands while a turn is streaming -------------------
    let turn = tokio::spawn(drive_one(run(&cwd, SLOW_PROMPT, Some(&session_id))));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    println!("OBS caseA: load starts while the turn is running");
    let load_started = Instant::now();
    let during = load(&cwd, &session_id).await;
    describe("caseA load during turn", &during, load_started.elapsed());

    let turn_events = turn.await.unwrap();
    println!(
        "OBS caseA turn: {} with {} chars streamed",
        terminal_status(&turn_events),
        streamed_text(&turn_events).chars().count()
    );

    let after_a = Instant::now();
    let reload = load(&cwd, &session_id).await;
    describe("caseA reload", &reload, after_a.elapsed());
    let kept = reload
        .as_ref()
        .map(|entries| user_texts(entries))
        .unwrap_or_default();
    println!(
        "OBS caseA integrity: prompts kept = {}, slow answer kept = {}",
        kept.len(),
        reload
            .as_ref()
            .map(|entries| assistant_text(entries).contains('7'))
            .unwrap_or(false)
    );

    // --- Case B: a turn starts while a load is in flight ------------------
    let loading = tokio::spawn({
        let cwd = cwd.clone();
        let session_id = session_id.clone();
        async move { load(&cwd, &session_id).await }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    println!("OBS caseB: turn starts while the load is running");
    let turn_b = drive_one(run(&cwd, "Reply with just: done", Some(&session_id))).await;
    println!("OBS caseB turn: {}", terminal_status(&turn_b));

    let load_b = loading.await.unwrap();
    match &load_b {
        Ok(entries) => println!(
            "OBS caseB load: ok, {} entries, {} user turns",
            entries.len(),
            user_texts(entries).len()
        ),
        Err(error) => println!("OBS caseB load: FAILED: {error}"),
    }

    let final_started = Instant::now();
    let final_load = load(&cwd, &session_id).await;
    describe("final reload", &final_load, final_started.elapsed());
    let final_entries = final_load.expect("the conversation must still be readable");
    let prompts = user_texts(&final_entries);
    for (index, prompt) in prompts.iter().enumerate() {
        println!("OBS final prompt[{index}]: {prompt:?}");
    }

    // The measured interaction, stated as a property: a load that runs while a
    // turn is streaming returns the conversation *without* the in-flight turn.
    // That is a stale snapshot, not corruption — and it is why the server
    // refuses to install a replay whose overlay moved under it.
    let during = during.expect("the mid-turn load must return");
    let during_saw_the_turn = assistant_text(&during).contains('7');
    println!("OBS property: mid-turn load saw the in-flight answer = {during_saw_the_turn}");

    // Integrity of pi's own session file, which two processes just touched.
    match session_file(&session_id) {
        Some(path) => {
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = body
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            let malformed = lines
                .iter()
                .filter(|line| serde_json::from_str::<serde_json::Value>(line).is_err())
                .count();
            println!(
                "OBS session file: {} lines, {} malformed ({} bytes)",
                lines.len(),
                malformed,
                body.len()
            );
            assert_eq!(
                malformed, 0,
                "the session file must still parse line by line"
            );
        }
        None => println!("OBS session file: not found (PI_CODING_AGENT_DIR layout unknown)"),
    }

    // The outcome that means real harm: a turn is missing from the
    // conversation. All three prompts were answered and must still be there.
    assert_eq!(
        prompts.len(),
        3,
        "every turn must still be in the conversation, got {prompts:#?}"
    );
    assert!(
        !during_saw_the_turn,
        "the replay is a snapshot, not a live view"
    );
}

/// pi's own record of where a session lives: `session-map.json` under the
/// agent dir, which is what `session/load` resolves `--session` through.
fn session_file(session_id: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let map = std::path::PathBuf::from(home)
        .join(".pi")
        .join("agent")
        .join("pi-acp")
        .join("session-map.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(map).ok()?).ok()?;
    let stored = parsed.get("sessions")?.get(session_id)?;
    let file = stored.get("sessionFile")?.as_str()?;
    Some(std::path::PathBuf::from(file))
}

//! Does a real agent's thinking reach loom's event stream?
//!
//! The client renders a "thinking" row from `item/reasoning/*` events, so a
//! missing thinking process in the UI is either an agent that produced none or a
//! translation that lost them. This asks a real `pi` directly, with the thinking
//! level raised, and prints what came back:
//!
//! ```text
//! cargo test -p loom-worker --test reasoning_probe -- --ignored --nocapture
//! ```
//!
//! It needs model credentials, like `real_pi.rs`, and is `#[ignore]`d for that
//! reason. The assertion is deliberately about the *translation*, not about the
//! model: if the agent streams thoughts at all, they must arrive as reasoning
//! events rather than being dropped on the way.

use std::time::Duration;

use loom_domain::{ProviderEvent, ReasoningLevel, RunEvent};
use loom_provider_protocol::ProviderSpec;
use loom_worker::acp::permission::PermissionRegistry;
use loom_worker::acp::session::{drive, Transport};
use loom_worker::provider::ProviderRun;
use tokio::sync::mpsc;

const TURN_BUDGET: Duration = Duration::from_secs(180);

fn pi_binary() -> String {
    std::env::var("PI_BIN").unwrap_or_else(|_| "pi".to_owned())
}

async fn drive_one(
    run: ProviderRun,
    transport: Transport,
) -> (Vec<RunEvent>, Option<loom_domain::catalog::ProviderCatalog>) {
    let (tx, mut rx) = mpsc::channel(256);
    let (interactions, requests) = mpsc::channel::<loom_provider_protocol::InteractionRequest>(8);
    let (catalogs, mut catalog_reports) = mpsc::channel(8);
    let collector = tokio::spawn(async move {
        let mut seen: Vec<String> = Vec::new();
        let mut requests = requests;
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
    // The catalogue report the session emitted is the only place the test can
    // see which model the agent actually settled on.
    let catalog = catalog_reports.try_recv().ok().map(|report| report.catalog);
    (events, catalog)
}

/// The thinking a run streamed, concatenated.
fn streamed_thinking(events: &[RunEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.event.body {
            ProviderEvent::ItemReasoningTextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the real `pi` CLI and needs model credentials"]
async fn a_real_turn_reports_its_thinking() {
    // `PROBE_CWD` aims the turn at a directory the probe wants the agent to see
    // — a real file to read, say; otherwise the turn runs in a fresh temp dir.
    let asked_cwd = std::env::var("PROBE_CWD").ok();
    let workspace = asked_cwd.is_none().then(|| tempfile::tempdir().unwrap());
    let cwd = asked_cwd.unwrap_or_else(|| {
        workspace
            .as_ref()
            .expect("a temp workspace when PROBE_CWD is unset")
            .path()
            .to_string_lossy()
            .into_owned()
    });
    let mut spec = ProviderSpec::acp_pi();
    spec.command = pi_binary();
    spec.cwd = Some(cwd.clone());
    let run = ProviderRun {
        spec,
        model: std::env::var("PROBE_MODEL").ok(),
        prompt: std::env::var("PROBE_PROMPT").unwrap_or_else(|_| {
            "Think it through before answering: how many distinct ways can you arrange the \
             letters of the word BANANA?"
                .to_owned()
        }),
        host_id: loom_domain::HostId::mint(),
        thread_id: loom_domain::ThreadId::mint(),
        project_id: loom_domain::ProjectId::mint(),
        run_id: loom_domain::RunId::mint(),
        timeout: TURN_BUDGET,
        permission_timeout: Duration::from_secs(15),
        permission_ceiling: loom_domain::HostPermissionMode::Full,
        provider_session_id: None,
        reasoning_level: Some(ReasoningLevel::from(
            std::env::var("PROBE_REASONING").unwrap_or_else(|_| "high".to_owned()),
        )),
    };

    let transport = Transport::EmbeddedPi {
        command: run.spec.command.clone(),
        args: Vec::new(),
    };
    let (events, catalog) = drive_one(run, transport).await;
    if let Some(catalog) = &catalog {
        println!("session model: {:?}", catalog.current_model);
        for model in &catalog.models {
            println!(
                "  option value {:?}  ladder={:?} default={:?}",
                model.id,
                model
                    .thinking_levels
                    .iter()
                    .map(|l| l.id.as_str())
                    .collect::<Vec<_>>(),
                model.default_thinking_level
            );
        }
    }
    if std::env::var("PROBE_DUMP").is_ok() {
        for event in &events {
            println!(
                "EVENT {}",
                serde_json::to_string(&event.event.body).unwrap_or_default()
            );
        }
    }
    for event in &events {
        match &event.event.body {
            ProviderEvent::ProviderModelFallback {
                original_model,
                fallback_model,
                ..
            } => println!("model fallback: {original_model} -> {fallback_model}"),
            ProviderEvent::ProviderWarning { summary, .. } => {
                println!("warning: {summary:?}")
            }
            _ => {}
        }
    }
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event.kind())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let thinking = streamed_thinking(&events);
    println!("event kinds: {kinds:?}");
    println!("thinking bytes: {}", thinking.len());
    println!(
        "thinking head: {:?}",
        thinking.chars().take(200).collect::<String>()
    );

    let terminal = events.iter().find(|event| event.is_terminal());
    println!("terminal: {:?}", terminal.map(|event| event.kind()));
    assert!(
        !thinking.is_empty(),
        "the agent streamed no reasoning at all; kinds were {kinds:?}"
    );
}

/// Which of the agents installed here report thinking when asked the same thing.
///
/// A missing thinking process in the UI is per-agent far more often than it is a
/// rendering bug: the level a client picks only reaches an agent that publishes
/// its levels over ACP v2 config options, and a v1 agent decides for itself.
/// This prints what each one actually streams.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the agents installed on this machine and needs their credentials"]
async fn each_discovered_agent_reports_what_it_thinks() {
    use loom_provider_protocol::ProviderLaunch;

    for spec in loom_worker::discovery::candidates() {
        let transport = match spec.launch {
            ProviderLaunch::AcpStdio => Transport::Stdio {
                command: spec.command.clone(),
                args: spec.args.clone(),
            },
            ProviderLaunch::AcpEmbeddedPi => Transport::EmbeddedPi {
                command: spec.command.clone(),
                args: spec.args.clone(),
            },
        };
        let workspace = tempfile::tempdir().unwrap();
        let cwd = workspace.path().to_string_lossy().into_owned();
        let mut spec = spec;
        spec.cwd = Some(cwd.clone());
        let run = ProviderRun {
            spec: spec.clone(),
            prompt: "Think it through before answering: how many distinct ways can you arrange \
                     the letters of the word BANANA?"
                .to_owned(),
            host_id: loom_domain::HostId::mint(),
            thread_id: loom_domain::ThreadId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            run_id: loom_domain::RunId::mint(),
            timeout: TURN_BUDGET,
            permission_timeout: Duration::from_secs(15),
            permission_ceiling: loom_domain::HostPermissionMode::Full,
            provider_session_id: None,
            model: None,
            reasoning_level: Some(ReasoningLevel::from("high")),
        };

        let (events, catalog) = drive_one(run, transport).await;
        if let Some(catalog) = &catalog {
            println!("          session model: {:?}", catalog.current_model);
        }
        let thinking = streamed_thinking(&events);
        let terminal = events
            .iter()
            .find(|event| event.is_terminal())
            .map(|event| event.kind())
            .unwrap_or("none");
        println!(
            "{:9} terminal={terminal:15} thinking={:5} bytes  kinds={:?}",
            spec.name,
            thinking.len(),
            events
                .iter()
                .map(|event| event.kind())
                .collect::<std::collections::BTreeSet<_>>(),
        );
    }
}

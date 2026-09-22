//! The embedded `pi-acp` transport, driven through a real run.
//!
//! `acp_session.rs` proves the client half works against a `pi-acp` *process*.
//! Here there is no adapter process at all: `pi-acp`'s `AcpAgent` runs on a
//! task inside the worker and the two halves are joined by an in-process
//! channel pair. Only the stub `pi` is a child.
//!
//! The stub speaks the Pi JSONL protocol *behind* the embedded `pi-acp` agent,
//! so the test exercises the real ACP-to-Pi translation without an LLM or auth.
//! It is written here rather than reused from pi-acp because pi-acp's mock lives
//! in its binary, and cargo does not build a dependency's binary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_domain::RunOutcome;
use loom_provider_protocol::ProviderSpec;
use loom_worker::acp::permission::PermissionRegistry;
use loom_worker::acp::session::{drive, Transport};
use loom_worker::provider::ProviderRun;
use tokio::sync::mpsc;

/// The JSONL `pi` replies with: a message start, one text delta, the
/// authoritative message end, then the settle that ends the turn.
///
/// These are the events `pi-acp` translates; matching them means the test
/// exercises the real translation rather than a shortcut.
/// A stub Pi speaking the private JSONL protocol consumed internally by
/// `pi-acp`. The outer worker path is still ACP.
///
/// Written here rather than reused from pi-acp because pi-acp's mock lives in
/// its *binary*, and cargo does not build a dependency's binary. The shapes
/// below are copied from that mock, so the stub is driven by the same protocol
/// real pi uses.
///
/// `pi` answers each request with a response naming the command it came from;
/// pi-acp routes responses by `id` and validates `command`, so both are echoed.
const STUB_PI: &str = r#"
model() {
  printf '{"id":"stub-model","name":"Stub Model","provider":"stub","reasoning":true,"contextWindow":200000,"maxTokens":4096}'
}
while IFS= read -r line; do
  printf '%s\n' "$line" >> "${PI_STUB_CAPTURE:-/dev/null}"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  cmd=$(printf '%s' "$line" | sed -n 's/.*"type":"\([^"]*\)".*/\1/p')
  case "$cmd" in
    get_state)
      printf '{"id":"%s","type":"response","command":"get_state","success":true,"data":{"thinkingLevel":"medium","isStreaming":false,"isCompacting":false,"steeringMode":"all","followUpMode":"all","sessionId":"stub","autoCompactionEnabled":false,"messageCount":0,"pendingMessageCount":0}}\n' "$id"
      ;;
    get_available_models)
      printf '{"id":"%s","type":"response","command":"get_available_models","success":true,"data":{"models":[%s]}}\n' "$id" "$(model)"
      ;;
    get_available_thinking_levels)
      printf '{"id":"%s","type":"response","command":"get_available_thinking_levels","success":true,"data":{"levels":["off","low","medium","high"]}}\n' "$id"
      ;;
    get_commands)
      printf '{"id":"%s","type":"response","command":"get_commands","success":true,"data":{"commands":%s}}\n' "$id" "${PI_STUB_COMMANDS:-[]}"
      ;;
    get_session_stats)
      printf '{"id":"%s","type":"response","command":"get_session_stats","success":true,"data":{"sessionId":"stub","userMessages":1,"assistantMessages":1,"toolCalls":0,"toolResults":0,"totalMessages":2,"tokens":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"total":15},"cost":0}}\n' "$id"
      ;;
    get_messages)
      printf '{"id":"%s","type":"response","command":"get_messages","success":true,"data":{"messages":[]}}\n' "$id"
      ;;
    set_model)
      printf '{"id":"%s","type":"response","command":"set_model","success":true,"data":%s}\n' "$id" "$(model)"
      ;;
    prompt)
      printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
      printf '%s\n' '{"type":"message_start","message":{"role":"assistant","content":[]}}'
      printf '%s\n' '{"type":"message_update","usage":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hello from embedded"}}'
      printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hello from embedded"}],"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15}}}'
      printf '%s\n' '{"type":"agent_settled"}'
      ;;
    *)
      printf '{"id":"%s","type":"response","command":"%s","success":true}\n' "$id" "$cmd"
      ;;
  esac
done
"#;

fn write_stub_pi(dir: &Path) -> PathBuf {
    write_stub_pi_with(dir, "")
}

/// Like [`write_stub_pi`], but the stub records every JSONL line `pi-acp` sends
/// it to `capture`. That is how a test sees the prompt the adapter actually
/// delivered — after its own slash-command expansion — instead of only the
/// answer the stub was scripted to give.
fn write_stub_pi_capturing(dir: &Path, capture: &Path) -> PathBuf {
    write_stub_pi_with(
        dir,
        &format!("export PI_STUB_CAPTURE='{}'\n", capture.display()),
    )
}

fn write_stub_pi_with(dir: &Path, prelude: &str) -> PathBuf {
    let path = dir.join("pi");
    std::fs::write(&path, format!("#!/bin/sh\n{prelude}{STUB_PI}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn run(cwd: &str, pi: &Path) -> ProviderRun {
    run_resuming(cwd, pi, None)
}

/// A run that continues `provider_session_id` when one is given.
fn run_resuming(cwd: &str, pi: &Path, provider_session_id: Option<&str>) -> ProviderRun {
    let mut spec = ProviderSpec::acp_pi();
    spec.command = pi.to_string_lossy().into_owned();
    spec.cwd = Some(cwd.to_string());
    ProviderRun {
        spec,
        prompt: "say hello".to_string(),
        host_id: loom_domain::HostId::mint(),
        thread_id: loom_domain::ThreadId::mint(),
        project_id: loom_domain::ProjectId::mint(),
        run_id: loom_domain::RunId::mint(),
        timeout: Duration::from_secs(30),
        ceiling: loom_worker::DEFAULT_RUN_CEILING,
        permission_timeout: Duration::from_secs(5),
        settle_timeout: loom_worker::DEFAULT_SETTLE_TIMEOUT,
        permission_ceiling: loom_domain::HostPermissionMode::Full,
        provider_session_id: provider_session_id.map(str::to_owned),
        model: None,
        reasoning_level: None,
    }
}

async fn drive_embedded(run: ProviderRun) -> Vec<loom_domain::RunEvent> {
    drive_embedded_collecting(run).await.0
}

/// The same drive, also returning the command lists the session advertised.
///
/// A command list is not an event, so it leaves on its own channel; a test that
/// wants to see it must collect that channel rather than the event stream.
async fn drive_embedded_collecting(
    run: ProviderRun,
) -> (
    Vec<loom_domain::RunEvent>,
    Vec<loom_provider_protocol::ProviderCommandsReport>,
) {
    let (tx, mut rx) = mpsc::channel(64);
    let transport = Transport::EmbeddedPi {
        command: run.spec.command.clone(),
        args: Vec::new(),
    };
    let (interactions, _requests) = mpsc::channel(8);
    let (catalogs, _catalog_reports) = mpsc::channel(8);
    let (commands, mut command_reports) = mpsc::channel(8);
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
    // The sender is owned by the finished task, so after the join everything
    // it sent is queued and drained here.
    let mut reports = Vec::new();
    while let Ok(report) = command_reports.try_recv() {
        reports.push(report);
    }
    (events, reports)
}

/// The whole point: Pi is reached with no adapter process.
#[tokio::test(flavor = "multi_thread")]
async fn an_embedded_pi_acp_agent_runs_a_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let pi = write_stub_pi(tmp.path());
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let events = drive_embedded(run(&workspace.to_string_lossy(), &pi)).await;

    let terminals: Vec<_> = events.iter().filter(|e| e.is_terminal()).collect();
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal event: {events:#?}"
    );
    assert_eq!(
        terminals[0].outcome,
        Some(RunOutcome::Completed),
        "the stub turn completed: {events:#?}"
    );

    // The agent's own words travel through pi-acp's translation, so a broken
    // embedding shows up as missing content rather than only as a lifecycle
    // difference.
    let text: String = events
        .iter()
        .filter_map(|e| match &e.event.body {
            loom_domain::ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("hello from embedded"),
        "the agent's message reached the log: {text:?}"
    );
}

/// A built-in slash command is answered by the embedded adapter itself: no pi
/// turn runs, and the answer is the command's own output. This is the execution
/// half of the command contract — the menu can list a command, but only this
/// pins that sending it does something.
#[tokio::test(flavor = "multi_thread")]
async fn a_builtin_slash_command_runs_without_a_pi_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let pi = write_stub_pi(tmp.path());
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut run = run(&workspace.to_string_lossy(), &pi);
    run.prompt = "/session".to_string();
    let events = drive_embedded(run).await;

    let terminals: Vec<_> = events.iter().filter(|e| e.is_terminal()).collect();
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal event: {events:#?}"
    );
    assert_eq!(
        terminals[0].outcome,
        Some(RunOutcome::Completed),
        "a command turn is a completed turn: {events:#?}"
    );

    let text: String = events
        .iter()
        .filter_map(|e| match &e.event.body {
            loom_domain::ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("Session: stub"),
        "the built-in's answer reached the log: {text:?}"
    );
}

/// A workspace prompt file is expanded by the embedded adapter before the
/// prompt reaches pi: `/name args` must arrive as the template body with the
/// argument substituted, not as the literal command line.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_prompt_file_is_expanded_before_pi_sees_it() {
    let tmp = tempfile::tempdir().unwrap();
    let capture = tmp.path().join("pi-input.jsonl");
    let pi = write_stub_pi_capturing(tmp.path(), &capture);
    let workspace = tmp.path().join("workspace");
    // A name no developer's own prompt directory is likely to hold: the user
    // prompts directory is scanned too, and a same-named file there would take
    // the expansion away from the workspace's copy.
    let prompts = workspace.join(".pi/prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(
        prompts.join("loom-embedded-expansion-check.md"),
        "---\ndescription: Test expansion\n---\nExpanded body for: $@\n",
    )
    .unwrap();

    let mut run = run(&workspace.to_string_lossy(), &pi);
    run.prompt = "/loom-embedded-expansion-check src/main.rs".to_string();
    let events = drive_embedded(run).await;

    let terminal = events
        .iter()
        .find(|e| e.is_terminal())
        .expect("a run whose prompt is a file command still ends");
    assert_eq!(terminal.outcome, Some(RunOutcome::Completed), "{events:#?}");

    let sent = std::fs::read_to_string(&capture).unwrap();
    assert!(
        sent.contains("Expanded body for: src/main.rs"),
        "pi received the raw command instead of the expanded template: {sent}"
    );
}

/// The agent's command advertisement reaches the control plane as its own
/// report, not as a timeline event: the event log must not carry a UI
/// affordance, but the menu still needs the agent's own list.
#[tokio::test(flavor = "multi_thread")]
async fn the_advertised_command_list_is_reported_out_of_band() {
    let tmp = tempfile::tempdir().unwrap();
    // pi's `get_commands` shape: a prompt template and a skill. pi-acp turns
    // these into the ACP advertisement; extension-sourced commands are
    // deliberately not advertised, so they are not in this list.
    let commands = r#"[{"name":"review","description":"Review the diff","source":"prompt","location":"project"},{"name":"skill:search","description":"Web search","source":"skill","location":"user"}]"#;
    let pi = write_stub_pi_with(
        tmp.path(),
        &format!("export PI_STUB_COMMANDS='{commands}'\n"),
    );
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let run = run(&workspace.to_string_lossy(), &pi);
    let host_id = run.host_id.clone();
    let (events, reports) = drive_embedded_collecting(run).await;

    let terminal = events
        .iter()
        .find(|e| e.is_terminal())
        .expect("a run whose agent advertises commands still ends");
    assert_eq!(terminal.outcome, Some(RunOutcome::Completed), "{events:#?}");

    let report = reports
        .iter()
        .find(|report| report.commands.iter().any(|c| c.name == "review"))
        .expect("the advertised command list was not reported");
    assert_eq!(report.host_id, host_id);
    assert_eq!(report.provider_id, "pi");
    assert_eq!(report.cwd, workspace.to_string_lossy());
    assert!(report.commands.iter().any(|c| c.name == "skill:search"));
}

/// The lifecycle events loom synthesizes are present on this path too, which is
/// what makes the two transports interchangeable.
#[tokio::test(flavor = "multi_thread")]
async fn the_embedded_path_states_the_same_lifecycle_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let pi = write_stub_pi(tmp.path());
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let events = drive_embedded(run(&workspace.to_string_lossy(), &pi)).await;
    let kinds: Vec<_> = events.iter().map(|e| e.kind()).collect();

    assert!(kinds.contains(&"thread/identity"), "{kinds:?}");
    assert!(kinds.contains(&"turn/started"), "{kinds:?}");
    assert!(kinds.contains(&"turn/completed"), "{kinds:?}");

    let identity = kinds.iter().position(|k| *k == "thread/identity").unwrap();
    let turn = kinds.iter().position(|k| *k == "turn/started").unwrap();
    let terminal = kinds.iter().position(|k| *k == "turn/completed").unwrap();
    assert!(
        identity < terminal && turn < terminal,
        "lifecycle facts precede the terminal: {kinds:?}"
    );
}

/// A `pi` that cannot start fails the run rather than hanging it.
#[tokio::test(flavor = "multi_thread")]
async fn an_embedded_agent_whose_pi_is_missing_fails_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let events = drive_embedded(run(
        &workspace.to_string_lossy(),
        Path::new("/nonexistent/pi"),
    ))
    .await;

    let terminal = events
        .iter()
        .find(|e| e.is_terminal())
        .expect("a run whose pi is missing still ends");
    assert_eq!(
        terminal.terminal_status(),
        Some(loom_domain::TurnStatus::Failed),
        "{events:#?}"
    );
}

/// Arguments cannot be honoured on this path, and saying so beats dropping
/// them: an operator who set `LOOM_PROVIDER_ARGS` would otherwise see them
/// silently ignored.
#[tokio::test(flavor = "multi_thread")]
async fn provider_arguments_are_refused_rather_than_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut run = run(&workspace.to_string_lossy(), &write_stub_pi(tmp.path()));
    run.spec.args = vec!["--mock-delay-ms".to_string(), "5".to_string()];
    let transport = Transport::EmbeddedPi {
        command: run.spec.command.clone(),
        args: run.spec.args.clone(),
    };

    // `drive` turns a pre-terminal failure into a terminal event, so the
    // refusal surfaces there rather than as a returned error.
    let (tx, mut rx) = mpsc::channel(64);
    let (interactions, _requests) = mpsc::channel(8);
    let (catalogs, _catalog_reports) = mpsc::channel(8);
    let (commands, _command_reports) = mpsc::channel(8);
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
    drop(tx);
    let mut events = Vec::new();
    while let Some(report) = rx.recv().await {
        events.push(report.event);
    }

    let terminal = events
        .iter()
        .find(|e| e.is_terminal())
        .expect("the run ends");
    let message = terminal.terminal_error().unwrap_or_default();
    assert!(
        message.contains("no provider arguments"),
        "the refusal explains the limitation rather than dropping the arguments: {message}"
    );
}

/// The two transports state the same lifecycle facts.
///
/// This is the property that makes embedding worth it: a consumer cannot tell
/// from the log which transport carried the turn. The *content* differs because
/// the agents differ, but the facts loom owns — identity, the turn opening and
/// closing — must not.
#[tokio::test(flavor = "multi_thread")]
async fn both_transports_state_the_same_lifecycle_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let pi = write_stub_pi(tmp.path());
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let embedded = drive_embedded(run(&workspace.to_string_lossy(), &pi)).await;

    // The facts loom synthesizes, which must be present whichever transport is
    // used. Everything else in the log comes from the agent and legitimately
    // varies.
    let facts = |events: &[loom_domain::RunEvent]| {
        let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();
        (
            kinds.iter().filter(|k| **k == "thread/identity").count(),
            kinds.iter().filter(|k| **k == "turn/started").count(),
            kinds.iter().filter(|k| **k == "turn/completed").count(),
            kinds.iter().position(|k| *k == "thread/identity"),
            kinds.iter().position(|k| *k == "turn/started"),
        )
    };

    let (identity, turns, terminals, identity_at, turn_at) = facts(&embedded);
    assert_eq!(identity, 1, "identity once: {embedded:#?}");
    assert_eq!(turns, 1, "one turn opened: {embedded:#?}");
    assert_eq!(terminals, 1, "one turn closed: {embedded:#?}");
    assert!(
        identity_at < turn_at,
        "identity precedes the turn: {embedded:#?}"
    );
    // Nothing precedes identity: it is the first fact about the thread.
    assert_eq!(
        identity_at,
        Some(0),
        "identity opens the log: {embedded:#?}"
    );
}

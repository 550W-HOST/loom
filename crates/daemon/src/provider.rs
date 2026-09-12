//! The provider bridge: spawn a provider CLI, speak its protocol, normalise its
//! output into contract [`RunEvent`]s.
//!
//! This is the part that actually touches an agent. It is provider-specific on
//! purpose — the control plane never sees Pi's event shape — but the two
//! properties it enforces are not:
//!
//! 1. **A stdout guard.** A provider is not required to keep stdout clean.
//!    Every line goes through [`guard_stdout_line`] first; only a JSON object
//!    with a string `type` is treated as a protocol frame, and everything else
//!    is routed to stderr *before* it can reach the frame mapper. bb #1180 is
//!    the failure this prevents: Pi's OSC 777 notification wedged a turn
//!    forever because the bridge fed arbitrary stdout into a JSON parser.
//! 2. **Exactly one terminal event.** The run loop always ends by emitting
//!    exactly one `turn/completed` (bb's contract terminal event), whether the
//!    provider settled, exited non-zero, reported a protocol failure or hit the
//!    deadline. A path that returned without one is what would let a thread
//!    stay `working`; there is deliberately no such path.
//!
//! # Why the events are contract events
//!
//! The daemon is the only place that knows a provider's wire format. It is
//! therefore the only correct place to translate Pi's frames into bb's
//! `ThreadEvent` union: the projection layer consumes that union directly, so
//! translating here is what keeps provider dialect from leaking to the UI. The
//! mapping lives in [`PiBridge`]; `docs/event-model.md` records the per-type
//! decisions.
//!
//! [`guard_stdout_line`]: loom_provider_protocol::guard_stdout_line

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use loom_domain::{
    FileChange, FileChangeKind, ItemStatus, ProviderEvent, ProviderEventType,
    ProviderWarningCategory, RunEvent, RunOutcome, SearchMode, ThreadEventItem, TurnError,
    TurnStatus,
};
use loom_provider_protocol::{
    guard_stdout_line, GuardedLine, ProviderReport, ProviderSpec, RunDispatch,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Everything one provider process needs.
#[derive(Clone, Debug)]
pub struct ProviderRun {
    /// How to start the provider.
    pub spec: ProviderSpec,
    /// The user turn that started the run.
    pub prompt: String,
    /// The host executing it, echoed in every report.
    pub host_id: loom_domain::HostId,
    /// The thread being advanced.
    pub thread_id: loom_domain::ThreadId,
    /// Its project.
    pub project_id: loom_domain::ProjectId,
    /// The run's identity.
    pub run_id: loom_domain::RunId,
    /// Daemon-side deadline. On expiry the process is killed and the run is
    /// reported `timed_out`.
    pub timeout: Duration,
    /// Base directory for per-thread provider sessions, when the provider
    /// supports resuming one.
    pub session_dir: Option<PathBuf>,
}

impl ProviderRun {
    /// Builds a run from a dispatch and the daemon's local overrides.
    pub fn from_dispatch(
        dispatch: &RunDispatch,
        spec: ProviderSpec,
        timeout: Duration,
        session_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            spec,
            prompt: dispatch.prompt.clone(),
            host_id: dispatch.host_id.clone(),
            thread_id: dispatch.thread_id.clone(),
            project_id: dispatch.project_id.clone(),
            run_id: dispatch.run_id.clone(),
            timeout,
            session_dir,
        }
    }

    fn report(&self, event: RunEvent) -> ProviderReport {
        ProviderReport {
            host_id: self.host_id.clone(),
            event,
        }
    }

    /// The provider session id loom reports as `providerThreadId`.
    ///
    /// loom runs exactly one provider session per thread, keyed by the thread
    /// id (see [`effective_argv`], which passes it as `--session-id`), so the
    /// thread id *is* the session identity. Reporting it keeps turn grouping
    /// stable across runs.
    fn provider_thread_id(&self) -> String {
        self.thread_id.to_string()
    }

    /// The workspace the provider runs in, which is the only source of a
    /// `cwd` for a tool item.
    fn cwd(&self) -> Option<String> {
        self.spec.cwd.clone()
    }
}

/// Spawns a provider and forwards its reports.
///
/// The task always emits exactly one terminal `turn/completed`; any failure
/// that happens before the provider settles is reported as a failed turn from
/// here.
pub fn spawn(
    run: ProviderRun,
    reports: mpsc::Sender<ProviderReport>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(message) = drive(&run, &reports).await {
            let _ = reports
                .send(run.report(terminal(&run, RunOutcome::Failed, &message)))
                .await;
        }
    })
}

/// Builds the terminal `turn/completed` event for an outcome.
fn terminal(run: &ProviderRun, outcome: RunOutcome, message: &str) -> RunEvent {
    let body = match outcome {
        RunOutcome::Completed => ProviderEvent::TurnCompleted {
            provider_thread_id: Some(run.provider_thread_id()),
            status: TurnStatus::Completed,
            error: None,
            provider_checkpoint_id: None,
        },
        other => ProviderEvent::TurnCompleted {
            provider_thread_id: Some(run.provider_thread_id()),
            status: other.turn_status(),
            error: Some(TurnError {
                message: message.to_owned(),
            }),
            provider_checkpoint_id: None,
        },
    };
    RunEvent::terminal(
        run.thread_id.clone(),
        run.project_id.clone(),
        run.run_id.clone(),
        now_ms(),
        outcome,
        body,
    )
}

/// Spawns the process, streams frames until a terminal one, and guarantees the
/// process is reaped.
///
/// Returns `Err` only for failures *before* a terminal event was sent. Once a
/// terminal was sent this returns `Ok(())`, so the caller's fallback cannot
/// double-report.
async fn drive(run: &ProviderRun, reports: &mpsc::Sender<ProviderReport>) -> Result<(), String> {
    let argv = effective_argv(&run.spec, &run.thread_id, run.session_dir.as_deref());
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "provider argv is empty".to_owned())?;

    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &run.spec.cwd {
        // The control plane names the workspace; the daemon is the only party
        // that can see *this* machine's filesystem, so it validates the
        // directory here. A missing directory is a hard error, never a silent
        // fallback to this process's cwd — that fallback is the bug this path
        // exists to prevent.
        if !Path::new(cwd).is_dir() {
            return Err(format!(
                "the dispatched working directory {cwd:?} does not exist on this host"
            ));
        }
        command.current_dir(cwd);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not spawn `{program}`: {error}"))?;

    if let Some(stderr) = child.stderr.take() {
        let name = run.spec.name.clone();
        tokio::spawn(drain_stderr(stderr, name));
    }

    let mut stdin = child.stdin.take();
    let prompt = format!("{}\n", json!({ "type": "prompt", "message": run.prompt }));
    if let Some(handle) = stdin.as_mut() {
        handle
            .write_all(prompt.as_bytes())
            .await
            .map_err(|error| format!("could not write the prompt: {error}"))?;
        handle
            .flush()
            .await
            .map_err(|error| format!("could not flush the prompt: {error}"))?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "provider has no stdout".to_owned())?;
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + run.timeout;
    let mut bridge = PiBridge::new(run);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = match tokio::time::timeout(remaining, lines.next_line()).await {
            // The deadline passed with no terminal event: kill and report.
            Err(_elapsed) => {
                terminate(&mut child).await;
                let event = terminal(
                    run,
                    RunOutcome::TimedOut,
                    &format!(
                        "provider did not settle within {}ms",
                        run.timeout.as_millis()
                    ),
                );
                let _ = reports.send(run.report(event)).await;
                return Ok(());
            }
            Ok(Ok(Some(line))) => line,
            // EOF: the process closed stdout. Fall through to the exit check.
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                terminate(&mut child).await;
                let event = terminal(
                    run,
                    RunOutcome::Failed,
                    &format!("provider stdout failed: {error}"),
                );
                let _ = reports.send(run.report(event)).await;
                return Ok(());
            }
        };

        let frame = match guard_stdout_line(&next) {
            // Pollution never reaches the frame mapper.
            GuardedLine::Stray(text) => {
                if !text.trim().is_empty() {
                    eprintln!("[provider:{}] {text}", run.spec.name);
                }
                continue;
            }
            GuardedLine::Frame(frame) => frame,
        };

        // A dialog request blocks the provider until answered. There is no UI
        // on this path yet, so decline it explicitly rather than hang, and tell
        // the client what was declined.
        if let Some(response) = auto_cancel_response(&frame) {
            if let Some(handle) = stdin.as_mut() {
                let _ = handle.write_all(format!("{response}\n").as_bytes()).await;
                let _ = handle.flush().await;
            }
        }

        let events = bridge.observe(&frame, now_ms());
        let mut settled = false;
        for event in events {
            let terminal = event.is_terminal();
            // A closed channel means the daemon is gone; stop reading.
            if reports.send(run.report(event)).await.is_err() {
                terminate(&mut child).await;
                return Ok(());
            }
            if terminal {
                settled = true;
            }
        }
        if settled {
            terminate(&mut child).await;
            return Ok(());
        }
    }

    // The stream ended without a terminal frame. Interpret the exit status.
    let status = child
        .wait()
        .await
        .map_err(|error| format!("could not wait for the provider: {error}"))?;
    let message = if status.success() {
        "provider exited without settling".to_owned()
    } else {
        format!("provider exited with {status}")
    };
    let event = terminal(run, RunOutcome::Failed, &message);
    let _ = reports.send(run.report(event)).await;
    Ok(())
}

/// Kills and reaps a child. SIGKILL plus `wait`, so no zombie is left behind.
async fn terminate(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Drains a provider's stderr into this process's stderr.
///
/// A provider's diagnostics are not part of the protocol and are not turned
/// into events; forwarding them is how an operator sees them without letting
/// them near the frame parser.
async fn drain_stderr(stderr: tokio::process::ChildStderr, provider: String) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[provider:{provider}] {line}");
    }
}

/// The argv to spawn, with per-thread session continuity for Pi.
///
/// The control plane dispatches a stateless provider spec. Keeping a thread's
/// conversation across runs is an execution-plane concern, so the daemon adds
/// the session arguments here: a stable session directory plus the thread id as
/// the session id. For any other provider the spec is used verbatim.
pub fn effective_argv(
    spec: &ProviderSpec,
    thread_id: &loom_domain::ThreadId,
    session_dir: Option<&Path>,
) -> Vec<String> {
    let mut argv = spec.argv();
    if spec.name == "pi" {
        if let Some(dir) = session_dir {
            argv.retain(|arg| arg != "--no-session");
            argv.push("--session-dir".into());
            argv.push(dir.to_string_lossy().into_owned());
            argv.push("--session-id".into());
            argv.push(thread_id.to_string());
        }
    }
    argv
}

/// A response that declines an interactive extension request.
///
/// Returns `None` for anything that is not a blocking dialog method, so the
/// fire-and-forget notifications pass through untouched.
pub fn auto_cancel_response(frame: &Value) -> Option<String> {
    if frame.get("type").and_then(Value::as_str)? != "extension_ui_request" {
        return None;
    }
    let method = frame.get("method").and_then(Value::as_str)?;
    if !matches!(method, "select" | "confirm" | "input" | "editor") {
        return None;
    }
    Some(
        json!({
            "type": "extension_ui_response",
            "id": frame.get("id").cloned().unwrap_or(Value::Null),
            "cancelled": true,
        })
        .to_string(),
    )
}

/// Wall-clock milliseconds, from the relay's clock so every producer agrees.
fn now_ms() -> u64 {
    loom_relay::now_ms()
}

/// Translates one run's Pi frames into contract events.
///
/// The bridge is stateful because several contract types carry an *item id*
/// that a later event must refer to: Pi's stream is keyed by content index and
/// tool call id, while the contract wants a stable per-item id. Holding that
/// mapping here is what lets the projection pair a delta with its `item/start`
/// and `item/completed`.
///
/// Public so a caller (and the conformance tests) can drive the translation
/// directly instead of through a child process.
pub struct PiBridge {
    provider: String,
    run_id: loom_domain::RunId,
    thread_id: loom_domain::ThreadId,
    project_id: loom_domain::ProjectId,
    provider_thread_id: String,
    cwd: Option<String>,
    /// Whether `thread/identity` has been emitted for this run.
    identified: bool,
    /// How many assistant messages have begun, for minting their item ids.
    assistant_seq: u64,
    /// The item id of the assistant message currently streaming.
    assistant_id: Option<String>,
    /// The item id of each thinking block, keyed by Pi's content index.
    thinking_ids: HashMap<u64, String>,
    /// Tool items by Pi tool call id, kept so the terminal event can reuse the
    /// shape the start event established.
    tools: HashMap<String, ThreadEventItem>,
}

impl PiBridge {
    /// Builds a bridge for one run.
    pub fn new(run: &ProviderRun) -> Self {
        Self {
            provider: run.spec.name.clone(),
            run_id: run.run_id.clone(),
            thread_id: run.thread_id.clone(),
            project_id: run.project_id.clone(),
            provider_thread_id: run.provider_thread_id(),
            cwd: run.cwd(),
            identified: false,
            assistant_seq: 0,
            assistant_id: None,
            thinking_ids: HashMap::new(),
            tools: HashMap::new(),
        }
    }

    /// Wraps a provider event as a contract run event, choosing its scope.
    fn event(&self, body: ProviderEvent) -> RunEvent {
        RunEvent::new(
            self.thread_id.clone(),
            self.project_id.clone(),
            self.run_id.clone(),
            now_ms(),
            body,
        )
    }

    /// The provider thread id as an owned string for a body field.
    fn ptid(&self) -> String {
        self.provider_thread_id.clone()
    }

    /// A terminal event carrying loom's own verdict alongside the contract
    /// status, so the server does not have to infer one from the other.
    fn terminal(&self, status: TurnStatus, error: Option<String>) -> RunEvent {
        let outcome = match status {
            TurnStatus::Completed => RunOutcome::Completed,
            TurnStatus::Interrupted => RunOutcome::Cancelled,
            TurnStatus::Failed => RunOutcome::Failed,
        };
        RunEvent::terminal(
            self.thread_id.clone(),
            self.project_id.clone(),
            self.run_id.clone(),
            now_ms(),
            outcome,
            ProviderEvent::TurnCompleted {
                provider_thread_id: Some(self.ptid()),
                status,
                error: error.map(|message| TurnError { message }),
                provider_checkpoint_id: None,
            },
        )
    }

    /// Translates one Pi frame into zero or more contract events.
    ///
    /// Every Pi frame loom models has its own arm; a frame that is a pure
    /// streaming boundary with no state yields nothing. A frame Pi emits that
    /// loom reaches no arm for is reported **explicitly on stderr** and
    /// produces no event: this bridge does not synthesize a catch-all
    /// `provider/unhandled` row. That is deliberate — a silent fallback hides
    /// a missing mapping, and the contract's diagnostic type is not an excuse
    /// to stop modelling. See `docs/event-model.md`.
    pub fn observe(&mut self, frame: &Value, at_ms: u64) -> Vec<RunEvent> {
        let kind = match frame.get("type").and_then(Value::as_str) {
            Some(kind) => kind,
            None => return Vec::new(),
        };
        let mut events = Vec::new();
        match kind {
            "agent_start" => {
                if !self.identified {
                    self.identified = true;
                    events.push(RunEvent::new(
                        self.thread_id.clone(),
                        self.project_id.clone(),
                        self.run_id.clone(),
                        at_ms,
                        ProviderEvent::ThreadIdentity {
                            provider_thread_id: self.ptid(),
                        },
                    ));
                }
                events.push(self.event(ProviderEvent::TurnStarted {
                    provider_thread_id: self.ptid(),
                    parent_tool_call_id: None,
                }));
            }
            // Pi ends a *low-level* run here; retries, compaction retries and
            // queued continuations may still follow, so it is not the terminal
            // event. loom reports token usage and any provider error from it.
            "agent_end" => {
                events.extend(self.observe_agent_end(frame));
            }
            // The session-level settle: nothing continues automatically, so
            // this is exactly the point a turn is over.
            "agent_settled" => {
                let mut events = self.flush_assistant();
                events.push(self.terminal(TurnStatus::Completed, None));
                return events;
            }
            "message_update" => {
                events.extend(self.observe_message_update(frame));
            }
            "tool_execution_start" => {
                events.extend(self.observe_tool_start(frame));
            }
            "tool_execution_update" => {
                events.extend(self.observe_tool_update(frame));
            }
            "tool_execution_end" => {
                events.extend(self.observe_tool_end(frame));
            }
            "compaction_start" => {
                events.push(self.event(ProviderEvent::ItemStarted {
                    item: ThreadEventItem::ContextCompaction {
                        id: "compaction".into(),
                        presentation: None,
                        parent_tool_call_id: None,
                    },
                    provider_thread_id: self.ptid(),
                }));
            }
            "compaction_end" => {
                let aborted = frame
                    .get("aborted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let error = frame
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                events.push(self.event(ProviderEvent::ItemCompleted {
                    item: ThreadEventItem::ContextCompaction {
                        id: "compaction".into(),
                        presentation: None,
                        parent_tool_call_id: None,
                    },
                    provider_thread_id: self.ptid(),
                }));
                if !aborted && error.is_none() {
                    events.push(self.event(ProviderEvent::ThreadCompacted {
                        provider_thread_id: self.ptid(),
                    }));
                } else {
                    events.push(self.event(ProviderEvent::ProviderError {
                        provider_thread_id: self.ptid(),
                        message: if aborted {
                            "context compaction interrupted".into()
                        } else {
                            "context compaction failed".into()
                        },
                        detail: error,
                        error_info: None,
                        will_retry: None,
                    }));
                }
            }
            "auto_retry_start" => {
                events.push(
                    self.event(ProviderEvent::ProviderError {
                        provider_thread_id: self.ptid(),
                        message: "provider retrying after a transient error".into(),
                        detail: frame
                            .get("errorMessage")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        error_info: None,
                        will_retry: Some(true),
                    }),
                );
            }
            "auto_retry_end" => {
                if frame.get("success").and_then(Value::as_bool) == Some(false) {
                    events.push(
                        self.event(ProviderEvent::ProviderError {
                            provider_thread_id: self.ptid(),
                            message: "provider retries exhausted".into(),
                            detail: frame
                                .get("finalError")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            error_info: None,
                            will_retry: Some(false),
                        }),
                    );
                }
            }
            "extension_error" => {
                events.push(
                    self.event(ProviderEvent::ProviderWarning {
                        provider_thread_id: self.ptid(),
                        category: ProviderWarningCategory::General,
                        summary: Some("provider extension error".into()),
                        details: frame
                            .get("error")
                            .map(content_text)
                            .filter(|text| !text.is_empty()),
                    }),
                );
            }
            // Only a blocking dialog is worth surfacing, and it is one this
            // bridge declines. Fire-and-forget status/title/widget
            // notifications are noise and produce no event.
            "extension_ui_request" => {
                let Some(method) = frame.get("method").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if !matches!(method, "select" | "confirm" | "input" | "editor") {
                    return Vec::new();
                }
                events.push(self.event(ProviderEvent::ProviderWarning {
                    provider_thread_id: self.ptid(),
                    category: ProviderWarningCategory::General,
                    summary: Some(format!(
                        "provider requested `{method}`; declined because no interactive client is attached"
                    )),
                    details: None,
                }));
            }
            // A rejected prompt is the one response that is terminal: failures
            // after acceptance arrive as normal events, not as a second
            // response.
            "response" => {
                if frame.get("success").and_then(Value::as_bool) != Some(false) {
                    return Vec::new();
                }
                if frame.get("command").and_then(Value::as_str) != Some("prompt") {
                    return Vec::new();
                }
                events.push(
                    self.event(ProviderEvent::TurnCompleted {
                        provider_thread_id: Some(self.ptid()),
                        status: TurnStatus::Failed,
                        error: Some(TurnError {
                            message: frame
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("provider rejected the prompt")
                                .to_owned(),
                        }),
                        provider_checkpoint_id: None,
                    }),
                );
            }
            // A streaming boundary with no state change: `message_start`,
            // `message_end` and `turn_start`/`turn_end` are implied by the
            // deltas and the terminal event, so they are not re-emitted as a
            // second, differently-shaped fact.
            "message_start" | "message_end" | "turn_start" | "turn_end" | "queue_update" => {}
            // A frame Pi emits that loom does not model. Reported explicitly,
            // never turned into a fallback event: a missing mapping must be
            // visible rather than papered over with a diagnostic row.
            other => {
                eprintln!(
                    "[provider:{}] unmapped frame `{other}`: {}",
                    self.provider, frame
                );
            }
        }
        events
    }

    /// `agent_end`: close the assistant message, report usage and any error.
    fn observe_agent_end(&mut self, frame: &Value) -> Vec<RunEvent> {
        let mut events = self.flush_assistant();
        let messages = frame.get("messages").and_then(Value::as_array);
        let last_assistant = messages.and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        });
        if let Some(assistant) = last_assistant {
            if let Some(usage) = assistant_usage(assistant) {
                events.push(self.event(ProviderEvent::ThreadTokenUsageUpdated {
                    provider_thread_id: self.ptid(),
                    token_usage: usage,
                }));
            }
            let is_error = assistant.get("stopReason").and_then(Value::as_str) == Some("error");
            if is_error {
                events.push(
                    self.event(ProviderEvent::ProviderError {
                        provider_thread_id: self.ptid(),
                        message: "provider error".into(),
                        detail: assistant
                            .get("errorMessage")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        error_info: None,
                        will_retry: frame.get("willRetry").and_then(Value::as_bool),
                    }),
                );
            }
        }
        events
    }

    /// `message_update`: streaming text, thinking and (unhandled) tool calls.
    fn observe_message_update(&mut self, frame: &Value) -> Vec<RunEvent> {
        let Some(delta) = frame.get("assistantMessageEvent") else {
            return Vec::new();
        };
        let Some(delta_kind) = delta.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        let content_index = delta.get("contentIndex").and_then(Value::as_u64);
        match delta_kind {
            "text_delta" => {
                let Some(text) = delta.get("delta").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if text.is_empty() {
                    return Vec::new();
                }
                let id = self.ensure_assistant_item();
                vec![self.event(ProviderEvent::ItemAgentMessageDelta {
                    item_id: id,
                    delta: text.to_owned(),
                    provider_thread_id: self.ptid(),
                    parent_tool_call_id: None,
                })]
            }
            "text_end" => {
                let Some(text) = delta.get("content") else {
                    return Vec::new();
                };
                let text = content_text(text);
                let id = self.ensure_assistant_item();
                self.assistant_id = None;
                vec![self.event(ProviderEvent::ItemCompleted {
                    item: ThreadEventItem::AgentMessage {
                        id,
                        text,
                        presentation: None,
                        parent_tool_call_id: None,
                    },
                    provider_thread_id: self.ptid(),
                })]
            }
            "thinking_delta" => {
                let (Some(text), Some(index)) =
                    (delta.get("delta").and_then(Value::as_str), content_index)
                else {
                    return Vec::new();
                };
                if text.is_empty() {
                    return Vec::new();
                }
                let id = self.ensure_thinking_item(index);
                vec![self.event(ProviderEvent::ItemReasoningTextDelta {
                    item_id: id,
                    delta: text.to_owned(),
                    provider_thread_id: self.ptid(),
                    parent_tool_call_id: None,
                })]
            }
            "thinking_end" => {
                let (Some(content), Some(index)) = (delta.get("content"), content_index) else {
                    return Vec::new();
                };
                let text = content_text(content);
                let id = self
                    .thinking_ids
                    .remove(&index)
                    .unwrap_or_else(|| format!("thinking-{index}"));
                vec![self.event(ProviderEvent::ItemCompleted {
                    item: ThreadEventItem::Reasoning {
                        id,
                        summary: Vec::new(),
                        content: if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![text]
                        },
                        presentation: None,
                        parent_tool_call_id: None,
                    },
                    provider_thread_id: self.ptid(),
                })]
            }
            // Tool call streaming (toolcall_start/delta/end) is fully described
            // by the `tool_execution_*` events; re-emitting it would duplicate
            // the same fact in a second shape.
            "toolcall_start" | "toolcall_delta" | "toolcall_end" | "text_start" => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn observe_tool_start(&mut self, frame: &Value) -> Vec<RunEvent> {
        let Some(tool_call_id) = frame.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let tool_name = frame
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let args = frame.get("args").cloned().unwrap_or(Value::Null);
        let item = classify_pi_tool(tool_call_id, &tool_name, &args, self.cwd.as_deref());
        self.tools.insert(tool_call_id.to_owned(), item.clone());
        vec![self.event(ProviderEvent::ItemStarted {
            item,
            provider_thread_id: self.ptid(),
        })]
    }

    fn observe_tool_update(&mut self, frame: &Value) -> Vec<RunEvent> {
        let Some(tool_call_id) = frame.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let tool_name = frame
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let partial = frame.get("partialResult").cloned().unwrap_or(Value::Null);
        let text = extract_result_text(&partial);
        if is_command_tool(tool_name) {
            // Pi's `partialResult` is the accumulated output, not a delta, so
            // the contract's `reset` tells the projection to replace rather
            // than append.
            if text.is_empty() {
                return Vec::new();
            }
            return vec![self.event(ProviderEvent::ItemCommandExecutionOutputDelta {
                item_id: tool_call_id.to_owned(),
                delta: text,
                provider_thread_id: self.ptid(),
                reset: Some(true),
                parent_tool_call_id: None,
            })];
        }
        vec![self.event(ProviderEvent::ItemToolCallProgress {
            item_id: tool_call_id.to_owned(),
            message: Some(if text.is_empty() {
                format!("{tool_name} progress update")
            } else {
                text
            }),
            provider_thread_id: self.ptid(),
            parent_tool_call_id: None,
        })]
    }

    fn observe_tool_end(&mut self, frame: &Value) -> Vec<RunEvent> {
        let Some(tool_call_id) = frame.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let tool_name = frame
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let is_error = frame
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status = if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        let result = frame.get("result").cloned().unwrap_or(Value::Null);
        let text = extract_result_text(&result);
        let started = self.tools.remove(tool_call_id).unwrap_or_else(|| {
            classify_pi_tool(tool_call_id, &tool_name, &Value::Null, self.cwd.as_deref())
        });
        let item = close_pi_tool(started, status, &text, &result, is_error);
        vec![self.event(ProviderEvent::ItemCompleted {
            item,
            provider_thread_id: self.ptid(),
        })]
    }

    /// Emits the buffered assistant message as a completion, if one is open.
    fn flush_assistant(&mut self) -> Vec<RunEvent> {
        let Some(id) = self.assistant_id.take() else {
            return Vec::new();
        };
        vec![self.event(ProviderEvent::ItemCompleted {
            item: ThreadEventItem::AgentMessage {
                id,
                text: String::new(),
                presentation: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: self.ptid(),
        })]
    }

    fn ensure_assistant_item(&mut self) -> String {
        if let Some(id) = &self.assistant_id {
            return id.clone();
        }
        self.assistant_seq += 1;
        let id = format!("assistant-{}", self.assistant_seq);
        self.assistant_id = Some(id.clone());
        id
    }

    fn ensure_thinking_item(&mut self, index: u64) -> String {
        self.thinking_ids
            .entry(index)
            .or_insert_with(|| format!("thinking-{index}"))
            .clone()
    }
}

/// Whether a Pi tool is rendered as a command execution.
fn is_command_tool(tool_name: &str) -> bool {
    tool_name == "bash"
}

/// Whether a Pi tool is rendered as a file change.
fn is_file_change_tool(tool_name: &str) -> bool {
    matches!(tool_name, "edit" | "write")
}

/// Shapes a Pi tool call into the contract item that best describes it.
///
/// The classification mirrors bb's Pi adapter: a shell command is a
/// `commandExecution`, a file edit is a `fileChange`, a read is a `fileRead`,
/// and anything else is a generic `toolCall`. That is a semantic choice the
/// projection depends on — a `bash` call rendered as a generic tool would lose
/// its command line and output rendering.
fn classify_pi_tool(
    tool_call_id: &str,
    tool_name: &str,
    args: &Value,
    session_cwd: Option<&str>,
) -> ThreadEventItem {
    if is_command_tool(tool_name) {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| session_cwd.map(str::to_owned))
            .unwrap_or_default();
        return ThreadEventItem::CommandExecution {
            id: tool_call_id.to_owned(),
            command,
            cwd,
            status: ItemStatus::Pending,
            approval_status: None,
            aggregated_output: None,
            exit_code: None,
            duration_ms: None,
            presentation: None,
            parent_tool_call_id: None,
        };
    }
    if is_file_change_tool(tool_name) {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // Whether the edit created the file (`add`) or modified it (`update`)
        // is the one thing the contract's `fileChange` derives from the
        // arguments; the before/after text itself is not part of the event.
        let created = args.get("oldText").is_none() && args.get("oldStr").is_none();
        let changes = if path.is_empty() {
            Vec::new()
        } else {
            vec![FileChange {
                path,
                kind: if created {
                    FileChangeKind::Add
                } else {
                    FileChangeKind::Update
                },
                move_path: None,
                diff: None,
            }]
        };
        return ThreadEventItem::FileChange {
            id: tool_call_id.to_owned(),
            changes,
            status: ItemStatus::Pending,
            approval_status: None,
            presentation: None,
            parent_tool_call_id: None,
        };
    }
    if tool_name == "read" {
        return ThreadEventItem::FileRead {
            id: tool_call_id.to_owned(),
            path: args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            cmd: None,
            status: ItemStatus::Pending,
            presentation: None,
            parent_tool_call_id: None,
        };
    }
    if matches!(tool_name, "grep" | "find" | "ls") {
        let mode = match tool_name {
            "grep" => SearchMode::Content,
            "find" => SearchMode::Path,
            _ => SearchMode::List,
        };
        return ThreadEventItem::Search {
            id: tool_call_id.to_owned(),
            mode,
            query: args
                .get("pattern")
                .or_else(|| args.get("query"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            path: args.get("path").and_then(Value::as_str).map(str::to_owned),
            cmd: None,
            status: ItemStatus::Pending,
            presentation: None,
            parent_tool_call_id: None,
        };
    }
    ThreadEventItem::ToolCall {
        id: tool_call_id.to_owned(),
        server: None,
        tool: tool_name.to_owned(),
        arguments: args.as_object().map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        }),
        status: ItemStatus::Pending,
        result: None,
        error: None,
        duration_ms: None,
        presentation: None,
        parent_tool_call_id: None,
    }
}

/// Applies a tool result to the item its start event established.
fn close_pi_tool(
    started: ThreadEventItem,
    status: ItemStatus,
    text: &str,
    result: &Value,
    is_error: bool,
) -> ThreadEventItem {
    match started {
        ThreadEventItem::CommandExecution {
            id,
            command,
            cwd,
            approval_status,
            presentation,
            parent_tool_call_id,
            ..
        } => ThreadEventItem::CommandExecution {
            id,
            command,
            cwd,
            status,
            approval_status,
            aggregated_output: (!text.is_empty()).then(|| text.to_owned()),
            exit_code: Some(if is_error { 1 } else { 0 }),
            duration_ms: None,
            presentation,
            parent_tool_call_id,
        },
        ThreadEventItem::FileChange {
            id,
            changes,
            approval_status,
            presentation,
            parent_tool_call_id,
            ..
        } => ThreadEventItem::FileChange {
            id,
            changes,
            status,
            approval_status,
            presentation,
            parent_tool_call_id,
        },
        ThreadEventItem::FileRead {
            id,
            path,
            cmd,
            presentation,
            parent_tool_call_id,
            ..
        } => ThreadEventItem::FileRead {
            id,
            path,
            cmd,
            status,
            presentation,
            parent_tool_call_id,
        },
        ThreadEventItem::Search {
            id,
            mode,
            query,
            path,
            cmd,
            presentation,
            parent_tool_call_id,
            ..
        } => ThreadEventItem::Search {
            id,
            mode,
            query,
            path,
            cmd,
            status,
            presentation,
            parent_tool_call_id,
        },
        ThreadEventItem::ToolCall {
            id,
            server,
            tool,
            arguments,
            duration_ms,
            presentation,
            parent_tool_call_id,
            ..
        } => ThreadEventItem::ToolCall {
            id,
            server,
            tool,
            arguments,
            status,
            result: (!result.is_null()).then(|| result.clone()),
            error: is_error.then(|| text.to_owned()),
            duration_ms,
            presentation,
            parent_tool_call_id,
        },
        // A shape whose result the contract does not type (or one that was
        // never opened as a tool) is returned unchanged, so the projection
        // still sees the start rather than a wrong result.
        other => other,
    }
}

/// Reads Pi's `usage` block into the contract's token breakdown.
fn assistant_usage(assistant: &Value) -> Option<loom_domain::ThreadTokenUsage> {
    let usage = assistant.get("usage")?;
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input = number("input");
    let output = number("output");
    let cache_read = number("cacheRead");
    let cache_write = number("cacheWrite");
    let total = match usage.get("totalTokens").and_then(Value::as_u64) {
        Some(total) if total > 0 => total,
        _ => input + output + cache_read + cache_write,
    };
    let breakdown = loom_domain::TokenUsageBreakdown {
        total_tokens: total,
        input_tokens: input,
        cached_input_tokens: cache_read + cache_write,
        output_tokens: output,
        reasoning_output_tokens: 0,
    };
    Some(loom_domain::ThreadTokenUsage {
        total: breakdown,
        last: breakdown,
        model_context_window: None,
    })
}

/// Flattens a Pi content value into text.
fn extract_result_text(value: &Value) -> String {
    content_text(value)
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(map) => {
            if let Some(content) = map.get("content") {
                return content_text(content);
            }
            map.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        }
        _ => String::new(),
    }
}

/// The provider event types the bridge currently produces from Pi frames.
///
/// Exposed so a test can assert the bridge's coverage against the contract
/// rather than trusting a prose list. `docs/event-model.md` records, for every
/// remaining contract type, whether a server produces it or nothing does.
pub const BRIDGE_PROVIDER_TYPES: [ProviderEventType; 13] = [
    ProviderEventType::ThreadIdentity,
    ProviderEventType::TurnStarted,
    ProviderEventType::TurnCompleted,
    ProviderEventType::ItemStarted,
    ProviderEventType::ItemCompleted,
    ProviderEventType::ItemAgentMessageDelta,
    ProviderEventType::ItemCommandExecutionOutputDelta,
    ProviderEventType::ItemReasoningTextDelta,
    ProviderEventType::ItemToolCallProgress,
    ProviderEventType::ThreadCompacted,
    ProviderEventType::ThreadTokenUsageUpdated,
    ProviderEventType::ProviderError,
    ProviderEventType::ProviderWarning,
];

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::{ProjectId, ThreadId};

    fn new_bridge() -> PiBridge {
        let run = ProviderRun {
            spec: ProviderSpec::custom("/bin/true", Vec::new()),
            prompt: "hi".into(),
            host_id: loom_domain::HostId::mint(),
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            run_id: loom_domain::RunId::mint(),
            timeout: Duration::from_secs(1),
            session_dir: None,
        };
        PiBridge::new(&run)
    }

    fn kinds(events: &[RunEvent]) -> Vec<&'static str> {
        events.iter().map(RunEvent::kind).collect()
    }

    #[test]
    fn streaming_deltas_become_distinct_contract_events() {
        let mut bridge = new_bridge();
        let text = bridge.observe(
            &json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": "hello" }
            }),
            1,
        );
        assert_eq!(kinds(&text), vec!["item/agentMessage/delta"]);
        assert_eq!(
            serde_json::to_value(&text[0]).unwrap()["event"]["delta"],
            "hello"
        );

        // Thinking text is a *different* contract type, so the projection can
        // render the two channels apart.
        let thinking = bridge.observe(
            &json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "thinking_delta", "contentIndex": 0, "delta": "hmm" }
            }),
            2,
        );
        assert_eq!(kinds(&thinking), vec!["item/reasoning/textDelta"]);
        let value = serde_json::to_value(&thinking[0]).unwrap();
        assert_eq!(value["event"]["delta"], "hmm");
        // The reasoning item is a distinct item from the assistant message.
        assert_eq!(value["event"]["itemId"], "thinking-0");
    }

    #[test]
    fn a_tool_lifecycle_stays_one_item_across_start_and_end() {
        let mut bridge = new_bridge();
        let start = bridge.observe(
            &json!({
                "type": "tool_execution_start",
                "toolCallId": "c1",
                "toolName": "bash",
                "args": { "command": "ls" }
            }),
            1,
        );
        assert_eq!(kinds(&start), vec!["item/started"]);
        let start_value = serde_json::to_value(&start[0]).unwrap();
        assert_eq!(start_value["event"]["item"]["type"], "commandExecution");
        assert_eq!(start_value["event"]["item"]["id"], "c1");
        assert_eq!(start_value["event"]["item"]["command"], "ls");

        let end = bridge.observe(
            &json!({
                "type": "tool_execution_end",
                "toolCallId": "c1",
                "toolName": "bash",
                "result": { "content": [{ "type": "text", "text": "a" }] },
                "isError": false
            }),
            2,
        );
        assert_eq!(kinds(&end), vec!["item/completed"]);
        let end_value = serde_json::to_value(&end[0]).unwrap();
        assert_eq!(end_value["event"]["item"]["status"], "completed");
        assert_eq!(end_value["event"]["item"]["aggregatedOutput"], "a");
        assert_eq!(end_value["event"]["item"]["exitCode"], 0);
    }

    #[test]
    fn a_file_edit_is_a_file_change_not_a_generic_tool() {
        let mut bridge = new_bridge();
        let start = bridge.observe(
            &json!({
                "type": "tool_execution_start",
                "toolCallId": "c2",
                "toolName": "edit",
                "args": { "path": "/srv/a.rs", "oldText": "x", "newText": "y" }
            }),
            1,
        );
        let value = serde_json::to_value(&start[0]).unwrap();
        assert_eq!(value["event"]["item"]["type"], "fileChange");
        assert_eq!(value["event"]["item"]["changes"][0]["kind"], "update");
    }

    #[test]
    fn settling_is_the_only_completion_and_closes_the_assistant_item() {
        let mut bridge = new_bridge();
        bridge.observe(
            &json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": "hi" }
            }),
            1,
        );
        let events = bridge.observe(&json!({"type": "agent_settled"}), 2);
        assert_eq!(kinds(&events), vec!["item/completed", "turn/completed"]);
        assert!(events.last().unwrap().is_terminal());
    }

    #[test]
    fn a_rejected_prompt_is_terminal_with_a_failed_status() {
        let mut bridge = new_bridge();
        let events = bridge.observe(
            &json!({
                "type": "response",
                "command": "prompt",
                "success": false,
                "error": "no model configured"
            }),
            1,
        );
        assert_eq!(kinds(&events), vec!["turn/completed"]);
        let value = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(value["event"]["status"], "failed");
        assert_eq!(value["event"]["error"]["message"], "no model configured");
        // A successful response is just an ack.
        assert!(bridge
            .observe(
                &json!({"type":"response","command":"prompt","success":true}),
                1
            )
            .is_empty());
    }

    #[test]
    fn an_unmodelled_frame_produces_no_fallback_event() {
        // The issue forbids a `provider/unhandled`-style catch-all: an unmapped
        // frame must be an explicit, visible non-event, not a synthesized row.
        let mut bridge = new_bridge();
        let events = bridge.observe(
            &json!({ "type": "bash_execution_update", "id": "req-1", "delta": "x" }),
            1,
        );
        assert!(events.is_empty(), "no fallback event may be produced");
    }

    #[test]
    fn a_dialog_is_declined_not_hung() {
        let frame = json!({
            "type": "extension_ui_request",
            "id": "u1",
            "method": "confirm",
            "title": "Allow?",
        });
        let response: Value = serde_json::from_str(&auto_cancel_response(&frame).unwrap()).unwrap();
        assert_eq!(response["type"], "extension_ui_response");
        assert_eq!(response["id"], "u1");
        assert_eq!(response["cancelled"], true);

        // Fire-and-forget notifications are not answered.
        assert!(auto_cancel_response(&json!({
            "type": "extension_ui_request",
            "id": "u2",
            "method": "notify",
            "message": "hi"
        }))
        .is_none());

        // A blocking dialog produces a provider warning; a fire-and-forget
        // status update produces nothing at all.
        let mut bridge = new_bridge();
        let events = bridge.observe(&frame, 1);
        assert_eq!(kinds(&events), vec!["provider/warning"]);

        let mut bridge = new_bridge();
        assert!(bridge
            .observe(
                &json!({
                    "type": "extension_ui_request",
                    "id": "u3",
                    "method": "setStatus",
                    "statusKey": "cost"
                }),
                1
            )
            .is_empty());
    }

    #[test]
    fn every_bridge_event_validates_against_the_contract() {
        // The strongest form of the wire check: drive the bridge with a varied
        // frame sequence and validate every produced frame against
        // `contracts/bb/thread-event.json`. A renamed field or discriminant
        // fails here, not in the UI.
        let contract = loom_contract::Contract::load();
        let mut bridge = new_bridge();
        let frames = [
            json!({ "type": "agent_start" }),
            json!({ "type": "turn_start" }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "thinking_delta", "contentIndex": 0, "delta": "hmm" }
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "thinking_end", "contentIndex": 0, "content": "hmm" }
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "contentIndex": 1, "delta": "hello " }
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "contentIndex": 1, "delta": "world" }
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "c1",
                "toolName": "bash",
                "args": { "command": "ls" }
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": "c1",
                "toolName": "bash",
                "partialResult": { "content": [{ "type": "text", "text": "a" }] }
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "c1",
                "toolName": "bash",
                "result": { "content": [{ "type": "text", "text": "a\nb" }] },
                "isError": false
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "c2",
                "toolName": "read",
                "args": { "path": "/srv/a.rs" }
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": "c2",
                "toolName": "read",
                "partialResult": { "content": [{ "type": "text", "text": "reading" }] }
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "c2",
                "toolName": "read",
                "result": { "content": [{ "type": "text", "text": "x" }] },
                "isError": false
            }),
            json!({ "type": "compaction_start", "reason": "threshold" }),
            json!({ "type": "compaction_end", "reason": "threshold", "aborted": false }),
            json!({ "type": "auto_retry_start", "attempt": 1, "maxAttempts": 3 }),
            json!({ "type": "extension_ui_request", "id": "u1", "method": "confirm", "title": "?" }),
            json!({
                "type": "agent_end",
                "willRetry": false,
                "messages": [{
                    "role": "assistant",
                    "stopReason": "stop",
                    "usage": { "input": 5, "output": 1 }
                }]
            }),
            json!({ "type": "agent_settled" }),
        ];

        // Both directions, so the declared coverage list cannot drift from what
        // the bridge really emits: an undeclared emission fails, and a declared
        // type the sequence never produced fails too.
        let mut seen: Vec<String> = Vec::new();
        let mut saw_terminal = false;
        for (index, frame) in frames.iter().enumerate() {
            for event in bridge.observe(frame, index as u64 + 1) {
                saw_terminal |= event.is_terminal();
                let value = serde_json::to_value(&event).unwrap();
                let contract_event = &value["event"];
                let event_type = contract_event["type"].as_str().unwrap();
                seen.push(event_type.to_owned());
                let violations = contract.validate_thread_event(contract_event);
                assert!(
                    violations.is_empty(),
                    "`{event_type}` is not a valid ThreadEvent: {violations:?}\n{contract_event}"
                );
            }
        }
        assert!(saw_terminal, "the sequence must end in a terminal event");
        for event_type in BRIDGE_PROVIDER_TYPES {
            assert!(
                seen.iter().any(|kind| kind == event_type.as_str()),
                "`{}` is declared as produced but the sequence never emitted it",
                event_type.as_str()
            );
        }
        for kind in &seen {
            assert!(
                BRIDGE_PROVIDER_TYPES.iter().any(|t| t.as_str() == kind),
                "`{kind}` was emitted but is not declared in BRIDGE_PROVIDER_TYPES"
            );
        }
    }

    #[test]
    fn every_produced_type_is_a_real_contract_event() {
        // The bridge's declared coverage must be a subset of the contract's
        // provider union: a type that is not real cannot be produced.
        for event_type in BRIDGE_PROVIDER_TYPES {
            let token = event_type.as_str();
            assert_eq!(
                ProviderEventType::ALL
                    .iter()
                    .filter(|t| t.as_str() == token)
                    .count(),
                1,
                "{token} must be a contract provider type"
            );
        }
    }

    #[test]
    fn pi_gains_a_stable_session_with_a_session_dir() {
        let thread_id = ThreadId::mint();
        let dir = Path::new("/var/lib/loom/sessions");
        let argv = effective_argv(&ProviderSpec::pi(), &thread_id, Some(dir));
        assert!(!argv.iter().any(|arg| arg == "--no-session"));
        let index = argv.iter().position(|arg| arg == "--session-dir").unwrap();
        assert_eq!(argv[index + 1], "/var/lib/loom/sessions");
        let index = argv.iter().position(|arg| arg == "--session-id").unwrap();
        assert_eq!(argv[index + 1], thread_id.to_string());

        // Without a session dir the dispatched args are used verbatim.
        assert_eq!(
            effective_argv(&ProviderSpec::pi(), &thread_id, None),
            ProviderSpec::pi().argv()
        );

        // A non-Pi provider is never rewritten.
        let custom = ProviderSpec::custom("/bin/stub", vec!["--x".into()]);
        assert_eq!(
            effective_argv(&custom, &thread_id, Some(dir)),
            custom.argv()
        );
    }

    #[test]
    fn an_agent_end_reports_usage_and_closes_the_message() {
        let mut bridge = new_bridge();
        bridge.observe(
            &json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": "hi" }
            }),
            1,
        );
        let events = bridge.observe(
            &json!({
                "type": "agent_end",
                "willRetry": false,
                "messages": [{
                    "role": "assistant",
                    "stopReason": "stop",
                    "usage": { "input": 10, "output": 2, "cacheRead": 1, "cacheWrite": 1, "totalTokens": 14 }
                }]
            }),
            2,
        );
        assert_eq!(
            kinds(&events),
            vec!["item/completed", "thread/tokenUsage/updated"]
        );
        let usage = serde_json::to_value(&events[1]).unwrap();
        assert_eq!(usage["event"]["tokenUsage"]["last"]["totalTokens"], 14);
    }
}

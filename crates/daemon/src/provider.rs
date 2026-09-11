//! The provider bridge: spawn a provider CLI, speak its protocol, normalise its
//! output into [`RunEvent`]s.
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
//! 2. **Exactly one terminal event.** The run loop always ends by reporting
//!    either `completed` (the provider settled), `failed` (non-zero exit,
//!    protocol rejection, or an exit with no settle) or `timed_out` (the
//!    deadline passed and the process was killed). A `None` here is what would
//!    let a thread stay `working`; there is deliberately no path that returns
//!    without one.
//!
//! [`guard_stdout_line`]: loom_provider_protocol::guard_stdout_line
//! [`RunEvent`]: loom_domain::RunEvent

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use loom_domain::{NoticeLevel, OutputStream, RunEvent, RunOutcome, TurnPhase};
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
    /// The run's identity.
    pub run_id: loom_domain::RunId,
    /// The thread being advanced.
    pub thread_id: loom_domain::ThreadId,
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
            run_id: dispatch.run_id.clone(),
            thread_id: dispatch.thread_id.clone(),
            timeout,
            session_dir,
        }
    }

    fn report(&self, event: RunEvent) -> ProviderReport {
        ProviderReport {
            host_id: self.host_id.clone(),
            run_id: self.run_id.clone(),
            thread_id: self.thread_id.clone(),
            event,
        }
    }
}

/// Spawns a provider and forwards its reports.
///
/// The task always emits exactly one [`RunEvent::Finished`]; any failure that
/// happens before the provider settles is reported as `failed` from here.
pub fn spawn(
    run: ProviderRun,
    reports: mpsc::Sender<ProviderReport>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(message) = drive(&run, &reports).await {
            let _ = reports
                .send(run.report(RunEvent::Finished {
                    outcome: RunOutcome::Failed,
                    error: Some(message),
                }))
                .await;
        }
    })
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

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = match tokio::time::timeout(remaining, lines.next_line()).await {
            // The deadline passed with no terminal event: kill and report.
            Err(_elapsed) => {
                terminate(&mut child).await;
                let _ = reports
                    .send(run.report(RunEvent::Finished {
                        outcome: RunOutcome::TimedOut,
                        error: Some(format!(
                            "provider did not settle within {}ms",
                            run.timeout.as_millis()
                        )),
                    }))
                    .await;
                return Ok(());
            }
            Ok(Ok(Some(line))) => line,
            // EOF: the process closed stdout. Fall through to the exit check.
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                terminate(&mut child).await;
                let _ = reports
                    .send(run.report(RunEvent::Finished {
                        outcome: RunOutcome::Failed,
                        error: Some(format!("provider stdout failed: {error}")),
                    }))
                    .await;
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

        if let Some(event) = map_pi_frame(&frame, &run.spec.name) {
            let terminal = event.is_terminal();
            // A closed channel means the daemon is gone; stop reading.
            if reports.send(run.report(event)).await.is_err() {
                terminate(&mut child).await;
                return Ok(());
            }
            if terminal {
                terminate(&mut child).await;
                return Ok(());
            }
        }
    }

    // The stream ended without a terminal frame. Interpret the exit status.
    let status = child
        .wait()
        .await
        .map_err(|error| format!("could not wait for the provider: {error}"))?;
    let error = if status.success() {
        "provider exited without settling".to_owned()
    } else {
        format!("provider exited with {status}")
    };
    let _ = reports
        .send(run.report(RunEvent::Finished {
            outcome: RunOutcome::Failed,
            error: Some(error),
        }))
        .await;
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

/// Maps one Pi RPC frame onto at most one run event.
///
/// The mapping is deliberately partial: a frame loom does not model produces
/// `None` rather than a synthetic event. `agent_settled` is Pi's "no retry,
/// compaction or queued continuation remains", which is exactly the point at
/// which a run is over.
pub fn map_pi_frame(frame: &Value, provider: &str) -> Option<RunEvent> {
    let kind = frame.get("type").and_then(Value::as_str)?;
    match kind {
        "agent_start" => Some(RunEvent::Started {
            provider: provider.to_owned(),
        }),
        "turn_start" => Some(RunEvent::Turn {
            phase: TurnPhase::Began,
        }),
        "turn_end" => Some(RunEvent::Turn {
            phase: TurnPhase::Ended,
        }),
        "message_update" => {
            let delta = frame.get("assistantMessageEvent")?;
            match delta.get("type").and_then(Value::as_str)? {
                "text_delta" => Some(RunEvent::Output {
                    stream: OutputStream::Assistant,
                    text: delta.get("delta").and_then(Value::as_str)?.to_owned(),
                }),
                "thinking_delta" => Some(RunEvent::Output {
                    stream: OutputStream::Thinking,
                    text: delta.get("delta").and_then(Value::as_str)?.to_owned(),
                }),
                _ => None,
            }
        }
        "tool_execution_start" => Some(RunEvent::ToolCall {
            tool_call_id: frame.get("toolCallId").and_then(Value::as_str)?.to_owned(),
            name: frame.get("toolName").and_then(Value::as_str)?.to_owned(),
            args: frame.get("args").cloned().unwrap_or(Value::Null),
        }),
        "tool_execution_end" => Some(RunEvent::ToolResult {
            tool_call_id: frame.get("toolCallId").and_then(Value::as_str)?.to_owned(),
            name: frame.get("toolName").and_then(Value::as_str)?.to_owned(),
            ok: !frame
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            output: frame
                .get("result")
                .map(|result| content_text(result.get("content").unwrap_or(result)))
                .unwrap_or_default(),
        }),
        "agent_settled" => Some(RunEvent::Finished {
            outcome: RunOutcome::Completed,
            error: None,
        }),
        // A rejected prompt is the one response that is terminal: failures
        // after acceptance arrive as normal events, not as a second response.
        "response" => {
            if frame.get("success").and_then(Value::as_bool) != Some(false) {
                return None;
            }
            if frame.get("command").and_then(Value::as_str) != Some("prompt") {
                return None;
            }
            Some(RunEvent::Finished {
                outcome: RunOutcome::Failed,
                error: Some(
                    frame
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("provider rejected the prompt")
                        .to_owned(),
                ),
            })
        }
        "extension_error" => Some(RunEvent::Notice {
            level: NoticeLevel::Warning,
            message: format!(
                "extension error: {}",
                frame.get("error").map(content_text).unwrap_or_default()
            ),
        }),
        // Only a blocking dialog is worth surfacing: it is one this bridge
        // declines. Fire-and-forget status/title/widget notifications are
        // noise and produce no event.
        "extension_ui_request" => {
            let method = frame.get("method").and_then(Value::as_str)?;
            if !matches!(method, "select" | "confirm" | "input" | "editor") {
                return None;
            }
            Some(RunEvent::Notice {
                level: NoticeLevel::Warning,
                message: format!(
                    "provider requested `{method}`; declined because no interactive client is attached"
                ),
            })
        }
        "auto_retry_start" => Some(RunEvent::Notice {
            level: NoticeLevel::Info,
            message: format!(
                "provider retrying (attempt {})",
                frame.get("attempt").and_then(Value::as_u64).unwrap_or(0)
            ),
        }),
        "auto_retry_end" => {
            if frame.get("success").and_then(Value::as_bool) == Some(false) {
                Some(RunEvent::Notice {
                    level: NoticeLevel::Error,
                    message: frame
                        .get("finalError")
                        .and_then(Value::as_str)
                        .unwrap_or("provider retries exhausted")
                        .to_owned(),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Flattens a Pi content value into text.
fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::ThreadId;

    #[test]
    fn pi_streaming_deltas_become_output_events() {
        let frame = json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "text_delta", "delta": "hello" }
        });
        assert_eq!(
            map_pi_frame(&frame, "pi"),
            Some(RunEvent::Output {
                stream: OutputStream::Assistant,
                text: "hello".into(),
            })
        );

        let thinking = json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "thinking_delta", "delta": "hmm" }
        });
        assert_eq!(
            map_pi_frame(&thinking, "pi"),
            Some(RunEvent::Output {
                stream: OutputStream::Thinking,
                text: "hmm".into(),
            })
        );

        // A delta shape loom does not model is dropped, not invented.
        assert_eq!(
            map_pi_frame(
                &json!({"type":"message_update","assistantMessageEvent":{"type":"text_end"}}),
                "pi"
            ),
            None
        );
    }

    #[test]
    fn tool_lifecycle_maps_to_call_and_result() {
        let start = json!({
            "type": "tool_execution_start",
            "toolCallId": "c1",
            "toolName": "bash",
            "args": {"command": "ls"}
        });
        assert_eq!(
            map_pi_frame(&start, "pi"),
            Some(RunEvent::ToolCall {
                tool_call_id: "c1".into(),
                name: "bash".into(),
                args: json!({"command": "ls"}),
            })
        );

        let end = json!({
            "type": "tool_execution_end",
            "toolCallId": "c1",
            "toolName": "bash",
            "result": {"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
            "isError": false
        });
        assert_eq!(
            map_pi_frame(&end, "pi"),
            Some(RunEvent::ToolResult {
                tool_call_id: "c1".into(),
                name: "bash".into(),
                ok: true,
                output: "ab".into(),
            })
        );
    }

    #[test]
    fn settling_is_the_only_completion() {
        assert_eq!(
            map_pi_frame(&json!({"type": "agent_settled"}), "pi"),
            Some(RunEvent::Finished {
                outcome: RunOutcome::Completed,
                error: None,
            })
        );
        // agent_end is not terminal: a retry or compaction may follow.
        assert_eq!(map_pi_frame(&json!({"type": "agent_end"}), "pi"), None);
    }

    #[test]
    fn a_rejected_prompt_is_terminal() {
        let frame = json!({
            "type": "response",
            "command": "prompt",
            "success": false,
            "error": "no model configured"
        });
        assert_eq!(
            map_pi_frame(&frame, "pi"),
            Some(RunEvent::Finished {
                outcome: RunOutcome::Failed,
                error: Some("no model configured".into()),
            })
        );
        // A successful response is just an ack.
        assert_eq!(
            map_pi_frame(
                &json!({"type":"response","command":"prompt","success":true}),
                "pi"
            ),
            None
        );
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

        // A blocking dialog produces a visible notice; a fire-and-forget status
        // update produces nothing at all.
        assert_eq!(
            map_pi_frame(&frame, "pi"),
            Some(RunEvent::Notice {
                level: NoticeLevel::Warning,
                message: "provider requested `confirm`; declined because no interactive client is attached"
                    .into(),
            })
        );
        assert_eq!(
            map_pi_frame(
                &json!({
                    "type": "extension_ui_request",
                    "id": "u3",
                    "method": "setStatus",
                    "statusKey": "cost"
                }),
                "pi"
            ),
            None
        );
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
}

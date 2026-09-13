//! Driving an ACP agent for one run.
//!
//! This is the transport half of the adapter: it initializes ACP v1, opens or
//! restores a session, sends one prompt, and feeds every update through
//! [`AcpTranslator`](super::AcpTranslator) on the way to the run's report
//! channel.
//!
//! Two kinds of peer are supported and loom treats them identically:
//!
//! - **a spawned native ACP agent**, connected over stdio;
//! - **an embedded `pi-acp`**, connected over an in-process channel, so Pi needs
//!   no adapter process of its own.
//!
//! The client code does not branch on which it is: both are a
//! [`ConnectTo`](agent_client_protocol::ConnectTo) handed to the same builder.
//! Only the transport differs.
//!
//! ## The terminal-event invariant
//!
//! A run reports exactly one terminal event. This module owns that guarantee on
//! the ACP path: every way the session can end — an agent that refuses, a
//! connection that drops, a deadline, or the ordinary stop reason — funnels
//! through one place that emits it. Without that, a dropped connection would
//! leave the thread in `working` forever.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{on_receive_notification, on_receive_request, Client, ConnectionTo};
use loom_domain::{ProviderEvent, RunEvent};
use loom_provider_protocol::ProviderReport;
use tokio::sync::mpsc;

use super::{AcpTranslator, RunContext};
use crate::provider::ProviderRun;

/// How an ACP agent is reached.
pub enum Transport {
    /// A child process speaking ACP over stdio.
    Stdio {
        /// The executable.
        command: String,
        /// Its arguments.
        args: Vec<String>,
    },
    /// `pi-acp` linked into this process, reached over an in-process channel.
    ///
    /// The adapter is not a process, but `pi` still is, so this carries the
    /// command that reaches it — the same field the stdio ACP path uses, and
    /// the same one an operator override replaces.
    EmbeddedPi {
        /// The `pi` executable the in-process adapter spawns.
        command: String,
        /// Its arguments.
        args: Vec<String>,
    },
}

/// Drives one ACP session to completion, reporting as it goes.
///
/// Returns `Err` only for a failure *before* a terminal event was sent; once
/// one was sent this returns `Ok(())`, so the caller's fallback cannot
/// double-report. The task wrapper below turns any returned failure into the
/// same terminal event.
pub async fn drive(
    run: &ProviderRun,
    transport: Transport,
    reports: &mpsc::Sender<ProviderReport>,
) -> Result<(), String> {
    let cwd = run.spec.cwd.clone().ok_or_else(|| {
        "an ACP session requires a working directory, and the dispatch has none".to_string()
    })?;
    // The control plane names the workspace; the daemon is the only party that
    // can see this machine's filesystem, so it validates the directory here.
    // A missing directory is a hard error, never a silent start in the
    // daemon's own cwd — an agent editing the wrong project is the bug this
    // check prevents, and ACP would otherwise happily create a session there.
    if !std::path::Path::new(&cwd).is_dir() {
        return Err(format!(
            "the dispatched working directory {cwd:?} does not exist on this host"
        ));
    }

    let sink = UpdateSink {
        run: run.clone(),
        state: Arc::new(tokio::sync::Mutex::new(UpdateState {
            translator: AcpTranslator::new(RunContext {
                thread_id: run.thread_id.clone(),
                cwd: Some(cwd.clone()),
                provider_session_id: run.provider_session_id.clone(),
            }),
            phase: UpdatePhase::Constructing,
            pending: Vec::new(),
            pending_load_usage: None,
        })),
        reports: reports.clone(),
        report_lock: Arc::new(tokio::sync::Mutex::new(())),
        terminal_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let operation = async {
        match transport {
            Transport::Stdio { command, args } => {
                let agent =
                    agent_client_protocol::AcpAgent::from_args(agent_argv(&command, &args, &cwd))
                        .map_err(|e| format!("could not describe the ACP agent: {e}"))?;
                serve(agent, &sink, &cwd).await
            }
            Transport::EmbeddedPi { command, args } => {
                serve_embedded(&sink, &cwd, command, args).await
            }
        }
    };
    let outcome = match tokio::time::timeout(run.timeout, operation).await {
        Ok(outcome) => outcome,
        Err(_) => {
            sink.terminal_timeout(format!(
                "ACP agent did not settle within {}ms",
                run.timeout.as_millis()
            ))
            .await?;
            return Ok(());
        }
    };

    match outcome {
        Ok(()) => Ok(()),
        Err(message) => {
            // A failure that did not reach a terminal still has to end the run.
            sink.terminal_failure(message.clone()).await?;
            Ok(())
        }
    }
}

/// The argv for a spawned agent, including the ACP session workspace.
///
/// `agent-client-protocol::AcpAgentConfig` intentionally models only a command,
/// args and environment; it has no working-directory field. On Unix a small
/// `sh -c` launcher supplies the missing process boundary without changing the
/// agent's argv or touching the daemon's global current directory.
fn agent_argv(command: &str, args: &[String], cwd: &str) -> Vec<String> {
    #[cfg(unix)]
    {
        let command = if std::path::Path::new(command).is_absolute() || !command.contains('/') {
            command.to_owned()
        } else {
            std::env::current_dir()
                .map(|dir| dir.join(command).to_string_lossy().into_owned())
                .unwrap_or_else(|_| command.to_owned())
        };
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"cd -- "$1" && shift && exec "$@""#.to_string(),
            "loom-acp-agent".to_string(),
            cwd.to_string(),
            command,
        ];
        argv.extend(args.iter().cloned());
        argv
    }
    #[cfg(not(unix))]
    {
        let _ = cwd;
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(command.to_string());
        argv.extend(args.iter().cloned());
        argv
    }
}

/// Runs the client loop against a connected agent.
/// `connect_with` takes the *counterpart* role: a client connects to something
/// that is a `ConnectTo<Client>`, which is what `AcpAgent` implements.
async fn serve(
    agent: impl agent_client_protocol::ConnectTo<Client> + 'static,
    sink: &UpdateSink,
    cwd: &str,
) -> Result<(), String> {
    let notifications = sink.clone();
    let permissions = sink.clone();
    let conversation = sink.clone();
    let cwd = cwd.to_string();

    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                notifications.on_notification(notification).await;
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let response = permissions.on_permission_request(request).await;
                let _ = responder.respond(response);
                Ok(())
            },
            on_receive_request!(),
        )
        .connect_with(
            agent,
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                conversation.converse(&connection, &cwd).await
            },
        )
        .await
        .map_err(|e| format!("the ACP connection ended: {e}"))
}

/// Aborts an embedded adapter if the surrounding run times out or is
/// cancelled. Dropping a bare `JoinHandle` would detach `pi-acp` and leave its
/// child alive after the daemon has declared the run finished.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs the client loop against `pi-acp` linked into this process.
///
/// The adapter is not a child process: `pi-acp`'s `AcpAgent` runs on a task in
/// this daemon, and the two halves are joined by an in-process channel pair
/// that the SDK provides for exactly this. Only `pi` itself is a child.
///
/// The client code above is shared with the spawned path — an embedded library
/// and a spawned agent differ only in which transport is handed to the builder
/// — which is the property that makes one provider path cover every agent.
async fn serve_embedded(
    sink: &UpdateSink,
    cwd: &str,
    command: String,
    args: Vec<String>,
) -> Result<(), String> {
    use agent_client_protocol::Channel;

    // `pi-acp` takes a *program*, not a command line: its resolver decides
    // between a path, a PATH lookup and a Windows batch wrapper, and appends
    // nothing. So a dispatch that names arguments cannot be honoured on this
    // path, and saying so is better than dropping them silently — an operator
    // who set `LOOM_PROVIDER_ARGS` would otherwise see them ignored.
    if !args.is_empty() {
        return Err(format!(
            "the embedded pi-acp transport takes no provider arguments, but the dispatch \
             supplies {args:?}; use the acp_stdio launch kind to pass a command line"
        ));
    }
    let mut config = pi_acp::config::Config::default();
    // The adapter spawns `pi` itself, so the command travels through rather
    // than being loom's business. Leaving the default would look for a bare
    // `pi` on PATH, which is right when nothing overrides it.
    if !command.is_empty() {
        config.pi_command = command;
    }

    let agent = Arc::new(pi_acp::agent::AcpAgent::new(config));

    // `duplex` returns two connected endpoints. The adapter takes one and runs
    // until it ends; loom's client takes the other.
    let (adapter_side, client_side) = Channel::duplex();
    // The adapter's exit is deliberately not reported from its own task: the
    // client side observes the closed channel and reports the failure, so there
    // is one reporting path rather than two that could race.
    let mut running = AbortOnDrop(tokio::spawn(async move {
        let _ = agent.run_with(adapter_side).await;
    }));

    let outcome = serve(client_side, sink, cwd).await;
    // `serve` consumed the client side, which ends the adapter's loop, and
    // awaiting lets the adapter run `AcpAgent::run_with`'s own `dispose_all`.
    //
    // This is graceful rather than necessary: `pi-acp` gives `PiProcess` a
    // `Drop` that signals the child's process group, so aborting here would not
    // orphan `pi`. Waiting is still preferable because it is the *disposing*
    // path the adapter documents for itself, and it costs nothing when the
    // adapter is already unwinding — the channel closed, so it will finish.
    // Bounded so a wedged adapter cannot hold the run open.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut running.0).await;
    outcome
}

/// The construction/load phase controls which agent notifications can be
/// translated. It is kept with the translator so checking the phase and
/// enqueueing a notification is one atomic operation.
enum UpdatePhase {
    /// `session/new` is in flight and notifications wait for its returned id.
    Constructing,
    /// `session/load` is in flight; replay is not part of the current run.
    Loading { session_id: String },
    /// The load response arrived, but its post-response history replay must
    /// still be suppressed until the new prompt begins.
    Loaded { session_id: String },
    /// The session is named and notifications belong to the active run.
    Ready,
}

struct PendingUpdate {
    session_id: String,
    update: SessionUpdate,
}

struct UpdateState {
    translator: AcpTranslator,
    phase: UpdatePhase,
    pending: Vec<PendingUpdate>,
    /// v1 `session/load` may publish a context usage snapshot while loading.
    /// Keep the latest one; history and metadata updates are intentionally not
    /// replayed into the new run.
    pending_load_usage: Option<SessionUpdate>,
}

/// What is shared between the client callbacks and the conversation driver.
#[derive(Clone)]
struct UpdateSink {
    run: ProviderRun,
    state: Arc<tokio::sync::Mutex<UpdateState>>,
    reports: mpsc::Sender<ProviderReport>,
    /// Preserves report order across the notification callback and the
    /// conversation future. In particular, identity must be sent before any
    /// update released from construction.
    report_lock: Arc<tokio::sync::Mutex<()>>,
    /// Whether the terminal event has been sent, so it is sent exactly once
    /// even when a failure races an ordinary completion.
    terminal_sent: Arc<std::sync::atomic::AtomicBool>,
}

impl UpdateSink {
    /// One ACP session update: translate, then report.
    ///
    /// ACP notifications can arrive while `session/new` or `session/load` is
    /// answering. The phase and session-id checks happen while holding the
    /// same lock as the pending queue, so an update cannot fall into the gap
    /// between "not identified" and "identity released".
    async fn on_notification(&self, notification: SessionNotification) {
        let session_id = notification.session_id.0.to_string();
        let events = {
            let mut state = self.state.lock().await;
            match &state.phase {
                UpdatePhase::Constructing => {
                    state.pending.push(PendingUpdate {
                        session_id,
                        update: notification.update,
                    });
                    return;
                }
                UpdatePhase::Loading {
                    session_id: expected,
                }
                | UpdatePhase::Loaded {
                    session_id: expected,
                } => {
                    if expected == &session_id
                        && matches!(notification.update, SessionUpdate::UsageUpdate(_))
                    {
                        state.pending_load_usage = Some(notification.update);
                    }
                    return;
                }
                UpdatePhase::Ready => {}
            }
            if state.translator.provider_session_id() != Some(session_id.as_str()) {
                return;
            }
            state.translator.on_session_update(&notification.update)
        };
        self.report_all(events).await;
    }

    /// Marks a v1 load request as in flight.
    async fn begin_load(&self, session_id: String) {
        let mut state = self.state.lock().await;
        state.phase = UpdatePhase::Loading { session_id };
        state.pending_load_usage = None;
    }

    /// Finishes a load response. The phase remains `Loaded` until the new
    /// prompt is sent, because pi-acp emits history after the load response is
    /// queued.
    async fn finish_load(&self, session_id: &str) {
        let mut state = self.state.lock().await;
        let loaded = matches!(
            &state.phase,
            UpdatePhase::Loading { session_id: expected } if expected == session_id
        );
        if loaded {
            state.phase = UpdatePhase::Loaded {
                session_id: session_id.to_owned(),
            };
        }
    }

    /// Allows notifications from the new prompt after load-time replay has
    /// been suppressed. A usage update that arrived after the load response is
    /// still returned for reporting before the prompt is sent.
    async fn ready_for_prompt(&self) -> Option<SessionUpdate> {
        let mut state = self.state.lock().await;
        let usage = state.pending_load_usage.take();
        state.phase = UpdatePhase::Ready;
        usage
    }

    /// Names the agent's session and releases anything held for it.
    ///
    /// Called once the session exists, before the prompt is sent. The identity
    /// event is where the control plane learns the id, so it goes first and
    /// everything held follows in arrival order.
    async fn on_session_known(&self, session_id: &str) {
        let _report_guard = self.report_lock.lock().await;
        let (identity, buffered, load_usage) = {
            let mut state = self.state.lock().await;
            state.translator.set_provider_session_id(session_id);
            let buffered = std::mem::take(&mut state.pending)
                .into_iter()
                .filter(|pending| pending.session_id == session_id)
                .collect::<Vec<_>>();
            let was_constructing = matches!(state.phase, UpdatePhase::Constructing);
            if was_constructing {
                state.phase = UpdatePhase::Ready;
            }
            let identity = state.translator.on_prompt_sent();
            let load_usage = state.pending_load_usage.take();
            (identity, buffered, load_usage)
        };

        // Identity first: it is the event that tells the control plane which
        // conversation this thread is, and it must precede facts about it.
        self.report_all_locked(identity, None).await;
        for pending in buffered {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_session_update(&pending.update)
            };
            self.report_all_locked(events, None).await;
        }
        if let Some(update) = load_usage {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_session_update(&update)
            };
            self.report_all_locked(events, None).await;
        }
    }

    /// An agent is asking permission. Answer it, and record the fact.
    ///
    /// The interaction is not yet projected to a client, so this answers with
    /// the first option that allows the action, and declines when there is
    /// none. That is a placeholder for a policy decision, not a policy: the
    /// point is that the agent is never left blocked.
    async fn on_permission_request(
        &self,
        request: RequestPermissionRequest,
    ) -> RequestPermissionResponse {
        use agent_client_protocol::schema::v1::{
            PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
        };
        let choice = request
            .options
            .iter()
            .find(|option| {
                matches!(
                    option.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                )
            })
            .or_else(|| request.options.first());

        match choice {
            Some(option) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            )),
            // No options at all: the agent asked something unanswerable, and
            // cancelling is the only truthful reply.
            None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
        }
    }

    /// Initialize, open a session, send the prompt, and wait for the turn.
    async fn converse(
        &self,
        connection: &ConnectionTo<agent_client_protocol::Agent>,
        cwd: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let initialized = connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        if initialized.protocol_version != ProtocolVersion::V1 {
            return Err(agent_client_protocol::Error::internal_error().data(
                "the ACP agent negotiated an unsupported protocol version; loom currently supports v1",
            ));
        }
        if self.run.provider_session_id.is_some() && !initialized.agent_capabilities.load_session {
            return Err(agent_client_protocol::Error::internal_error().data(
                "the ACP agent does not advertise session/load support for this resumed run",
            ));
        }

        // Resume the thread's existing conversation when the dispatch carried
        // one, and start a new session otherwise. This is what makes a second
        // turn continue the first: the id came from the previous run's
        // `thread/identity` event, was stored with the thread, and travelled
        // back on the dispatch.
        //
        // `session/load` rather than `session/resume`, because this protocol
        // version's `resume` is not implemented by the adapter loom embeds
        // (pi-acp handles `load`). `load` replays history, which loom does not
        // need, but it accepts the same id and restores the same conversation,
        // so the redundant replay is cheaper than a missing capability.
        let session_id = match &self.run.provider_session_id {
            Some(existing) => {
                self.begin_load(existing.clone()).await;
                connection
                    .send_request(LoadSessionRequest::new(existing.clone(), cwd))
                    .block_task()
                    .await?;
                self.finish_load(existing).await;
                self.on_session_known(existing).await;
                existing.clone()
            }
            None => {
                let created = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let session_id = created.session_id.0.to_string();
                self.on_session_known(&session_id).await;
                session_id
            }
        };

        // For a resumed v1 session, `session/load` may have replayed history
        // after its response. The sink keeps that phase suppressed until this
        // point; only updates caused by the new prompt belong to this run.
        if let Some(update) = self.ready_for_prompt().await {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_session_update(&update)
            };
            self.report_all(events).await;
        }
        let session = SessionId::new(session_id);
        let prompt = PromptRequest::new(
            session,
            vec![ContentBlock::Text(TextContent::new(
                self.run.prompt.clone(),
            ))],
        );
        let response = connection.send_request(prompt).block_task().await?;

        let events = {
            let mut state = self.state.lock().await;
            state.translator.on_stop_reason(response.stop_reason)
        };
        self.report_all(events).await;
        Ok(())
    }

    /// Ends the run with a failure, unless it already ended.
    async fn terminal_failure(&self, message: String) -> Result<(), String> {
        let events = {
            let mut state = self.state.lock().await;
            state.translator.on_failure(message)
        };
        self.report_all(events).await;
        Ok(())
    }

    /// Ends the run with a timeout, unless it already ended.
    async fn terminal_timeout(&self, message: String) -> Result<(), String> {
        let events = {
            let mut state = self.state.lock().await;
            state.translator.on_failure(message)
        };
        self.report_all_with_outcome(events, Some(loom_domain::RunOutcome::TimedOut))
            .await;
        Ok(())
    }

    /// Reports translated bodies, stopping once a terminal event has gone out.
    async fn report_all(&self, events: Vec<ProviderEvent>) {
        self.report_all_with_outcome(events, None).await;
    }

    async fn report_all_with_outcome(
        &self,
        events: Vec<ProviderEvent>,
        forced_outcome: Option<loom_domain::RunOutcome>,
    ) {
        let _report_guard = self.report_lock.lock().await;
        self.report_all_locked(events, forced_outcome).await;
    }

    /// Reports while the caller holds the ordering lock.
    async fn report_all_locked(
        &self,
        events: Vec<ProviderEvent>,
        forced_outcome: Option<loom_domain::RunOutcome>,
    ) {
        use std::sync::atomic::Ordering;
        for body in events {
            if self.terminal_sent.load(Ordering::SeqCst) {
                // A terminal event already ended this run; anything after it
                // would be reported against a finished run.
                return;
            }
            let terminal = body.is_terminal();
            let event = RunEvent::new(
                self.run.thread_id.clone(),
                self.run.project_id.clone(),
                self.run.run_id.clone(),
                loom_relay::now_ms(),
                body,
            );
            let event = if terminal {
                self.terminal_sent.store(true, Ordering::SeqCst);
                let outcome = forced_outcome.unwrap_or_else(|| match event.terminal_status() {
                    Some(loom_domain::TurnStatus::Completed) => loom_domain::RunOutcome::Completed,
                    Some(loom_domain::TurnStatus::Interrupted) => {
                        loom_domain::RunOutcome::Cancelled
                    }
                    _ => loom_domain::RunOutcome::Failed,
                });
                let mut event = event;
                event.outcome = Some(outcome);
                event
            } else {
                event
            };
            let _ = self
                .reports
                .send(ProviderReport {
                    host_id: self.run.host_id.clone(),
                    event,
                })
                .await;
        }
    }
}

/// Runs one ACP turn on a task, reporting a failure that never reached a
/// terminal event.
///
/// a dispatch cannot be left without a verdict. This is the only provider task
/// wrapper; the former direct-Pi driver no longer exists.
pub fn spawn(
    run: ProviderRun,
    transport: Transport,
    reports: mpsc::Sender<ProviderReport>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(message) = drive(&run, transport, &reports).await {
            let _ = reports
                .send(ProviderReport {
                    host_id: run.host_id.clone(),
                    event: crate::provider::terminal_event(
                        &run,
                        loom_domain::RunOutcome::Failed,
                        &message,
                    ),
                })
                .await;
        }
    })
}

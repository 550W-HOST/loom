//! Driving an ACP agent for one run.
//!
//! This is the transport half of the adapter: it negotiates a protocol version,
//! opens or restores a session, sends one prompt, and feeds every update through
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
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, TextContent,
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
    /// command that reaches it — the same field the JSON-RPC path uses, and
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
/// double-report. That mirrors [`crate::provider::spawn`].
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

    let translator = Arc::new(tokio::sync::Mutex::new(AcpTranslator::new(RunContext {
        thread_id: run.thread_id.clone(),
        cwd: Some(cwd.clone()),
    })));

    // Every update goes through one funnel, so the mapping is exercised the
    // same way regardless of which protocol version negotiated.
    let sink = UpdateSink {
        run: run.clone(),
        translator,
        reports: reports.clone(),
        terminal_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let outcome = match transport {
        Transport::Stdio { command, args } => {
            let agent = agent_client_protocol::AcpAgent::from_args(agent_argv(&command, &args))
                .map_err(|e| format!("could not describe the ACP agent: {e}"))?;
            serve(agent, &sink, &cwd).await
        }
        Transport::EmbeddedPi { command, args } => serve_embedded(&sink, &cwd, command, args).await,
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

/// The argv for a spawned agent, which the SDK's config takes as one slice.
fn agent_argv(command: &str, args: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(command.to_string());
    argv.extend(args.iter().cloned());
    argv
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
    let running = tokio::spawn(async move {
        let _ = agent.run_with(adapter_side).await;
    });

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
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), running).await;
    outcome
}

/// What is shared between the client callbacks and the conversation driver.
#[derive(Clone)]
struct UpdateSink {
    run: ProviderRun,
    translator: Arc<tokio::sync::Mutex<AcpTranslator>>,
    reports: mpsc::Sender<ProviderReport>,
    /// Whether the terminal event has been sent, so it is sent exactly once
    /// even when a failure races an ordinary completion.
    terminal_sent: Arc<std::sync::atomic::AtomicBool>,
}

impl UpdateSink {
    /// One ACP session update: translate, then report.
    async fn on_notification(&self, notification: SessionNotification) {
        let events = {
            let mut translator = self.translator.lock().await;
            translator.on_session_update(&notification.update)
        };
        self.report_all(events).await;
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
        let negotiated = initialized.protocol_version;

        // Identity and the turn must open *before* the session request, because
        // an agent may emit updates while answering `session/new` (pi-acp
        // publishes session info and a usage snapshot there). Emitting the
        // lifecycle events afterwards puts them after facts they precede, and a
        // consumer reading the log in order sees an update for a thread it has
        // not been told about.
        {
            let mut translator = self.translator.lock().await;
            self.report_all(translator.on_prompt_sent()).await;
        }

        let session = connection
            .send_request(NewSessionRequest::new(cwd))
            .block_task()
            .await?;

        let prompt = PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(
                self.run.prompt.clone(),
            ))],
        );
        let response = connection.send_request(prompt).block_task().await?;

        // v1 reports completion on this response; v2 reports it through a
        // `state_update` notification instead, so this response carries no
        // stop reason there and the terminal already went out.
        //
        // Compared numerically rather than against `ProtocolVersion::V2`,
        // because that constant only exists with the v2 feature enabled and
        // this check must behave correctly either way.
        if negotiated.as_u16() < 2 {
            let events = {
                let mut translator = self.translator.lock().await;
                translator.on_stop_reason(response.stop_reason)
            };
            self.report_all(events).await;
        }
        Ok(())
    }

    /// Ends the run with a failure, unless it already ended.
    async fn terminal_failure(&self, message: String) -> Result<(), String> {
        let events = {
            let mut translator = self.translator.lock().await;
            translator.on_failure(message)
        };
        self.report_all(events).await;
        Ok(())
    }

    /// Reports translated bodies, stopping once a terminal event has gone out.
    async fn report_all(&self, events: Vec<ProviderEvent>) {
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
                let outcome = match event.terminal_status() {
                    Some(loom_domain::TurnStatus::Completed) => loom_domain::RunOutcome::Completed,
                    Some(loom_domain::TurnStatus::Interrupted) => {
                        loom_domain::RunOutcome::Cancelled
                    }
                    _ => loom_domain::RunOutcome::Failed,
                };
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
/// Mirrors [`crate::provider::spawn`]: the task always ends the run, so a
/// dispatch cannot be left without a verdict.
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

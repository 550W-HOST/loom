//! Driving an ACP agent for one run.
//!
//! This is the transport half of the adapter: it negotiates ACP v2 first and
//! falls back to v1, opens or restores a session, sends one prompt, and feeds
//! every update through [`AcpTranslator`](super::AcpTranslator) on the way to
//! the run's report channel.
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

use std::collections::{HashSet, VecDeque};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PromptRequest, RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::schema::{v1, v2, ProtocolVersion};
use agent_client_protocol::{
    on_receive_notification, on_receive_request, Agent, Client, ConnectTo, ConnectionTo, Error,
};
use loom_domain::{
    ModelFallbackReason, ProviderErrorCategory, ProviderEvent, ProviderWarningCategory,
    ReasoningLevel, RunEvent,
};
use loom_provider_protocol::{
    InteractionRequest, ProviderCatalogReport, ProviderCommandsReport, ProviderReport,
};
use tokio::sync::mpsc;

use super::permission::{PermissionBroker, PermissionRegistry};
use super::{AcpTranslator, RunContext, Translated};
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

/// Whether the ACP boundary should print what it receives.
///
/// `LOOM_ACP_TRACE=1` turns on a line per notification, per translated event and
/// per terminal decision. It exists because a missing frame is invisible
/// otherwise: the worker has no logging framework, and "no terminal event ever
/// arrived" is indistinguishable from "the agent never said the turn was over".
fn acp_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("LOOM_ACP_TRACE").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    })
}

/// A short name for one v2 session update, for the trace line.
#[cfg_attr(not(test), allow(dead_code))]
fn v2_update_name(update: &v2::SessionUpdate) -> &'static str {
    match update {
        v2::SessionUpdate::StateUpdate(state) => match state {
            v2::StateUpdate::Running(_) => "state:running",
            v2::StateUpdate::Idle(_) => "state:idle",
            v2::StateUpdate::RequiresAction(_) => "state:requires-action",
            v2::StateUpdate::Other(_) => "state:other",
            _ => "state:unknown",
        },
        v2::SessionUpdate::AgentMessageChunk(_) => "agent_message_chunk",
        v2::SessionUpdate::AgentMessage(_) => "agent_message",
        v2::SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
        v2::SessionUpdate::AgentThought(_) => "agent_thought",
        v2::SessionUpdate::UserMessageChunk(_) => "user_message_chunk",
        v2::SessionUpdate::UserMessage(_) => "user_message",
        v2::SessionUpdate::ToolCallContentChunk(_) => "tool_call_content_chunk",
        v2::SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
        v2::SessionUpdate::TerminalUpdate(_) => "terminal_update",
        v2::SessionUpdate::TerminalOutputChunk(_) => "terminal_output_chunk",
        v2::SessionUpdate::PlanUpdate(_) => "plan_update",
        v2::SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
        v2::SessionUpdate::UsageUpdate(_) => "usage_update",
        v2::SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
        v2::SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
        _ => "other",
    }
}

/// Prints one ACP trace line when [`acp_trace_enabled`].
macro_rules! acp_trace {
    ($($arg:tt)*) => {
        if crate::acp::session::acp_trace_enabled() {
            eprintln!("loom-worker acp: {}", format!($($arg)*));
        }
    };
}

/// Drives one ACP session to completion, reporting as it goes.
///
/// Returns `Err` only for a failure *before* a terminal event was sent; once
/// one was sent this returns `Ok(())`, so the caller's fallback cannot
/// double-report. The task wrapper below turns any returned failure into the
/// same terminal event.
#[allow(clippy::too_many_arguments)]
pub async fn drive(
    run: &ProviderRun,
    transport: Transport,
    reports: &mpsc::Sender<ProviderReport>,
    catalogs: &mpsc::Sender<ProviderCatalogReport>,
    commands: &mpsc::Sender<ProviderCommandsReport>,
    permissions: PermissionRegistry,
    interactions: mpsc::Sender<InteractionRequest>,
    steers: &crate::steer::SteerRegistry,
) -> Result<(), String> {
    let cwd = run.spec.cwd.clone().ok_or_else(|| {
        "an ACP session requires a working directory, and the dispatch has none".to_string()
    })?;
    // The control plane names the workspace; the worker is the only party that
    // can see this machine's filesystem, so it validates the directory here.
    // A missing directory is a hard error, never a silent start in the
    // worker's own cwd — an agent editing the wrong project is the bug this
    // check prevents, and ACP would otherwise happily create a session there.
    if !std::path::Path::new(&cwd).is_dir() {
        return Err(format!(
            "the dispatched working directory {cwd:?} does not exist on this host"
        ));
    }

    // The run's steer channel is opened before its session is, so a steer that
    // arrives while the session is still being constructed waits for the loop
    // instead of being dropped. It is closed when the run ends, however it
    // ends, which is what makes a later steer a harmless no-op.
    let steer_rx = steers.register(run.run_id.clone()).await;

    // The run's budgets are measured against what it reports; the sink is the
    // only place that sees every event, so the two share this.
    let liveness = Arc::new(RunLiveness::new());

    let sink = UpdateSink {
        run: run.clone(),
        liveness: liveness.clone(),
        state: Arc::new(tokio::sync::Mutex::new(UpdateState {
            translator: AcpTranslator::new(RunContext {
                thread_id: run.thread_id.clone(),
                cwd: Some(cwd.clone()),
                provider_session_id: run.provider_session_id.clone(),
            }),
            phase: UpdatePhase::Constructing,
            pending: Vec::new(),
            pending_load_usage: None,
            pending_load_usage_v2: None,
        })),
        reports: reports.clone(),
        catalogs: catalogs.clone(),
        commands: commands.clone(),
        report_lock: Arc::new(tokio::sync::Mutex::new(())),
        terminal_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        completion_notify: Arc::new(tokio::sync::Notify::new()),
        broker: PermissionBroker::new(
            run.clone(),
            interactions,
            permissions,
            run.permission_timeout,
        ),
        steers: Arc::new(tokio::sync::Mutex::new(Some(steer_rx))),
        steer_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let operation = async {
        match transport {
            Transport::Stdio { command, args } => {
                let argv = agent_argv(&command, &args, &cwd);
                agent_client_protocol::AcpAgent::from_args(argv.clone())
                    .map_err(|e| format!("could not describe the ACP agent: {e}"))?;
                serve(
                    move || {
                        agent_client_protocol::AcpAgent::from_args(argv.clone())
                            .expect("validated ACP agent arguments")
                    },
                    &sink,
                    &cwd,
                )
                .await
            }
            Transport::EmbeddedPi { command, args } => {
                serve_embedded(&sink, &cwd, command, args, run.settle_timeout).await
            }
        }
    };
    // The run is bounded by its own reporting rather than by the clock alone:
    // see `RunLiveness`. `timeout_at` borrows the operation so a moving
    // deadline re-arms the wait instead of restarting the turn.
    tokio::pin!(operation);
    let started = tokio::time::Instant::now();
    let mut expired: Option<RunBudget> = None;
    let outcome = loop {
        let Some((deadline, _)) = liveness.deadline(started, run.timeout, run.ceiling) else {
            // Both bounds were removed (`0`), so nothing but the operation
            // itself ends the run.
            break operation.await;
        };
        match tokio::time::timeout_at(deadline, &mut operation).await {
            Ok(outcome) => break outcome,
            Err(_) => {
                // A deadline can move while it is being waited on: a report
                // re-arms the silence bound, and a finished item re-arms it
                // from the completion. Only a deadline still in the past ends
                // the run.
                if let Some((again, budget)) = liveness.deadline(started, run.timeout, run.ceiling)
                {
                    if again <= tokio::time::Instant::now() {
                        expired = Some(budget);
                        break Ok(());
                    }
                }
            }
        }
    };
    let outcome = match expired {
        Some(budget) => {
            let budget_value = match budget {
                RunBudget::Timeout => run.timeout,
                RunBudget::Ceiling => run.ceiling,
            };
            let result = sink.terminal_timeout(budget.reason(budget_value)).await;
            steers.forget(&run.run_id).await;
            return result;
        }
        None => outcome,
    };
    steers.forget(&run.run_id).await;

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
/// agent's argv or touching the worker's global current directory.
pub(super) fn agent_argv(command: &str, args: &[String], cwd: &str) -> Vec<String> {
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
async fn serve<C, F>(agent_factory: F, sink: &UpdateSink, cwd: &str) -> Result<(), String>
where
    C: agent_client_protocol::ConnectTo<Client>,
    F: FnMut() -> C + Send + 'static,
{
    Client
        .protocol_connector()
        .with_v1({
            let sink = sink.clone();
            let cwd = cwd.to_owned();
            move || V1Client {
                sink: sink.clone(),
                cwd: cwd.clone(),
            }
        })
        .with_v2({
            let sink = sink.clone();
            let cwd = cwd.to_owned();
            move || V2Client {
                sink: sink.clone(),
                cwd: cwd.clone(),
            }
        })
        .connect_to(agent_factory)
        .await
        .map_err(|e| format!("the ACP connection ended: {e}"))
}

/// A typed ACP v1 client component used by the protocol connector.
struct V1Client {
    sink: UpdateSink,
    cwd: String,
}

impl ConnectTo<Agent> for V1Client {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let notifications = self.sink.clone();
        let permissions = self.sink.broker.clone();
        let conversation = self.sink;
        let cwd = self.cwd;

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
                    let response = permissions.ask_v1(request).await;
                    let _ = responder.respond(response);
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                conversation.converse_v1(&connection, &cwd).await
            })
            .await
    }
}

/// A typed ACP v2 client component used by the protocol connector.
struct V2Client {
    sink: UpdateSink,
    cwd: String,
}

impl ConnectTo<Agent> for V2Client {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let notifications = self.sink.clone();
        let permissions = self.sink.broker.clone();
        let conversation = self.sink;
        let cwd = self.cwd;

        Client
            .v2()
            .on_receive_notification(
                async move |notification: v2::UpdateSessionNotification, _cx| {
                    notifications.on_v2_notification(notification).await;
                    Ok(())
                },
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: v2::RequestPermissionRequest, responder, _cx| {
                    let response = permissions.ask_v2(request).await;
                    responder.respond(response)
                },
                on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                conversation.converse_v2(&connection, &cwd).await
            })
            .await
    }
}

/// A join guard for an embedded adapter task.
///
/// The protocol connector owns a transport component until the connection is
/// finished. If the surrounding run is cancelled while that component is
/// being polled, dropping its future must also stop the adapter task rather
/// than detaching it.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The embedded agent transport must keep its adapter task alive after the
/// connector's initialize probe. A bare [`agent_client_protocol::Channel`]
/// reports a ready component future, which would make the protocol connector
/// stop forwarding frames as soon as the probe completed.
pub(super) struct EmbeddedAgentTransport {
    channel: agent_client_protocol::Channel,
    task: AbortOnDrop<Result<(), String>>,
}

impl ConnectTo<Client> for EmbeddedAgentTransport {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let Self { channel, mut task } = self;
        let transport = ConnectTo::<Client>::connect_to(channel, client);
        let (transport_result, task_result) = tokio::join!(transport, &mut task.0);
        transport_result?;

        match task_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(Error::internal_error().data(format!(
                "the embedded pi-acp agent ended with an error: {error}"
            ))),
            Err(error) => Err(Error::internal_error()
                .data(format!("the embedded pi-acp agent task failed: {error}"))),
        }
    }
}

/// The embedded adapter's settle budget, in the whole seconds `pi-acp` takes.
///
/// `0` disables the fallback. Any other sub-second budget rounds *up* to one
/// second rather than truncating to `0`, which would disable it instead of
/// shortening it.
fn settle_timeout_secs(settle_timeout: std::time::Duration) -> u64 {
    if settle_timeout.is_zero() {
        0
    } else {
        settle_timeout.as_millis().div_ceil(1_000) as u64
    }
}

/// Builds a fresh embedded agent connection for each protocol negotiation
/// attempt. The connector may need a new connection when it falls back from
/// v2 to v1, so the adapter and its channel must be created per factory call.
pub(super) fn embedded_agent_factory(
    command: String,
    settle_timeout: std::time::Duration,
) -> impl FnMut() -> EmbeddedAgentTransport + Send + 'static {
    move || {
        let mut config = pi_acp::config::Config::default();
        if !command.is_empty() {
            config.pi_command = command.clone();
        }
        // The adapter's own settle fallback. `Config::default()` does not read
        // `PI_ACP_SETTLE_TIMEOUT_SECS`, so this is where loom decides the value
        // it runs with; see `WorkerConfig::settle_timeout`.
        config.settle_timeout_secs = settle_timeout_secs(settle_timeout);
        let agent = Arc::new(pi_acp::agent::AcpAgent::new(config));
        let (adapter_side, client_side) = agent_client_protocol::Channel::duplex();
        let task = tokio::spawn(async move {
            agent
                .run_with(adapter_side)
                .await
                .map_err(|error| error.to_string())
        });
        EmbeddedAgentTransport {
            channel: client_side,
            task: AbortOnDrop(task),
        }
    }
}

/// Runs the client loop against `pi-acp` linked into this process.
///
/// The adapter is not a child process: `pi-acp`'s `AcpAgent` runs on a task in
/// this worker, and the two halves are joined by an in-process channel pair
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
    settle_timeout: std::time::Duration,
) -> Result<(), String> {
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
    serve(embedded_agent_factory(command, settle_timeout), sink, cwd).await
}

/// The construction/load phase controls which agent notifications can be
/// translated. It is kept with the translator so checking the phase and
/// enqueueing a notification is one atomic operation.
enum UpdatePhase {
    /// `session/new` is in flight and notifications wait for its returned id.
    Constructing,
    /// `session/load` is in flight; replayed timeline items are not part of the
    /// current run. Session metadata is retained for release after identity.
    Loading { session_id: String },
    /// The load response arrived, but replayed timeline items must still be
    /// suppressed until the new prompt begins. Session metadata is retained.
    Loaded { session_id: String },
    /// The session is named and notifications belong to the active run.
    Ready,
}

struct PendingUpdate {
    session_id: String,
    update: PendingUpdateKind,
}

enum PendingUpdateKind {
    V1(SessionUpdate),
    V2(v2::SessionUpdate),
}

struct UpdateState {
    translator: AcpTranslator,
    phase: UpdatePhase,
    pending: Vec<PendingUpdate>,
    /// v1 `session/load` may publish a context usage snapshot while loading.
    /// Keep the latest one. Timeline history is intentionally not replayed into
    /// the new run, while session metadata is retained in `pending`.
    pending_load_usage: Option<SessionUpdate>,
    /// The v2 equivalent of the load-time usage snapshot.
    pending_load_usage_v2: Option<v2::SessionUpdate>,
}

/// Which bound ended a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunBudget {
    /// The run stopped reporting for its silence budget.
    Timeout,
    /// The run spent its total budget, however active it was.
    Ceiling,
}

impl RunBudget {
    /// The message the terminal event carries.
    ///
    /// It names the budget that fired rather than reusing the adapter's own
    /// "did not settle" wording. The two are different timers with different
    /// values, and a client told "did not settle within 1800000ms" while the
    /// adapter's settle fallback is 600000ms cannot tell which one ended the
    /// turn.
    fn reason(self, budget: std::time::Duration) -> String {
        match self {
            RunBudget::Timeout => format!(
                "the agent produced no events for {}ms, so loom ended the run",
                budget.as_millis()
            ),
            RunBudget::Ceiling => {
                format!("the run exceeded its {}ms ceiling", budget.as_millis())
            }
        }
    }
}

/// What a run's budgets are measured against.
///
/// The rule is the one the embedded adapter applies to its own settle fallback:
/// a run that is still reporting is not stuck, and a *call* that started without
/// completing is work rather than silence. A tool call that runs for an hour
/// therefore holds [`ProviderRun::timeout`] off, and only
/// [`ProviderRun::ceiling`] can end it. Without this the worker's wall clock
/// would kill exactly the turns the adapter was fixed to protect — a long
/// build, a long download, a delegated child agent.
#[derive(Debug)]
struct RunLiveness {
    state: std::sync::Mutex<RunLivenessState>,
}

#[derive(Debug)]
struct RunLivenessState {
    /// When the run last reported anything.
    last_event: tokio::time::Instant,
    /// Calls that have started and have not completed, by item id.
    ///
    /// Only work counts — see
    /// [`ThreadEventItem::is_running_call`](loom_domain::ThreadEventItem::is_running_call):
    /// a message has no completion in the contract, so counting one would hold
    /// the silence bound off for the rest of the run.
    ///
    /// A set rather than a count, so a duplicated `item/started` or a
    /// completion for an item the run never opened cannot drift the bound.
    open_items: HashSet<String>,
}

impl RunLiveness {
    /// Starts the clock at construction, which is when the run's driver began.
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(RunLivenessState {
                last_event: tokio::time::Instant::now(),
                open_items: HashSet::new(),
            }),
        }
    }

    /// Records one reported event against the budgets.
    fn observe(&self, event: &ProviderEvent) {
        let mut state = self.state.lock().expect("run liveness mutex");
        state.last_event = tokio::time::Instant::now();
        match event {
            ProviderEvent::ItemStarted { item, .. } if item.is_running_call() => {
                state.open_items.insert(item.id().to_owned());
            }
            ProviderEvent::ItemCompleted { item, .. } => {
                state.open_items.remove(item.id());
            }
            _ => {}
        }
    }

    /// The instant the run must be terminal by, and the budget that decided it.
    ///
    /// `None` means neither bound is in force, which `0` selects for either.
    fn deadline(
        &self,
        started: tokio::time::Instant,
        timeout: std::time::Duration,
        ceiling: std::time::Duration,
    ) -> Option<(tokio::time::Instant, RunBudget)> {
        let state = self.state.lock().expect("run liveness mutex");
        let ceiling_at = (!ceiling.is_zero()).then(|| started + ceiling);
        let idle_at =
            (!timeout.is_zero() && state.open_items.is_empty()).then(|| state.last_event + timeout);
        match (idle_at, ceiling_at) {
            (Some(idle), Some(ceiling)) => Some(if ceiling <= idle {
                (ceiling, RunBudget::Ceiling)
            } else {
                (idle, RunBudget::Timeout)
            }),
            (Some(idle), None) => Some((idle, RunBudget::Timeout)),
            (None, Some(ceiling)) => Some((ceiling, RunBudget::Ceiling)),
            (None, None) => None,
        }
    }
}

/// What is shared between the client callbacks and the conversation driver.
#[derive(Clone)]
struct UpdateSink {
    run: ProviderRun,
    /// What the run's own budgets are measured against. See [`RunLiveness`].
    liveness: Arc<RunLiveness>,
    state: Arc<tokio::sync::Mutex<UpdateState>>,
    reports: mpsc::Sender<ProviderReport>,
    /// Where the catalogue read from this session's config options is reported.
    ///
    /// The catalogue is a fact about the host's agent rather than about this
    /// run, so it travels on its own channel to the socket loop instead of
    /// being folded into a run event.
    catalogs: mpsc::Sender<ProviderCatalogReport>,
    /// Where the commands read from this session's advertisement are reported.
    ///
    /// A command list is a fact about the workspace and the agent rather than
    /// about this run, so it travels on its own channel to the socket loop
    /// instead of being folded into a run event.
    commands: mpsc::Sender<ProviderCommandsReport>,
    /// Preserves report order across the notification callback and the
    /// conversation future. In particular, identity must be sent before any
    /// update released from construction.
    report_lock: Arc<tokio::sync::Mutex<()>>,
    /// Whether the terminal event has been sent, so it is sent exactly once
    /// even when a failure races an ordinary completion.
    terminal_sent: Arc<std::sync::atomic::AtomicBool>,
    /// Wakes the v2 conversation future when `StateUpdate::Idle` is reported.
    completion_notify: Arc<tokio::sync::Notify>,
    /// Where a permission request goes and how its answer gets back.
    ///
    /// The broker applies the run's selected permission policy: it auto-selects
    /// an allowing ACP option for Full Access and routes other requests through
    /// the control plane when they require user approval. See
    /// [`crate::acp::permission`].
    broker: PermissionBroker,
    /// The steers the control plane sent for this run, in arrival order.
    ///
    /// Shared with the run's task rather than owned by it because the sink is
    /// cloned into the ACP client callbacks; the conversation takes the
    /// receiver for its own loop when it starts prompting.
    steers: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<String>>>>,
    /// Whether a steer is replacing the prompt that is in flight.
    ///
    /// A v2 agent reports the end of a turn through `state_update: idle`, and
    /// that notification translates to a terminal event. While a steer is being
    /// applied, the cancelled prompt's idle must not end the run: the
    /// conversation is about to re-prompt on the same session, and *it* owns the
    /// run's one terminal. This gates exactly that — set when a steer is queued,
    /// cleared once nothing is waiting.
    steer_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

/// The ACP config option ids loom prefers for model and reasoning choices.
///
/// ACP leaves the ids to the agent. v2 Pi uses `thought_level`; v1 agents may
/// choose another id and are matched by the option's semantic category.
pub(crate) const MODEL_CONFIG_ID: &str = "model";
pub(crate) const THOUGHT_LEVEL_CONFIG_ID: &str = "thought_level";

/// Ask the agent to set one session config option.
///
/// Returns the option set the agent holds afterwards. The reply is what makes
/// the level list after a model change the *new* model's list rather than the
/// one loom happened to record earlier.
async fn set_config_option(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    config_id: &str,
    value: &str,
) -> Result<Vec<v2::SessionConfigOption>, Error> {
    let request = v2::SetSessionConfigOptionRequest::new(
        session_id.to_owned(),
        v2::SessionConfigId::new(config_id),
        v2::SessionConfigOptionValue::id(value.to_owned()),
    );
    let response = connection.send_request(request).block_task().await?;
    Ok(response.config_options)
}

/// Ask a v1 agent to set one session config option.
///
/// v1's value-id form intentionally serializes without the v2 `type: "id"`
/// discriminator. The option id is supplied by the agent's own response, so
/// this also supports agents such as omp whose thought-level id is `thinking`.
pub(super) async fn set_config_option_v1(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    config_id: &str,
    value: &str,
) -> Result<Vec<v1::SessionConfigOption>, Error> {
    let request = v1::SetSessionConfigOptionRequest::new(
        v1::SessionId::new(session_id.to_owned()),
        v1::SessionConfigId::new(config_id),
        v1::SessionConfigOptionValue::value_id(value.to_owned()),
    );
    let response = connection.send_request(request).block_task().await?;
    Ok(response.config_options)
}

/// The thought-level value that answers `level`, if this model offers one.
///
/// The level is already the provider's own id: the client picked it from the
/// values the server advertised for this model and sent it back verbatim, so
/// there is nothing to translate here. The lookup is still made against the
/// option set the agent returned for the model it now holds, which is what
/// makes the ladder follow the model — a value this model does not offer is
/// left unset rather than approximated, and the agent keeps its own default.
fn thought_level_value(
    options: &[v2::SessionConfigOption],
    level: &ReasoningLevel,
) -> Option<String> {
    let (_, values) = select_state(options, THOUGHT_LEVEL_CONFIG_ID)?;
    values.into_iter().find(|value| value == level.as_str())
}

/// The current value of a select config option, and the values it offers.
///
/// What an agent *did* accept is only worth reporting when it comes from the
/// agent's own reply, which is why the fallback path reads it here rather than
/// from anything loom recorded earlier.
fn select_state(
    options: &[v2::SessionConfigOption],
    config_id: &str,
) -> Option<(String, Vec<String>)> {
    let option = options
        .iter()
        .find(|option| option.config_id.0.as_ref() == config_id)?;
    let v2::SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let candidates: Vec<&v2::SessionConfigSelectOption> = match &select.options {
        v2::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        v2::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => return None,
    };
    Some((
        select.current_value.0.to_string(),
        candidates
            .iter()
            .map(|candidate| candidate.value.0.to_string())
            .collect(),
    ))
}

/// The current value and selectable values of a v1 option, along with the
/// option's actual id. The preferred id is tried first; the category handles
/// agents that choose a provider-specific id such as omp's `thinking`.
fn select_state_v1(
    options: &[v1::SessionConfigOption],
    preferred_id: &str,
    category: v1::SessionConfigOptionCategory,
) -> Option<(String, String, Vec<String>)> {
    let option = options
        .iter()
        .find(|option| {
            option.id.0.as_ref() == preferred_id
                && matches!(&option.kind, v1::SessionConfigKind::Select(_))
        })
        .or_else(|| {
            options.iter().find(|option| {
                option.category.as_ref() == Some(&category)
                    && matches!(&option.kind, v1::SessionConfigKind::Select(_))
            })
        })?;
    let v1::SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let values = match &select.options {
        v1::SessionConfigSelectOptions::Ungrouped(values) => values,
        v1::SessionConfigSelectOptions::Grouped(groups) => {
            return Some((
                option.id.0.to_string(),
                select.current_value.0.to_string(),
                groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .map(|value| value.value.0.to_string())
                    .collect(),
            ));
        }
        _ => return None,
    };
    Some((
        option.id.0.to_string(),
        select.current_value.0.to_string(),
        values
            .iter()
            .map(|value| value.value.0.to_string())
            .collect(),
    ))
}

/// Finds a v1 reasoning value and returns the option id needed to set it.
fn thought_level_value_v1(
    options: &[v1::SessionConfigOption],
    level: &ReasoningLevel,
) -> Option<(String, String)> {
    let (config_id, _, values) = select_state_v1(
        options,
        THOUGHT_LEVEL_CONFIG_ID,
        v1::SessionConfigOptionCategory::ThoughtLevel,
    )?;
    values
        .into_iter()
        .find(|value| value == level.as_str())
        .map(|value| (config_id, value))
}

/// Waits for the next steer for this run.
///
/// Never resolves when the run has no channel — the conversation takes the
/// receiver once, and a closed channel means the worker is shutting the run
/// down — so a `select!` over it waits on the prompt instead of spinning.
async fn next_steer(steers: &mut Option<mpsc::Receiver<String>>) -> String {
    match steers {
        Some(receiver) => match receiver.recv().await {
            Some(text) => text,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Moves every steer that is already waiting into `queued`.
///
/// Called after a prompt settles: a steer that arrived alongside the response
/// belongs to the turn and must be honoured before the turn is allowed to end.
fn drain_steers(steers: &mut Option<mpsc::Receiver<String>>, queued: &mut VecDeque<String>) {
    if let Some(receiver) = steers.as_mut() {
        while let Ok(text) = receiver.try_recv() {
            queued.push_back(text);
        }
    }
}

/// Asks a v1 agent to abandon the prompt in flight.
///
/// A steer is delivered by cancelling the running prompt and re-prompting on
/// the same session, which is how every ACP client — Zed's "send immediately",
/// bb's `steerMode: "queue"` bridge — does it: ACP has no injection method.
async fn cancel_prompt(connection: &ConnectionTo<Agent>, session_id: &str) {
    let _ = connection.send_notification(CancelNotification::new(SessionId::new(session_id)));
}

/// Asks a v2 agent to abandon the prompt in flight. The v2 spelling of
/// [`cancel_prompt`].
async fn cancel_prompt_v2(connection: &ConnectionTo<Agent>, session_id: &str) {
    let _ = connection.send_notification(v2::CancelSessionNotification::new(v2::SessionId::new(
        session_id,
    )));
}

impl UpdateSink {
    /// Takes this run's steer receiver for the conversation loop.
    ///
    /// `None` only when a conversation already took it, which cannot happen for
    /// a run that prompts once.
    async fn take_steers(&self) -> Option<mpsc::Receiver<String>> {
        self.steers.lock().await.take()
    }

    /// One ACP session update: translate, then report.
    ///
    /// ACP notifications can arrive while `session/new` or `session/load` is
    /// answering. The phase and session-id checks happen while holding the
    /// same lock as the pending queue, so an update cannot fall into the gap
    /// between "not identified" and "identity released".
    async fn on_notification(&self, notification: SessionNotification) {
        let session_id = notification.session_id.0.to_string();
        match &notification.update {
            SessionUpdate::ToolCallUpdate(update) => acp_trace!(
                "v1 tool update session={} id={} status={:?} content_blocks={} raw_output={}",
                session_id,
                update.tool_call_id,
                update.fields.status,
                update.fields.content.as_ref().map_or(0, Vec::len),
                update.fields.raw_output.is_some()
            ),
            SessionUpdate::ToolCall(call) => acp_trace!(
                "v1 tool start session={} id={} kind={:?} content_blocks={} raw_output={}",
                session_id,
                call.tool_call_id,
                call.kind,
                call.content.len(),
                call.raw_output.is_some()
            ),
            _ => acp_trace!(
                "v1 update {:?} for session {}",
                std::mem::discriminant(&notification.update),
                session_id
            ),
        }

        let events = {
            let mut state = self.state.lock().await;
            match &state.phase {
                UpdatePhase::Constructing => {
                    state.pending.push(PendingUpdate {
                        session_id,
                        update: PendingUpdateKind::V1(notification.update),
                    });
                    return;
                }
                UpdatePhase::Loading {
                    session_id: expected,
                } if expected == &session_id => {
                    match &notification.update {
                        SessionUpdate::UsageUpdate(_) => {
                            state.pending_load_usage = Some(notification.update);
                        }
                        // Session metadata and the command menu are session state,
                        // not conversation history. OMP sends both around the load
                        // response, so retain them for release after identity instead
                        // of dropping them with replayed messages.
                        SessionUpdate::SessionInfoUpdate(_)
                        | SessionUpdate::AvailableCommandsUpdate(_) => {
                            state.pending.push(PendingUpdate {
                                session_id,
                                update: PendingUpdateKind::V1(notification.update),
                            });
                        }
                        _ => {}
                    }
                    return;
                }
                UpdatePhase::Loaded {
                    session_id: expected,
                } if expected == &session_id => {
                    if matches!(notification.update, SessionUpdate::UsageUpdate(_)) {
                        state.pending_load_usage = Some(notification.update);
                        return;
                    }
                    if !matches!(
                        notification.update,
                        SessionUpdate::SessionInfoUpdate(_)
                            | SessionUpdate::AvailableCommandsUpdate(_)
                    ) {
                        return;
                    }
                    if !state.translator.has_identity() {
                        state.pending.push(PendingUpdate {
                            session_id,
                            update: PendingUpdateKind::V1(notification.update),
                        });
                        return;
                    }
                }
                UpdatePhase::Loading { .. } | UpdatePhase::Loaded { .. } => return,
                UpdatePhase::Ready => {}
            }
            if state.translator.provider_session_id() != Some(session_id.as_str()) {
                return;
            }
            state.translator.on_session_update(&notification.update)
        };
        self.report_commands().await;
        self.report_all(events).await;
    }

    /// The v2 form of one ACP session update. Construction and resume use the
    /// same buffering rules as v1, but the schema is intentionally kept typed
    /// until it reaches the adapter translator.
    async fn on_v2_notification(&self, notification: v2::UpdateSessionNotification) {
        let session_id = notification.session_id.0.to_string();
        acp_trace!(
            "v2 update {} for session {} :: {:?}",
            v2_update_name(&notification.update),
            session_id,
            notification.update
        );
        let events = {
            let mut state = self.state.lock().await;
            match &state.phase {
                UpdatePhase::Constructing => {
                    state.pending.push(PendingUpdate {
                        session_id,
                        update: PendingUpdateKind::V2(notification.update),
                    });
                    return;
                }
                UpdatePhase::Loading {
                    session_id: expected,
                } if expected == &session_id => {
                    match &notification.update {
                        v2::SessionUpdate::UsageUpdate(_) => {
                            state.pending_load_usage_v2 = Some(notification.update);
                        }
                        v2::SessionUpdate::SessionInfoUpdate(_)
                        | v2::SessionUpdate::AvailableCommandsUpdate(_) => {
                            state.pending.push(PendingUpdate {
                                session_id,
                                update: PendingUpdateKind::V2(notification.update),
                            });
                        }
                        _ => {}
                    }
                    return;
                }
                UpdatePhase::Loaded {
                    session_id: expected,
                } if expected == &session_id => {
                    if matches!(notification.update, v2::SessionUpdate::UsageUpdate(_)) {
                        state.pending_load_usage_v2 = Some(notification.update);
                        return;
                    }
                    if !matches!(
                        notification.update,
                        v2::SessionUpdate::SessionInfoUpdate(_)
                            | v2::SessionUpdate::AvailableCommandsUpdate(_)
                    ) {
                        return;
                    }
                    if !state.translator.has_identity() {
                        state.pending.push(PendingUpdate {
                            session_id,
                            update: PendingUpdateKind::V2(notification.update),
                        });
                        return;
                    }
                }
                UpdatePhase::Loading { .. } | UpdatePhase::Loaded { .. } => return,
                UpdatePhase::Ready => {}
            }
            if state.translator.provider_session_id() != Some(session_id.as_str()) {
                return;
            }
            state.translator.on_v2_session_update(&notification.update)
        };
        self.report_commands().await;
        if self
            .steer_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // A steer is replacing the prompt this update ended, so the run is
            // not over: dropping the terminal keeps it open for the re-prompt,
            // and the conversation emits the run's one terminal when nothing is
            // waiting. Anything else in the batch still belongs to the session
            // and is reported.
            let rest = events
                .into_iter()
                .filter(|event| !event.is_terminal())
                .collect();
            self.report_all(rest).await;
            return;
        }
        self.report_all(events).await;
    }

    /// Marks a v1 load request as in flight.
    async fn begin_load(&self, session_id: String) {
        let mut state = self.state.lock().await;
        state.phase = UpdatePhase::Loading { session_id };
        state.pending_load_usage = None;
        state.pending_load_usage_v2 = None;
    }

    /// Finishes a load response. The phase remains `Loaded` until the new
    /// prompt is sent, because post-response timeline replay is suppressed
    /// rather than mixed into the new run. Session metadata is still accepted
    /// during this phase, since agents such as OMP send the bootstrap title
    /// after the load response.
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

    /// Releases a v2 load-time usage snapshot after replay suppression ends.
    async fn ready_for_prompt_v2(&self) -> Option<v2::SessionUpdate> {
        let mut state = self.state.lock().await;
        let usage = state.pending_load_usage_v2.take();
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
        let (identity, buffered, load_usage, load_usage_v2) = {
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
            let load_usage_v2 = state.pending_load_usage_v2.take();
            (identity, buffered, load_usage, load_usage_v2)
        };

        // Identity first: it is the event that tells the control plane which
        // conversation this thread is, and it must precede facts about it.
        self.report_all_locked(identity, None).await;
        for pending in buffered {
            let events = {
                let mut state = self.state.lock().await;
                match &pending.update {
                    PendingUpdateKind::V1(update) => state.translator.on_session_update(update),
                    PendingUpdateKind::V2(update) => state.translator.on_v2_session_update(update),
                }
            };
            self.report_all_locked(events, None).await;
            self.report_commands().await;
        }
        if let Some(update) = load_usage {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_session_update(&update)
            };
            self.report_all_locked(events, None).await;
        }
        if let Some(update) = load_usage_v2 {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_v2_session_update(&update)
            };
            self.report_all_locked(events, None).await;
        }
    }

    /// Initialize, open a v1 session, send the prompt, and finish from the
    /// response's stop reason.
    async fn converse_v1(&self, connection: &ConnectionTo<Agent>, cwd: &str) -> Result<(), Error> {
        let initialized = connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1).client_info(
                agent_client_protocol::schema::v1::Implementation::new(
                    "loom",
                    env!("CARGO_PKG_VERSION"),
                ),
            ))
            .block_task()
            .await?;
        if initialized.protocol_version != ProtocolVersion::V1 {
            return Err(Error::internal_error().data(
                "the ACP agent negotiated an unsupported protocol version for the v1 client",
            ));
        }
        if self.run.provider_session_id.is_some() && !initialized.agent_capabilities.load_session {
            return Err(Error::internal_error().data(
                "the ACP agent does not advertise session/load support for this resumed run",
            ));
        }

        let session_id = match &self.run.provider_session_id {
            Some(existing) => {
                self.begin_load(existing.clone()).await;
                let loaded = connection
                    .send_request(LoadSessionRequest::new(existing.clone(), cwd))
                    .block_task()
                    .await?;
                self.finish_load(existing).await;
                self.on_session_known(existing).await;
                let options = loaded.config_options.unwrap_or_default();
                let (options, choice_events) = self
                    .apply_config_choices_v1(connection, existing, options)
                    .await;
                self.report_all(choice_events).await;
                self.report_catalog_v1(&options).await;
                existing.clone()
            }
            None => {
                let created = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let session_id = created.session_id.0.to_string();
                self.on_session_known(&session_id).await;
                let options = created.config_options.unwrap_or_default();
                let (options, choice_events) = self
                    .apply_config_choices_v1(connection, &session_id, options)
                    .await;
                self.report_all(choice_events).await;
                self.report_catalog_v1(&options).await;
                session_id
            }
        };

        if let Some(update) = self.ready_for_prompt().await {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_session_update(&update)
            };
            self.report_all(events).await;
        }
        // The turn is a small loop because a steer *joins* it rather than
        // starting a second one. When a steer arrives the prompt in flight is
        // cancelled and the steer text becomes the next prompt on the same
        // session, so the model sees it at the next tool boundary with the
        // conversation intact. The run ends only when nothing is waiting, which
        // keeps its terminal event with the last prompt rather than a cancelled
        // one.
        let mut steers = self.take_steers().await;
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending = self.run.prompt.clone();
        loop {
            let prompt = PromptRequest::new(
                SessionId::new(session_id.clone()),
                vec![ContentBlock::Text(TextContent::new(pending.clone()))],
            );
            let mut prompt_fut = Box::pin(connection.send_request(prompt).block_task());
            let mut cancel_sent = false;
            let response = loop {
                tokio::select! {
                    result = &mut prompt_fut => break result?,
                    text = next_steer(&mut steers) => {
                        queued.push_back(text);
                        // One cancel is enough: the first steer abandons the
                        // prompt, and any steer behind it waits for the
                        // re-prompt the loop is about to send.
                        if !cancel_sent {
                            cancel_sent = true;
                            cancel_prompt(connection, &session_id).await;
                        }
                    }
                }
            };
            drain_steers(&mut steers, &mut queued);
            match queued.pop_front() {
                Some(next) => pending = next,
                None => {
                    let events = {
                        let mut state = self.state.lock().await;
                        state.translator.on_stop_reason(response.stop_reason)
                    };
                    self.report_all(events).await;
                    return Ok(());
                }
            }
        }
    }

    /// Apply the client's model and reasoning choices to a session.
    ///
    /// The agent owns the meaning of both values: each is an id it advertised
    /// through the session's config options. The model goes first because the
    /// levels a session offers are the model's own, so the level is chosen
    /// against the option set the model change returned rather than against
    /// what loom recorded when the thread was created.
    ///
    /// A choice the agent refuses is not a run failure. A refusal means the
    /// catalogue moved under a stored choice, and the conversation is still
    /// worth having on the agent's own default for the model it holds.
    ///
    /// What it must not be is *silent*. A run that quietly uses a different
    /// model from the one the thread names produces an answer the user will
    /// attribute to the model they chose, and a thinking level that was dropped
    /// looks exactly like an agent that has nothing to think. Both are reported
    /// as the contract's own events, which the client already renders.
    ///
    /// Returns the option set the session holds afterwards, which is the
    /// freshest description of the agent's catalogue this run has seen, and the
    /// events that describe what it would not take.
    async fn apply_config_choices(
        &self,
        connection: &ConnectionTo<Agent>,
        session_id: &str,
        mut options: Vec<v2::SessionConfigOption>,
    ) -> (Vec<v2::SessionConfigOption>, Translated) {
        let mut events: Translated = Vec::new();
        let provider_thread_id = self.provider_thread_id().await;

        if let Some(model) = self.run.model.as_deref() {
            match set_config_option(connection, session_id, MODEL_CONFIG_ID, model).await {
                Ok(updated) => options = updated,
                Err(error) => {
                    // The fallback is the model the session already holds, and
                    // it is only worth naming when the agent's own reply names
                    // it.
                    let held = select_state(&options, MODEL_CONFIG_ID)
                        .map(|(current, _)| current)
                        .unwrap_or_else(|| "the agent's default".to_owned());
                    eprintln!("loom-worker: the agent did not accept model `{model}`: {error}");
                    events.push(ProviderEvent::ProviderModelFallback {
                        provider_thread_id: provider_thread_id.clone(),
                        original_model: model.to_owned(),
                        fallback_model: held.clone(),
                        reason: ModelFallbackReason::Refusal,
                        message: format!(
                            "The agent did not accept `{model}` for this session, so this turn \
                             ran on `{held}`."
                        ),
                    });
                }
            }
        }

        let Some(level) = self.run.reasoning_level.as_ref() else {
            return (options, events);
        };
        let Some(value) = thought_level_value(&options, level) else {
            // The model on this session does not offer the level the thread
            // names — the ladder follows the model, so this is what a model
            // change looks like from the other side. loom leaves the level
            // unset rather than approximating with a different one, and says
            // which levels the model does offer.
            if let Some((current, offered)) = select_state(&options, THOUGHT_LEVEL_CONFIG_ID) {
                events.push(ProviderEvent::ProviderWarning {
                    provider_thread_id: provider_thread_id.clone(),
                    category: ProviderWarningCategory::Config,
                    summary: Some(format!(
                        "This model does not offer the reasoning level `{}`",
                        level.as_str()
                    )),
                    details: Some(format!(
                        "The turn ran at the agent's own default `{current}`. The model offers: \
                         {}.",
                        offered.join(", ")
                    )),
                });
            }
            return (options, events);
        };
        match set_config_option(connection, session_id, THOUGHT_LEVEL_CONFIG_ID, &value).await {
            // The reply after the level change is the option set the session
            // holds, so it is the one worth reporting.
            Ok(updated) => options = updated,
            Err(error) => {
                eprintln!(
                    "loom-worker: the agent did not accept reasoning level `{value}`: {error}"
                );
                events.push(ProviderEvent::ProviderWarning {
                    provider_thread_id: provider_thread_id.clone(),
                    category: ProviderWarningCategory::Config,
                    summary: Some(format!(
                        "The agent did not accept the reasoning level `{}`",
                        level.as_str()
                    )),
                    details: Some(format!(
                        "The turn ran at whatever level the session already held: {error}"
                    )),
                });
            }
        }
        (options, events)
    }

    /// Apply the client's model and reasoning choices to an ACP v1 session.
    ///
    /// v1 returns an optional config list and lets the agent choose option ids.
    /// The model is applied first because changing it may replace the reasoning
    /// ladder; both the lookup and the setter therefore use the freshest reply.
    async fn apply_config_choices_v1(
        &self,
        connection: &ConnectionTo<Agent>,
        session_id: &str,
        mut options: Vec<v1::SessionConfigOption>,
    ) -> (Vec<v1::SessionConfigOption>, Translated) {
        let mut events: Translated = Vec::new();
        let provider_thread_id = self.provider_thread_id().await;

        if let Some(model) = self.run.model.as_deref() {
            if let Some((config_id, held, _)) = select_state_v1(
                &options,
                MODEL_CONFIG_ID,
                v1::SessionConfigOptionCategory::Model,
            ) {
                match set_config_option_v1(connection, session_id, &config_id, model).await {
                    Ok(updated) => options = updated,
                    Err(error) => {
                        eprintln!(
                            "loom-worker: the v1 agent did not accept model `{model}`: {error}"
                        );
                        events.push(ProviderEvent::ProviderModelFallback {
                            provider_thread_id: provider_thread_id.clone(),
                            original_model: model.to_owned(),
                            fallback_model: held.clone(),
                            reason: ModelFallbackReason::Refusal,
                            message: format!(
                                "The agent did not accept `{model}` for this session, so this turn \
                                 ran on `{held}`."
                            ),
                        });
                    }
                }
            }
        }

        let Some(level) = self.run.reasoning_level.as_ref() else {
            return (options, events);
        };
        let Some((config_id, value)) = thought_level_value_v1(&options, level) else {
            if let Some((_, current, offered)) = select_state_v1(
                &options,
                THOUGHT_LEVEL_CONFIG_ID,
                v1::SessionConfigOptionCategory::ThoughtLevel,
            ) {
                events.push(ProviderEvent::ProviderWarning {
                    provider_thread_id: provider_thread_id.clone(),
                    category: ProviderWarningCategory::Config,
                    summary: Some(format!(
                        "This model does not offer the reasoning level `{}`",
                        level.as_str()
                    )),
                    details: Some(format!(
                        "The turn ran at the agent's own default `{current}`. The model offers: \
                         {}.",
                        offered.join(", ")
                    )),
                });
            }
            return (options, events);
        };
        match set_config_option_v1(connection, session_id, &config_id, &value).await {
            Ok(updated) => options = updated,
            Err(error) => {
                eprintln!(
                    "loom-worker: the v1 agent did not accept reasoning level `{value}`: {error}"
                );
                events.push(ProviderEvent::ProviderWarning {
                    provider_thread_id,
                    category: ProviderWarningCategory::Config,
                    summary: Some(format!(
                        "The agent did not accept the reasoning level `{}`",
                        level.as_str()
                    )),
                    details: Some(format!(
                        "The turn ran at whatever level the session already held: {error}"
                    )),
                });
            }
        }
        (options, events)
    }

    /// The provider thread id this session reports against.
    ///
    /// Read through the translator rather than guessed, because the agent's own
    /// session id is what a consumer joins these events on.
    async fn provider_thread_id(&self) -> String {
        self.state.lock().await.translator.ptid()
    }

    /// Reports what this session's config options say the agent can run.
    ///
    /// The catalogue is a fact about the host, but a live session is the only
    /// place it is published, so every turn refreshes it for free. This is what
    /// keeps a stored model choice honest when the agent's list changes under
    /// it: the picker is corrected on the next turn rather than at the next
    /// worker restart.
    async fn report_catalog(&self, options: &[v2::SessionConfigOption]) {
        let catalog = super::catalog::catalog_from_options(options);
        if catalog.is_empty() {
            return;
        }
        let _ = self
            .catalogs
            .send(ProviderCatalogReport {
                host_id: self.run.host_id.clone(),
                // The agent that served this session: a machine may run
                // several, and the server keys catalogues by the pair.
                provider_id: self.run.spec.name.clone(),
                catalog,
            })
            .await;
    }

    /// Reports a v1 session's config options as the host's catalogue.
    async fn report_catalog_v1(&self, options: &[v1::SessionConfigOption]) {
        let catalog = super::catalog::catalog_from_v1_options(options);
        if catalog.is_empty() {
            return;
        }
        let _ = self
            .catalogs
            .send(ProviderCatalogReport {
                host_id: self.run.host_id.clone(),
                provider_id: self.run.spec.name.clone(),
                catalog,
            })
            .await;
    }

    /// Reports the commands the session advertised, once, after the update
    /// that carried them was translated.
    ///
    /// ACP's `AvailableCommandsUpdate` translates to no event — the contract
    /// has no command-list fact — so the list leaves on its own channel. The
    /// session's working directory is part of the report because prompt files
    /// are read from it: the same agent in another workspace has a different
    /// list.
    async fn report_commands(&self) {
        let commands = {
            let mut state = self.state.lock().await;
            state.translator.take_advertised_commands()
        };
        let Some(commands) = commands else { return };
        let Some(cwd) = self.run.spec.cwd.clone() else {
            return;
        };
        let _ = self
            .commands
            .send(ProviderCommandsReport {
                host_id: self.run.host_id.clone(),
                provider_id: self.run.spec.name.clone(),
                cwd,
                commands,
            })
            .await;
    }

    /// Initialize, open or resume a v2 session, send the prompt, and wait for
    /// the protocol's `state_update: idle` notification. The v2 resume request
    /// intentionally omits `replayFrom`: loom's timeline already owns the
    /// history, so replaying it would duplicate events in the current run.
    async fn converse_v2(&self, connection: &ConnectionTo<Agent>, cwd: &str) -> Result<(), Error> {
        acp_trace!("negotiated ACP v2 for run {}", self.run.run_id);
        let initialized = connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("loom", env!("CARGO_PKG_VERSION")),
            ))
            .block_task()
            .await?;
        if initialized.protocol_version != ProtocolVersion::V2 {
            return Err(Error::internal_error().data(
                "the ACP agent negotiated an unsupported protocol version for the v2 client",
            ));
        }
        if initialized.capabilities.session.is_none() {
            return Err(Error::internal_error().data(
                "the ACP v2 agent does not advertise the session capability required by loom",
            ));
        }

        let session_id = match &self.run.provider_session_id {
            Some(existing) => {
                self.begin_load(existing.clone()).await;
                let resumed = connection
                    .send_request(v2::ResumeSessionRequest::new(existing.clone(), cwd))
                    .block_task()
                    .await?;
                self.finish_load(existing).await;
                self.on_session_known(existing).await;
                let (options, choice_events) = self
                    .apply_config_choices(connection, existing, resumed.config_options)
                    .await;
                self.report_all(choice_events).await;
                self.report_catalog(&options).await;
                existing.clone()
            }
            None => {
                let created = connection
                    .send_request(v2::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let session_id = created.session_id.0.to_string();
                self.on_session_known(&session_id).await;
                let (options, choice_events) = self
                    .apply_config_choices(connection, &session_id, created.config_options)
                    .await;
                self.report_all(choice_events).await;
                self.report_catalog(&options).await;
                session_id
            }
        };

        if let Some(update) = self.ready_for_prompt_v2().await {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_v2_session_update(&update)
            };
            self.report_all(events).await;
        }
        // The turn is a small loop for the same reason the v1 one is: a steer
        // joins the turn by cancelling the prompt in flight and re-prompting on
        // the same session. v2 reports a turn's end through `state_update: idle`
        // as well as the response, so while a steer is being applied the
        // notification path suppresses the terminal (see `steer_in_flight`) and
        // this loop owns the run's one terminal, emitting it from the response
        // of the last prompt. The response ends the prompt. A v2 agent normally
        // reports the *reason* through `state_update: idle`, and that
        // notification is the primary signal — but the response is evidence too,
        // and an agent can lose the notification on the way out: pi-acp, for
        // instance, tears down its outbound connector when one update fails to
        // convert, and the idle update that follows is dropped. Trusting only
        // the notification leaves the run in flight until the control plane's
        // timeout (W-623), so the response closes the turn when nothing else
        // has.
        let mut steers = self.take_steers().await;
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending = self.run.prompt.clone();
        loop {
            let prompt = v2::PromptRequest::new(
                v2::SessionId::new(session_id.clone()),
                vec![v2::ContentBlock::Text(v2::TextContent::new(
                    pending.clone(),
                ))],
            );
            let mut prompt_fut = Box::pin(connection.send_request(prompt).block_task());
            let mut cancel_sent = false;
            loop {
                tokio::select! {
                    result = &mut prompt_fut => {
                        result?;
                        break;
                    }
                    text = next_steer(&mut steers) => {
                        queued.push_back(text);
                        self.steer_in_flight
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        // One cancel is enough: the first steer abandons the
                        // prompt, and any steer behind it waits for the
                        // re-prompt the loop is about to send.
                        if !cancel_sent {
                            cancel_sent = true;
                            cancel_prompt_v2(connection, &session_id).await;
                        }
                    }
                }
            }
            drain_steers(&mut steers, &mut queued);
            match queued.pop_front() {
                Some(next) => {
                    if self.terminal_sent.load(std::sync::atomic::Ordering::SeqCst) {
                        // The turn's own end reached the run before this steer
                        // could join it — the notification path emitted a
                        // terminal because no steer was pending at that instant.
                        // The control plane has already finished the run, so
                        // re-prompting would only run an unowned turn.
                        return Ok(());
                    }
                    pending = next;
                }
                None => {
                    // No steer is left, so the run ends here, from the last
                    // prompt's response. Clearing the flag first lets the
                    // terminal through; a stale idle from an earlier cancelled
                    // prompt arrives after `terminal_sent` is set and is
                    // dropped by the guard in `report_all`.
                    self.steer_in_flight
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    self.settle_from_prompt_response().await;
                    self.wait_for_completion().await;
                    return Ok(());
                }
            }
        }
    }

    /// Ends the turn from the prompt response, when nothing else did.
    ///
    /// The reason a v2 response cannot carry is `EndTurn`. A turn cancelled by
    /// a **steer** is not this run's end at all — the conversation re-prompts
    /// instead — so only the last prompt reaches here, and `EndTurn` is the
    /// honest verdict for it. The mapping is the same one pi-acp uses to build
    /// the idle notification it may have dropped, which is why this also closes
    /// the turn when that notification never arrives (W-623).
    async fn settle_from_prompt_response(&self) {
        use std::sync::atomic::Ordering;
        if self.terminal_sent.load(Ordering::SeqCst) {
            return;
        }
        acp_trace!("prompt response returned; closing the turn from it");
        let events = {
            let mut state = self.state.lock().await;
            state.translator.on_v2_stop_reason(v2::StopReason::EndTurn)
        };
        self.report_all(events).await;
    }

    async fn wait_for_completion(&self) {
        use std::sync::atomic::Ordering;
        acp_trace!("prompt sent; waiting for a terminal event");
        while !self.terminal_sent.load(Ordering::SeqCst) {
            self.completion_notify.notified().await;
        }
    }

    /// Ends the run with a failure, unless it already ended.
    async fn terminal_failure(&self, message: String) -> Result<(), String> {
        let events = {
            let mut state = self.state.lock().await;
            state
                .translator
                .on_failure(message, ProviderErrorCategory::ConnectionFailed)
        };
        self.report_all(events).await;
        Ok(())
    }

    /// Ends the run with a budget expiry, unless it already ended.
    ///
    /// The category is `BudgetExceeded`, not `ConnectionFailed`: a run that
    /// spent its budget had a working connection, and a client that treats a
    /// connection failure specially would do the wrong thing with this. bb's
    /// category set has no dedicated timeout value, and this is the one whose
    /// contract meaning — "a budget was exceeded" — is exactly the case; the
    /// message names which budget and when.
    async fn terminal_timeout(&self, message: String) -> Result<(), String> {
        let events = {
            let mut state = self.state.lock().await;
            state
                .translator
                .on_failure(message, ProviderErrorCategory::BudgetExceeded)
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
            acp_trace!(
                "translated {} (terminal: {})",
                body.kind(),
                body.is_terminal()
            );
            self.liveness.observe(&body);
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
                self.completion_notify.notify_one();
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
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    run: ProviderRun,
    transport: Transport,
    reports: mpsc::Sender<ProviderReport>,
    catalogs: mpsc::Sender<ProviderCatalogReport>,
    commands: mpsc::Sender<ProviderCommandsReport>,
    permissions: PermissionRegistry,
    interactions: mpsc::Sender<InteractionRequest>,
    steers: crate::steer::SteerRegistry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(message) = drive(
            &run,
            transport,
            &reports,
            &catalogs,
            &commands,
            permissions,
            interactions,
            &steers,
        )
        .await
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter takes whole seconds, so a sub-second budget must round up
    /// rather than truncate to `0` — which would disable the fallback instead
    /// of shortening it.
    #[test]
    fn settle_budget_rounds_up_to_whole_seconds() {
        assert_eq!(settle_timeout_secs(Duration::ZERO), 0);
        assert_eq!(settle_timeout_secs(Duration::from_millis(1)), 1);
        assert_eq!(settle_timeout_secs(Duration::from_millis(999)), 1);
        assert_eq!(settle_timeout_secs(Duration::from_millis(1_000)), 1);
        assert_eq!(settle_timeout_secs(Duration::from_millis(1_001)), 2);
        assert_eq!(settle_timeout_secs(Duration::from_secs(600)), 600);
    }

    /// A tool call, which is what a run's silence bound is held off for.
    fn tool_item(id: &str) -> loom_domain::ThreadEventItem {
        loom_domain::ThreadEventItem::ToolCall {
            id: id.to_owned(),
            server: None,
            tool: "fork".to_owned(),
            arguments: None,
            status: loom_domain::ItemStatus::Pending,
            result: None,
            error: None,
            duration_ms: None,
            presentation: None,
            parent_tool_call_id: None,
        }
    }

    /// A call that started and has not completed.
    fn open_item(id: &str) -> ProviderEvent {
        ProviderEvent::ItemStarted {
            item: tool_item(id),
            provider_thread_id: "session".to_owned(),
        }
    }

    /// The completion of a call [`open_item`] opened.
    fn completed_item(id: &str) -> ProviderEvent {
        ProviderEvent::ItemCompleted {
            item: tool_item(id),
            provider_thread_id: "session".to_owned(),
        }
    }

    /// A tool call that is still running is work, not silence.
    ///
    /// This is the rule that keeps a forked child agent — one item held open for
    /// half an hour — from being killed by the run's own wall clock. Only the
    /// ceiling can end it.
    #[test]
    fn an_open_item_holds_the_silence_budget_off() {
        let liveness = RunLiveness::new();
        let started = tokio::time::Instant::now();
        let idle = Duration::from_secs(30);
        let ceiling = Duration::from_secs(600);

        liveness.observe(&open_item("tool-1"));
        let (deadline, budget) = liveness
            .deadline(started, idle, ceiling)
            .expect("the ceiling is still a bound");
        assert_eq!(budget, RunBudget::Ceiling);
        assert_eq!(deadline, started + ceiling);
    }

    /// A finished item hands the silence budget back, from the completion.
    #[test]
    fn a_finished_item_gives_the_silence_budget_back() {
        let liveness = RunLiveness::new();
        let started = tokio::time::Instant::now();
        let idle = Duration::from_secs(30);
        let ceiling = Duration::from_secs(600);

        liveness.observe(&open_item("tool-1"));
        liveness.observe(&completed_item("tool-1"));
        let (deadline, budget) = liveness.deadline(started, idle, ceiling).expect("a bound");
        assert_eq!(budget, RunBudget::Timeout);
        assert!(
            deadline >= started + idle,
            "the silence bound must count from the completion: {deadline:?}"
        );
    }

    /// A message is not a call, however long it stays "open".
    ///
    /// A user or agent message has no completion in the contract — its
    /// `item/started` is its whole lifecycle — so counting one would hold the
    /// silence bound off for the rest of the run and leave only the ceiling.
    #[test]
    fn a_message_does_not_hold_the_silence_budget_off() {
        let liveness = RunLiveness::new();
        let started = tokio::time::Instant::now();
        let idle = Duration::from_secs(30);
        let ceiling = Duration::from_secs(600);

        liveness.observe(&ProviderEvent::ItemStarted {
            item: loom_domain::ThreadEventItem::UserMessage {
                id: "user-1".to_owned(),
                content: Vec::new(),
                client_request_id: None,
                parent_tool_call_id: None,
            },
            provider_thread_id: "session".to_owned(),
        });
        let (_, budget) = liveness.deadline(started, idle, ceiling).expect("a bound");
        assert_eq!(
            budget,
            RunBudget::Timeout,
            "a message must not disable the silence bound"
        );
    }

    /// `0` removes a bound rather than making it immediate.
    #[test]
    fn a_zero_budget_is_removed() {
        let liveness = RunLiveness::new();
        let started = tokio::time::Instant::now();

        // No silence bound: only the ceiling.
        let (_, budget) = liveness
            .deadline(started, Duration::ZERO, Duration::from_secs(600))
            .expect("the ceiling is still a bound");
        assert_eq!(budget, RunBudget::Ceiling);

        // An open item and no ceiling: nothing bounds the run.
        liveness.observe(&open_item("tool-1"));
        assert_eq!(
            liveness.deadline(started, Duration::from_secs(30), Duration::ZERO),
            None
        );

        // Neither bound is configured.
        assert_eq!(
            liveness.deadline(started, Duration::ZERO, Duration::ZERO),
            None
        );
    }

    /// The reason says which budget fired, not that the agent "did not settle".
    #[test]
    fn the_reason_names_the_budget_that_fired() {
        let silence = RunBudget::Timeout.reason(Duration::from_millis(1_500));
        assert!(silence.contains("no events for 1500ms"), "{silence}");
        assert!(!silence.contains("settle"), "{silence}");
        assert!(!silence.contains("connection"), "{silence}");

        let ceiling = RunBudget::Ceiling.reason(Duration::from_secs(6 * 60 * 60));
        assert!(ceiling.contains("21600000ms ceiling"), "{ceiling}");
    }

    fn select(config_id: &str, current: &str, values: &[&str]) -> v2::SessionConfigOption {
        v2::SessionConfigOption::select(
            config_id,
            config_id,
            current.to_string(),
            values
                .iter()
                .map(|value| v2::SessionConfigSelectOption::new(*value, *value))
                .collect::<Vec<_>>(),
        )
    }

    /// The client's value is the provider's own id, so it is asked for as it
    /// stands — including a spelling bb's own schema does not name.
    #[test]
    fn the_providers_own_value_is_asked_for_verbatim() {
        let options = vec![
            select(MODEL_CONFIG_ID, "provider/a", &["provider/a", "provider/b"]),
            select(THOUGHT_LEVEL_CONFIG_ID, "minimal", &["off", "minimal"]),
        ];
        assert_eq!(
            thought_level_value(&options, &ReasoningLevel::from("minimal")),
            Some("minimal".to_string())
        );
    }

    /// The ladder follows the model: a level this model does not offer is left
    /// unset rather than approximated, so the agent keeps its own default.
    #[test]
    fn a_level_the_model_does_not_offer_is_not_asked_for() {
        let options = vec![
            select(MODEL_CONFIG_ID, "provider/a", &["provider/a", "provider/b"]),
            select(THOUGHT_LEVEL_CONFIG_ID, "high", &["off", "high"]),
        ];
        assert_eq!(
            thought_level_value(&options, &ReasoningLevel::from("high")),
            Some("high".to_string())
        );
        assert_eq!(
            thought_level_value(&options, &ReasoningLevel::from("low")),
            None
        );
    }

    /// The fallback report names what the agent *did* hold, which is only
    /// knowable from the agent's own reply — a session on a model the thread
    /// did not ask for is exactly the case this exists to make visible.
    #[test]
    fn the_held_model_and_its_ladder_come_from_the_agents_reply() {
        let options = vec![
            select(MODEL_CONFIG_ID, "provider/a", &["provider/a", "provider/b"]),
            select(THOUGHT_LEVEL_CONFIG_ID, "high", &["off", "low", "high"]),
        ];
        assert_eq!(
            select_state(&options, MODEL_CONFIG_ID),
            Some((
                "provider/a".to_string(),
                vec!["provider/a".to_string(), "provider/b".to_string()]
            ))
        );
        assert_eq!(
            select_state(&options, THOUGHT_LEVEL_CONFIG_ID).map(|(_, offered)| offered),
            Some(vec![
                "off".to_string(),
                "low".to_string(),
                "high".to_string()
            ])
        );
        // An option the agent did not publish is absent, not invented.
        assert_eq!(select_state(&options, "service_tier"), None);
    }

    /// A grouped selector describes the same model, so it is searched too.
    #[test]
    fn a_grouped_level_selector_is_searched() {
        let option = v2::SessionConfigOption::select(
            THOUGHT_LEVEL_CONFIG_ID,
            "Thinking",
            "low".to_string(),
            v2::SessionConfigSelectOptions::Grouped(vec![v2::SessionConfigSelectGroup::new(
                "ladder",
                "Ladder",
                vec![v2::SessionConfigSelectOption::new("low", "Low")],
            )]),
        );
        assert_eq!(
            thought_level_value(&[option], &ReasoningLevel::from("low")),
            Some("low".to_string())
        );
    }

    /// An agent that names its options differently gets no choice applied
    /// rather than a request it cannot honour.
    #[test]
    fn an_agent_without_the_option_gets_no_choice() {
        let options = vec![select("reasoning_effort", "low", &["low"])];
        assert_eq!(
            thought_level_value(&options, &ReasoningLevel::from("low")),
            None
        );
    }

    /// ACP v1's category identifies reasoning even when the agent chooses a
    /// different option id, and the returned id is the one the setter must use.
    #[test]
    fn a_v1_reasoning_option_uses_the_agents_actual_id() {
        let options = vec![v1::SessionConfigOption::select(
            "thinking",
            "Thinking",
            "max",
            vec![
                v1::SessionConfigSelectOption::new("off", "Off"),
                v1::SessionConfigSelectOption::new("max", "Max"),
            ],
        )
        .category(v1::SessionConfigOptionCategory::ThoughtLevel)];

        assert_eq!(
            select_state_v1(
                &options,
                THOUGHT_LEVEL_CONFIG_ID,
                v1::SessionConfigOptionCategory::ThoughtLevel,
            ),
            Some((
                "thinking".to_owned(),
                "max".to_owned(),
                vec!["off".to_owned(), "max".to_owned()]
            ))
        );
        assert_eq!(
            thought_level_value_v1(&options, &ReasoningLevel::from("max")),
            Some(("thinking".to_owned(), "max".to_owned()))
        );
    }

    /// v1 value ids deliberately omit v2's explicit `type: "id"` marker.
    #[test]
    fn a_v1_config_request_has_the_v1_wire_shape() {
        let request = v1::SetSessionConfigOptionRequest::new(
            "session",
            "thinking",
            v1::SessionConfigOptionValue::value_id("high"),
        );
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "sessionId": "session",
                "configId": "thinking",
                "value": "high"
            })
        );
        assert!(json.get("type").is_none());
    }

    use std::time::Duration;

    use agent_client_protocol::schema::v2;

    /// A v2 ACP agent whose first prompt runs until it is cancelled and whose
    /// later prompts answer immediately.
    ///
    /// That is exactly the shape a steer needs: the first prompt is the turn in
    /// flight, the cancel is what a steer sends to join it, and every prompt
    /// after it is the steer re-prompted on the same session. Recording the
    /// prompt texts is what lets a test assert the session was *reused* rather
    /// than restarted.
    #[derive(Clone)]
    struct SteerableAgent {
        prompts: Arc<tokio::sync::Mutex<Vec<String>>>,
        first_prompt_seen: Arc<tokio::sync::Notify>,
        cancel_seen: Arc<tokio::sync::Notify>,
    }

    impl SteerableAgent {
        fn new() -> Self {
            Self {
                prompts: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                first_prompt_seen: Arc::new(tokio::sync::Notify::new()),
                cancel_seen: Arc::new(tokio::sync::Notify::new()),
            }
        }

        /// The plain text of a prompt, concatenated.
        fn prompt_text(request: &v2::PromptRequest) -> String {
            request
                .prompt
                .iter()
                .filter_map(|block| match block {
                    v2::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        }
    }

    impl ConnectTo<Client> for SteerableAgent {
        async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
            let prompts = self.clone();
            let cancels = self.clone();
            Agent
                .v2()
                .on_receive_request(
                    async move |_request: v2::InitializeRequest, responder, _cx| {
                        let mut capabilities = v2::AgentCapabilities::default();
                        capabilities.session = Some(v2::SessionCapabilities::default());
                        responder.respond(
                            v2::InitializeResponse::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("steerable", "0"),
                            )
                            .capabilities(capabilities),
                        )
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: v2::NewSessionRequest, responder, _cx| {
                        responder.respond(v2::NewSessionResponse::new("steerable-session"))
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: v2::PromptRequest, responder, cx| {
                        let agent = prompts.clone();
                        cx.spawn(async move {
                            let text = SteerableAgent::prompt_text(&request);
                            let index = {
                                let mut prompts = agent.prompts.lock().await;
                                prompts.push(text);
                                prompts.len()
                            };
                            if index == 1 {
                                // The first turn stays open until the steer's
                                // cancel arrives; the response then ends it, and
                                // the worker re-prompts on this same session.
                                agent.first_prompt_seen.notify_one();
                                agent.cancel_seen.notified().await;
                            }
                            responder.respond(v2::PromptResponse::new())
                        })?;
                        Ok(())
                    },
                    on_receive_request!(),
                )
                .on_receive_notification(
                    async move |_notification: v2::CancelSessionNotification, _cx| {
                        cancels.cancel_seen.notify_one();
                        Ok(())
                    },
                    on_receive_notification!(),
                )
                .connect_to(client)
                .await
        }
    }

    /// Builds the sink `drive` would build for `run`, with a steer channel the
    /// test can drive directly.
    async fn sink_for(
        run: &ProviderRun,
        cwd: &str,
        steers: &crate::steer::SteerRegistry,
        reports: mpsc::Sender<ProviderReport>,
        catalogs: mpsc::Sender<ProviderCatalogReport>,
        commands: mpsc::Sender<ProviderCommandsReport>,
    ) -> UpdateSink {
        let (interactions, _interaction_requests) = mpsc::channel(8);
        let steer_rx = steers.register(run.run_id.clone()).await;
        UpdateSink {
            run: run.clone(),
            liveness: Arc::new(RunLiveness::new()),
            state: Arc::new(tokio::sync::Mutex::new(UpdateState {
                translator: AcpTranslator::new(RunContext {
                    thread_id: run.thread_id.clone(),
                    cwd: Some(cwd.to_owned()),
                    provider_session_id: None,
                }),
                phase: UpdatePhase::Constructing,
                pending: Vec::new(),
                pending_load_usage: None,
                pending_load_usage_v2: None,
            })),
            reports,
            catalogs,
            commands,
            report_lock: Arc::new(tokio::sync::Mutex::new(())),
            terminal_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            completion_notify: Arc::new(tokio::sync::Notify::new()),
            broker: PermissionBroker::new(
                run.clone(),
                interactions,
                PermissionRegistry::new(),
                run.permission_timeout,
            ),
            steers: Arc::new(tokio::sync::Mutex::new(Some(steer_rx))),
            steer_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// OMP sends the session title as a bootstrap `session_info_update` after
    /// `session/load` has answered. It must survive the load replay guard once
    /// the session identity has been emitted.
    #[tokio::test]
    async fn a_loaded_session_title_is_reported_after_load() {
        let dir = tempfile::tempdir().expect("a temporary workspace");
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut spec = loom_provider_protocol::ProviderSpec::acp("unused", Vec::new());
        spec.cwd = Some(cwd.clone());
        let run = ProviderRun {
            spec,
            prompt: "prompt".to_owned(),
            host_id: loom_domain::HostId::mint(),
            thread_id: loom_domain::ThreadId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            run_id: loom_domain::RunId::mint(),
            timeout: Duration::from_secs(10),
            ceiling: crate::DEFAULT_RUN_CEILING,
            permission_timeout: Duration::from_secs(5),
            settle_timeout: crate::DEFAULT_SETTLE_TIMEOUT,
            permission_ceiling: loom_domain::HostPermissionMode::Full,
            permission_mode: loom_domain::automation::PermissionMode::Full,
            provider_session_id: Some("session".to_owned()),
            model: None,
            reasoning_level: None,
        };
        let steers = crate::steer::SteerRegistry::new();
        let (reports_tx, mut reports_rx) = mpsc::channel(16);
        let (catalogs_tx, _catalog_reports) = mpsc::channel(4);
        let (commands_tx, _command_reports) = mpsc::channel(4);
        let sink = sink_for(&run, &cwd, &steers, reports_tx, catalogs_tx, commands_tx).await;

        sink.begin_load("session".to_owned()).await;
        sink.finish_load("session").await;
        sink.on_session_known("session").await;
        sink.on_notification(SessionNotification::new(
            "session",
            SessionUpdate::SessionInfoUpdate(v1::SessionInfoUpdate::new().title("Loaded title")),
        ))
        .await;

        let reports: Vec<_> = std::iter::from_fn(|| reports_rx.try_recv().ok()).collect();
        assert!(
            reports.iter().any(|report| matches!(
                &report.event.event.body,
                ProviderEvent::ThreadNameUpdated { thread_name, .. }
                    if thread_name == "Loaded title"
            )),
            "the load-time title reaches the reports: {reports:#?}"
        );
    }

    #[tokio::test]
    async fn load_time_v1_commands_survive_the_resume_replay_guard() {
        let dir = tempfile::tempdir().expect("a temporary workspace");
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut spec = loom_provider_protocol::ProviderSpec::acp("unused", Vec::new());
        spec.cwd = Some(cwd.clone());
        let run = ProviderRun {
            spec,
            prompt: "prompt".to_owned(),
            host_id: loom_domain::HostId::mint(),
            thread_id: loom_domain::ThreadId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            run_id: loom_domain::RunId::mint(),
            timeout: Duration::from_secs(10),
            ceiling: crate::DEFAULT_RUN_CEILING,
            permission_timeout: Duration::from_secs(5),
            settle_timeout: crate::DEFAULT_SETTLE_TIMEOUT,
            permission_ceiling: loom_domain::HostPermissionMode::Full,
            permission_mode: loom_domain::automation::PermissionMode::Full,
            provider_session_id: Some("session".to_owned()),
            model: None,
            reasoning_level: None,
        };
        let steers = crate::steer::SteerRegistry::new();
        let (reports_tx, _reports_rx) = mpsc::channel(16);
        let (catalogs_tx, _catalog_reports) = mpsc::channel(4);
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let sink = sink_for(&run, &cwd, &steers, reports_tx, catalogs_tx, commands_tx).await;

        sink.begin_load("session".to_owned()).await;
        sink.on_notification(SessionNotification::new(
            "session",
            SessionUpdate::AvailableCommandsUpdate(v1::AvailableCommandsUpdate::new(vec![
                v1::AvailableCommand::new("loading", "During load"),
            ])),
        ))
        .await;
        sink.finish_load("session").await;
        sink.on_notification(SessionNotification::new(
            "session",
            SessionUpdate::AvailableCommandsUpdate(v1::AvailableCommandsUpdate::new(vec![
                v1::AvailableCommand::new("loaded", "After load"),
            ])),
        ))
        .await;
        sink.on_session_known("session").await;

        let reports: Vec<_> = std::iter::from_fn(|| commands_rx.try_recv().ok()).collect();
        let names: Vec<Vec<String>> = reports
            .iter()
            .map(|report| {
                report
                    .commands
                    .iter()
                    .map(|command| command.name.clone())
                    .collect()
            })
            .collect();
        assert_eq!(names, vec![vec!["loading"], vec!["loaded"]]);
    }

    #[tokio::test]
    async fn load_time_v2_commands_survive_the_resume_replay_guard() {
        let dir = tempfile::tempdir().expect("a temporary workspace");
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut spec = loom_provider_protocol::ProviderSpec::acp("unused", Vec::new());
        spec.cwd = Some(cwd.clone());
        let run = ProviderRun {
            spec,
            prompt: "prompt".to_owned(),
            host_id: loom_domain::HostId::mint(),
            thread_id: loom_domain::ThreadId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            run_id: loom_domain::RunId::mint(),
            timeout: Duration::from_secs(10),
            ceiling: crate::DEFAULT_RUN_CEILING,
            permission_timeout: Duration::from_secs(5),
            settle_timeout: crate::DEFAULT_SETTLE_TIMEOUT,
            permission_ceiling: loom_domain::HostPermissionMode::Full,
            permission_mode: loom_domain::automation::PermissionMode::Full,
            provider_session_id: Some("session".to_owned()),
            model: None,
            reasoning_level: None,
        };
        let steers = crate::steer::SteerRegistry::new();
        let (reports_tx, _reports_rx) = mpsc::channel(16);
        let (catalogs_tx, _catalog_reports) = mpsc::channel(4);
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let sink = sink_for(&run, &cwd, &steers, reports_tx, catalogs_tx, commands_tx).await;

        sink.begin_load("session".to_owned()).await;
        sink.on_v2_notification(v2::UpdateSessionNotification::new(
            "session",
            v2::SessionUpdate::AvailableCommandsUpdate(v2::AvailableCommandsUpdate::new(vec![
                v2::AvailableCommand::new("loading", "During load"),
            ])),
        ))
        .await;
        sink.finish_load("session").await;
        sink.on_v2_notification(v2::UpdateSessionNotification::new(
            "session",
            v2::SessionUpdate::AvailableCommandsUpdate(v2::AvailableCommandsUpdate::new(vec![
                v2::AvailableCommand::new("loaded", "After load"),
            ])),
        ))
        .await;
        sink.on_session_known("session").await;

        let reports: Vec<_> = std::iter::from_fn(|| commands_rx.try_recv().ok()).collect();
        let names: Vec<Vec<String>> = reports
            .iter()
            .map(|report| {
                report
                    .commands
                    .iter()
                    .map(|command| command.name.clone())
                    .collect()
            })
            .collect();
        assert_eq!(names, vec![vec!["loading"], vec!["loaded"]]);
    }

    /// A steer cancels the prompt in flight and re-prompts the **same session**,
    /// so the run reports exactly one terminal event — from the prompt the steer
    /// became, not from the one it replaced.
    #[tokio::test]
    async fn a_steer_cancels_the_prompt_in_flight_and_reprompts_the_same_session() {
        let dir = tempfile::tempdir().expect("a temporary workspace");
        let cwd = dir.path().to_string_lossy().into_owned();

        let mut spec = loom_provider_protocol::ProviderSpec::acp("unused", Vec::new());
        spec.cwd = Some(cwd.clone());
        let run = ProviderRun {
            spec,
            prompt: "first".to_string(),
            host_id: loom_domain::HostId::mint(),
            thread_id: loom_domain::ThreadId::mint(),
            project_id: loom_domain::ProjectId::mint(),
            run_id: loom_domain::RunId::mint(),
            timeout: Duration::from_secs(10),
            ceiling: crate::DEFAULT_RUN_CEILING,
            permission_timeout: Duration::from_secs(5),
            settle_timeout: crate::DEFAULT_SETTLE_TIMEOUT,
            permission_ceiling: loom_domain::HostPermissionMode::Full,
            permission_mode: loom_domain::automation::PermissionMode::Full,
            provider_session_id: None,
            model: None,
            reasoning_level: None,
        };

        let steers = crate::steer::SteerRegistry::new();
        let (reports_tx, mut reports_rx) = mpsc::channel(64);
        let (catalogs_tx, _catalog_reports) = mpsc::channel(64);
        let (commands_tx, _command_reports) = mpsc::channel(64);
        let sink = sink_for(&run, &cwd, &steers, reports_tx, catalogs_tx, commands_tx).await;

        let agent = SteerableAgent::new();
        let drive_sink = sink.clone();
        let drive_cwd = cwd.clone();
        let drive_agent = agent.clone();
        let driver = tokio::spawn(async move {
            serve(move || drive_agent.clone(), &drive_sink, &drive_cwd).await
        });

        // The first prompt is in flight; join it with a steer.
        agent.first_prompt_seen.notified().await;
        assert!(
            steers
                .steer(&run.run_id, "actually, use the other fixture".into())
                .await,
            "the run registered a steer channel"
        );

        // The run must end once, and from the steer's prompt.
        let mut terminal = None;
        while let Some(report) = reports_rx.recv().await {
            if report.event.is_terminal() {
                terminal = Some(report.event);
                break;
            }
        }
        let terminal = terminal.expect("a terminal event");
        assert_eq!(
            terminal.terminal_status(),
            Some(loom_domain::TurnStatus::Completed),
            "the terminal comes from the prompt the steer became"
        );

        // Two prompts reached the agent, in order, on one session.
        let prompts = agent.prompts.lock().await.clone();
        assert_eq!(
            prompts,
            vec![
                "first".to_string(),
                "actually, use the other fixture".to_string()
            ],
            "the steer is the next prompt, after the cancelled one"
        );

        let outcome = driver.await.expect("the driver task did not panic");
        assert!(outcome.is_ok(), "the connection ended cleanly: {outcome:?}");
    }
}

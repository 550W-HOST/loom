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

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::schema::{v2, ProtocolVersion};
use agent_client_protocol::{
    on_receive_notification, on_receive_request, Agent, Client, ConnectTo, ConnectionTo, Error,
};
use loom_domain::{ProviderEvent, ReasoningLevel, RunEvent};
use loom_provider_protocol::{InteractionRequest, ProviderReport};
use tokio::sync::mpsc;

use super::permission::{PermissionBroker, PermissionRegistry};
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
pub async fn drive(
    run: &ProviderRun,
    transport: Transport,
    reports: &mpsc::Sender<ProviderReport>,
    permissions: PermissionRegistry,
    interactions: mpsc::Sender<InteractionRequest>,
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
            pending_load_usage_v2: None,
        })),
        reports: reports.clone(),
        report_lock: Arc::new(tokio::sync::Mutex::new(())),
        terminal_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        completion_notify: Arc::new(tokio::sync::Notify::new()),
        broker: PermissionBroker::new(
            run.clone(),
            interactions,
            permissions,
            run.permission_timeout,
        ),
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
                    let response = permissions.ask(request).await;
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
                    let request = v2::conversion::try_v2_to_v1(request).map_err(|error| {
                        Error::invalid_params().data(format!(
                            "could not convert ACP v2 permission request: {error}"
                        ))
                    })?;
                    let response = permissions.ask(request).await;
                    let response = v2::conversion::try_v1_to_v2(response).map_err(|error| {
                        Error::internal_error().data(format!(
                            "could not convert ACP permission response to v2: {error}"
                        ))
                    })?;
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

/// Builds a fresh embedded agent connection for each protocol negotiation
/// attempt. The connector may need a new connection when it falls back from
/// v2 to v1, so the adapter and its channel must be created per factory call.
pub(super) fn embedded_agent_factory(
    command: String,
) -> impl FnMut() -> EmbeddedAgentTransport + Send + 'static {
    move || {
        let mut config = pi_acp::config::Config::default();
        if !command.is_empty() {
            config.pi_command = command.clone();
        }
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
    serve(embedded_agent_factory(command), sink, cwd).await
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
    /// Keep the latest one; history and metadata updates are intentionally not
    /// replayed into the new run.
    pending_load_usage: Option<SessionUpdate>,
    /// The v2 equivalent of the load-time usage snapshot.
    pending_load_usage_v2: Option<v2::SessionUpdate>,
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
    /// Wakes the v2 conversation future when `StateUpdate::Idle` is reported.
    completion_notify: Arc<tokio::sync::Notify>,
    /// Where a permission request goes and how its answer gets back.
    ///
    /// A broker rather than the former in-place auto-allow: the ACP client must
    /// not decide a permission the user has not granted. See
    /// [`crate::acp::permission`].
    broker: PermissionBroker,
}

/// The ACP session config option ids for the model and the reasoning level.
///
/// ACP leaves the ids to the agent; these are the two pi advertises. An agent
/// that names them differently simply gets no choice applied, because every
/// lookup here reads the agent's own reply rather than assuming the option
/// exists.
const MODEL_CONFIG_ID: &str = "model";
const THOUGHT_LEVEL_CONFIG_ID: &str = "thought_level";

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

/// The thought-level value that answers `level`, if this model offers one.
///
/// Which levels exist is the model's business, so this only ever answers with a
/// value the agent advertised: an exact match on loom's name for the level, or
/// the one spelling loom's closed set and pi's ladder disagree on (`none` is
/// pi's `off`). A level the agent does not offer here is left unset rather than
/// approximated — the agent already holds a default for the model it is using.
fn thought_level_value(
    options: &[v2::SessionConfigOption],
    level: ReasoningLevel,
) -> Option<String> {
    let wanted = reasoning_level_id(level)?;
    let option = options
        .iter()
        .find(|option| option.config_id.0.as_ref() == THOUGHT_LEVEL_CONFIG_ID)?;
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
    candidates
        .iter()
        .find(|candidate| candidate.value.0.as_ref() == wanted)
        .map(|candidate| candidate.value.0.to_string())
}

/// loom's name for a level as the provider spells it.
///
/// loom's set is bb's `reasoningLevelSchema`, which is wider than any
/// provider's ladder: `ultracode` and `ultra` have no counterpart here, so they
/// are never asked for.
fn reasoning_level_id(level: ReasoningLevel) -> Option<&'static str> {
    match level {
        ReasoningLevel::None => Some("off"),
        ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High => Some("high"),
        ReasoningLevel::Xhigh => Some("xhigh"),
        ReasoningLevel::Max => Some("max"),
        ReasoningLevel::Ultracode | ReasoningLevel::Ultra => None,
    }
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
        acp_trace!(
            "v1 update {:?} for session {}",
            std::mem::discriminant(&notification.update),
            session_id
        );
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

    /// The v2 form of one ACP session update. Construction and resume use the
    /// same buffering rules as v1, but the schema is intentionally kept typed
    /// until it reaches the adapter translator.
    async fn on_v2_notification(&self, notification: v2::UpdateSessionNotification) {
        let session_id = notification.session_id.0.to_string();
        acp_trace!(
            "v2 update {} for session {}",
            v2_update_name(&notification.update),
            session_id
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
                }
                | UpdatePhase::Loaded {
                    session_id: expected,
                } => {
                    if expected == &session_id
                        && matches!(notification.update, v2::SessionUpdate::UsageUpdate(_))
                    {
                        state.pending_load_usage_v2 = Some(notification.update);
                    }
                    return;
                }
                UpdatePhase::Ready => {}
            }
            if state.translator.provider_session_id() != Some(session_id.as_str()) {
                return;
            }
            state.translator.on_v2_session_update(&notification.update)
        };
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

        if let Some(update) = self.ready_for_prompt().await {
            let events = {
                let mut state = self.state.lock().await;
                state.translator.on_session_update(&update)
            };
            self.report_all(events).await;
        }
        let prompt = PromptRequest::new(
            SessionId::new(session_id),
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
    async fn apply_config_choices(
        &self,
        connection: &ConnectionTo<Agent>,
        session_id: &str,
        mut options: Vec<v2::SessionConfigOption>,
    ) {
        if let Some(model) = self.run.model.as_deref() {
            if let Ok(updated) =
                set_config_option(connection, session_id, MODEL_CONFIG_ID, model).await
            {
                options = updated;
            }
        }
        let Some(level) = self.run.reasoning_level else {
            return;
        };
        let Some(value) = thought_level_value(&options, level) else {
            return;
        };
        let _ = set_config_option(connection, session_id, THOUGHT_LEVEL_CONFIG_ID, &value).await;
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
                self.apply_config_choices(connection, existing, resumed.config_options)
                    .await;
                existing.clone()
            }
            None => {
                let created = connection
                    .send_request(v2::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let session_id = created.session_id.0.to_string();
                self.on_session_known(&session_id).await;
                self.apply_config_choices(connection, &session_id, created.config_options)
                    .await;
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
        let prompt = v2::PromptRequest::new(
            v2::SessionId::new(session_id),
            vec![v2::ContentBlock::Text(v2::TextContent::new(
                self.run.prompt.clone(),
            ))],
        );
        let _response = connection.send_request(prompt).block_task().await?;
        // The response ends the prompt. A v2 agent normally reports the *reason*
        // through `state_update: idle`, and that notification is the primary
        // signal — but the response is evidence too, and an agent can lose the
        // notification on the way out: pi-acp, for instance, tears down its
        // outbound connector when one update fails to convert, and the idle
        // update that follows is dropped. Trusting only the notification leaves
        // the run in flight until the control plane's timeout (W-623), so the
        // response closes the turn when nothing else has.
        self.settle_from_prompt_response().await;
        self.wait_for_completion().await;
        Ok(())
    }

    /// Ends the turn from the prompt response, when nothing else did.
    ///
    /// The reason a v2 response cannot carry is `EndTurn`: a cancelled turn is
    /// reported as `Cancelled` by the notification path, and loom has no
    /// client-side cancel for an ACP run at all. The mapping is therefore the
    /// same one pi-acp uses to build the idle notification it may have dropped.
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
            acp_trace!(
                "translated {} (terminal: {})",
                body.kind(),
                body.is_terminal()
            );
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
pub fn spawn(
    run: ProviderRun,
    transport: Transport,
    reports: mpsc::Sender<ProviderReport>,
    permissions: PermissionRegistry,
    interactions: mpsc::Sender<InteractionRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(message) = drive(&run, transport, &reports, permissions, interactions).await {
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

    /// The levels on offer are the model's own. A level loom knows, but the
    /// model the agent holds does not, is left unset rather than approximated.
    #[test]
    fn a_level_the_model_does_not_offer_is_not_asked_for() {
        let options = vec![
            select(MODEL_CONFIG_ID, "provider/a", &["provider/a", "provider/b"]),
            select(THOUGHT_LEVEL_CONFIG_ID, "high", &["off", "high"]),
        ];
        assert_eq!(
            thought_level_value(&options, ReasoningLevel::High),
            Some("high".to_string())
        );
        assert_eq!(thought_level_value(&options, ReasoningLevel::Low), None);
    }

    #[test]
    fn loom_none_is_asked_for_as_the_providers_off() {
        let options = vec![select(THOUGHT_LEVEL_CONFIG_ID, "off", &["off"])];
        assert_eq!(
            thought_level_value(&options, ReasoningLevel::None),
            Some("off".to_string())
        );
    }

    /// bb's closed set is wider than a provider's ladder, so the levels with no
    /// counterpart are never sent — the agent keeps its own default instead.
    #[test]
    fn levels_without_a_provider_counterpart_are_not_asked_for() {
        let options = vec![select(THOUGHT_LEVEL_CONFIG_ID, "max", &["off", "max"])];
        assert_eq!(
            thought_level_value(&options, ReasoningLevel::Max),
            Some("max".to_string())
        );
        assert_eq!(
            thought_level_value(&options, ReasoningLevel::Ultracode),
            None
        );
        assert_eq!(thought_level_value(&options, ReasoningLevel::Ultra), None);
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
            thought_level_value(&[option], ReasoningLevel::Low),
            Some("low".to_string())
        );
    }

    /// An agent that names its options differently gets no choice applied
    /// rather than a request it cannot honour.
    #[test]
    fn an_agent_without_the_option_gets_no_choice() {
        let options = vec![select("reasoning_effort", "low", &["low"])];
        assert_eq!(thought_level_value(&options, ReasoningLevel::Low), None);
    }
}

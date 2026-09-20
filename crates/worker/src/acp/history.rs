//! Restoring a conversation from its ACP session, without running it.
//!
//! A thread's history lives with the agent that owns the session, on the
//! machine that opened it. Loading it is therefore a read on that host: open
//! the session, let the agent replay it, close again. No prompt is sent, no run
//! is created, and no entity state changes — which is why this does not go
//! through `session::drive`, whose whole shape is the lifecycle of a run.
//!
//! The replay's completion boundary is the `session/load` (v1) or
//! `session/resume` (v2) *response*. That is a guarantee rather than a race
//! only because the agent publishes the history before answering it, which ACP
//! v2 states and `pi-acp` does as of `2f13a18`. Everything the translator
//! produced by the time that response arrives is the whole conversation, so
//! nothing is polled for afterwards — and an agent that answers first would be
//! caught as an incomplete replay rather than silently cached half a history.
//!
//! Entries are the provider-neutral bodies a live run translates into, with no
//! `RunEvent` wrapper: a restored conversation belongs to no loom run and must
//! not look like one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::{v1, v2, ProtocolVersion};
use agent_client_protocol::{
    on_receive_notification, on_receive_request, Agent, Client, ConnectTo, ConnectionTo, Error,
};
use loom_domain::{ProviderEvent, ThreadId};

use super::session::{agent_argv, embedded_agent_factory, Transport};
use super::{AcpTranslator, RunContext};

/// Why a history load did not produce a whole conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryFailure {
    /// A stable, machine-readable reason a caller can branch on.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl HistoryFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The bounds a load must respect.
#[derive(Clone, Copy, Debug)]
pub struct HistoryLimits {
    /// Upper bound on the serialized entries the whole replay may produce.
    ///
    /// Exceeding it **fails** the load; it never truncates. A partial
    /// conversation presented as the conversation is worse than no answer,
    /// because nothing downstream can tell the difference.
    pub max_total_bytes: u64,
    /// How long the agent has to answer the load.
    pub budget: Duration,
}

/// Loads one session's conversation from `transport`.
pub async fn load_history(
    transport: Transport,
    cwd: String,
    thread_id: ThreadId,
    session_id: String,
    limits: HistoryLimits,
) -> Result<Vec<ProviderEvent>, HistoryFailure> {
    let operation = async {
        match transport {
            Transport::Stdio { command, args } => {
                let argv = agent_argv(&command, &args, &cwd);
                agent_client_protocol::AcpAgent::from_args(argv.clone()).map_err(|error| {
                    HistoryFailure::new(
                        "agent_launch",
                        format!("could not describe the ACP agent: {error}"),
                    )
                })?;
                replay(
                    move || {
                        agent_client_protocol::AcpAgent::from_args(argv.clone())
                            .expect("validated ACP agent arguments")
                    },
                    cwd,
                    thread_id,
                    session_id,
                    limits.max_total_bytes,
                )
                .await
            }
            // Only `pi` is a child here; the adapter itself is in-process, so
            // there is no argv to build for it.
            Transport::EmbeddedPi { command, args } => {
                if !args.is_empty() {
                    return Err(HistoryFailure::new(
                        "agent_launch",
                        format!(
                            "the embedded pi-acp transport takes no provider arguments, but the \
                             request supplies {args:?}"
                        ),
                    ));
                }
                replay(
                    embedded_agent_factory(command),
                    cwd,
                    thread_id,
                    session_id,
                    limits.max_total_bytes,
                )
                .await
            }
        }
    };

    match tokio::time::timeout(limits.budget, operation).await {
        Ok(outcome) => outcome,
        Err(_) => Err(HistoryFailure::new(
            "timeout",
            format!(
                "the ACP agent did not answer the history load within {}ms",
                limits.budget.as_millis()
            ),
        )),
    }
}

/// Runs initialize-and-restore against a connected agent, both versions.
///
/// Both are registered because a v1 agent is a working agent: a session
/// restored through `session/load` is the same conversation as one restored
/// through `session/resume`, so refusing v1 would refuse every agent that has
/// not moved to the draft.
async fn replay<C, F>(
    agent_factory: F,
    cwd: String,
    thread_id: ThreadId,
    session_id: String,
    max_total_bytes: u64,
) -> Result<Vec<ProviderEvent>, HistoryFailure>
where
    C: ConnectTo<Client>,
    F: FnMut() -> C + Send + 'static,
{
    let state = Arc::new(HistoryState::new(
        thread_id,
        cwd.clone(),
        session_id.clone(),
        max_total_bytes,
    ));

    let v1_cwd = cwd.clone();
    let v1_state = Arc::clone(&state);
    let v1_session = session_id.clone();
    let v2_state = Arc::clone(&state);
    let v2_session = session_id;
    let result = Client
        .protocol_connector()
        .with_v1(move || V1HistoryClient {
            state: Arc::clone(&v1_state),
            cwd: v1_cwd.clone(),
            session_id: v1_session.clone(),
        })
        .with_v2(move || V2HistoryClient {
            state: Arc::clone(&v2_state),
            cwd: cwd.clone(),
            session_id: v2_session.clone(),
        })
        .connect_to(agent_factory)
        .await;

    result.map_err(|error| {
        HistoryFailure::new(
            "connection",
            format!("the ACP connection ended: {error}"),
        )
    })?;
    state.take()
}

/// What a replay accumulates, shared between the callbacks and the driver.
struct HistoryState {
    inner: Mutex<Inner>,
}

struct Inner {
    translator: AcpTranslator,
    entries: Vec<ProviderEvent>,
    bytes: u64,
    max_total_bytes: u64,
    overflowed: bool,
    answered: bool,
}

impl HistoryState {
    fn new(
        thread_id: ThreadId,
        cwd: String,
        session_id: String,
        max_total_bytes: u64,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                translator: AcpTranslator::new(RunContext {
                    thread_id,
                    cwd: Some(cwd),
                    provider_session_id: Some(session_id),
                }),
                entries: Vec::new(),
                bytes: 0,
                max_total_bytes,
                overflowed: false,
                answered: false,
            }),
        }
    }

    fn on_v1(&self, update: &v1::SessionUpdate) {
        let mut inner = self.lock();
        if inner.overflowed {
            return;
        }
        let translated = inner.translator.on_session_update(update);
        inner.push(translated);
    }

    fn on_v2(&self, update: &v2::SessionUpdate) {
        let mut inner = self.lock();
        if inner.overflowed {
            return;
        }
        let translated = inner.translator.on_v2_session_update(update);
        inner.push(translated);
    }

    /// Records that the restore request was answered, which is the replay's
    /// completion boundary.
    fn answered(&self) {
        self.lock().answered = true;
    }

    fn take(&self) -> Result<Vec<ProviderEvent>, HistoryFailure> {
        let mut inner = self.lock();
        if inner.overflowed {
            inner.entries.clear();
            return Err(HistoryFailure::new(
                "too_large",
                format!(
                    "the conversation exceeds the {}-byte history budget",
                    inner.max_total_bytes
                ),
            ));
        }
        if !inner.answered {
            inner.entries.clear();
            return Err(HistoryFailure::new(
                "incomplete",
                "the agent ended the connection before answering the history load",
            ));
        }
        Ok(std::mem::take(&mut inner.entries))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn push(&mut self, events: Vec<ProviderEvent>) {
        for event in events {
            if self.overflowed {
                return;
            }
            let size = serde_json::to_vec(&event)
                .map(|encoded| encoded.len() as u64)
                .unwrap_or(0);
            if self.bytes.saturating_add(size) > self.max_total_bytes {
                self.overflowed = true;
                self.entries.clear();
                return;
            }
            self.bytes += size;
            self.entries.push(event);
        }
    }
}

/// The v1 half: restore with `session/load`, which replays by definition.
struct V1HistoryClient {
    state: Arc<HistoryState>,
    cwd: String,
    session_id: String,
}

impl ConnectTo<Agent> for V1HistoryClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let notifications = Arc::clone(&self.state);
        let answered = self.state;
        let cwd = self.cwd;
        let session_id = self.session_id;

        Client
            .builder()
            .on_receive_notification(
                async move |notification: v1::SessionNotification, _cx| {
                    notifications.on_v1(&notification.update);
                    Ok(())
                },
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |_request: v1::RequestPermissionRequest, responder, _cx| {
                    // A replay reads a conversation; it never executes one. An
                    // agent that asks for permission while replaying is
                    // refused rather than shown to a user who asked to look
                    // back, not to approve anything.
                    let _ = responder.respond(v1::RequestPermissionResponse::new(
                        v1::RequestPermissionOutcome::Cancelled,
                    ));
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(
                        v1::InitializeRequest::new(ProtocolVersion::V1).client_info(
                            v1::Implementation::new("loom", env!("CARGO_PKG_VERSION")),
                        ),
                    )
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V1 {
                    return Err(Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version for the \
                         history load",
                    ));
                }
                // The response is the completion boundary: the agent publishes
                // the whole replay before answering.
                connection
                    .send_request(v1::LoadSessionRequest::new(session_id, cwd))
                    .block_task()
                    .await?;
                answered.answered();
                Ok(())
            })
            .await
    }
}

/// The v2 half: `session/resume` with an explicit `replayFrom: start`.
///
/// v2 made the replay opt-in, so omitting it would restore the session and
/// hand back no history at all — the one failure that looks like an empty
/// conversation rather than a broken read.
struct V2HistoryClient {
    state: Arc<HistoryState>,
    cwd: String,
    session_id: String,
}

impl ConnectTo<Agent> for V2HistoryClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let notifications = Arc::clone(&self.state);
        let answered = self.state;
        let cwd = self.cwd;
        let session_id = self.session_id;

        Client
            .v2()
            .on_receive_notification(
                async move |notification: v2::UpdateSessionNotification, _cx| {
                    notifications.on_v2(&notification.update);
                    Ok(())
                },
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |_request: v2::RequestPermissionRequest, responder, _cx| {
                    let refused = v1::RequestPermissionResponse::new(
                        v1::RequestPermissionOutcome::Cancelled,
                    );
                    let response = v2::conversion::try_v1_to_v2(refused).map_err(|error| {
                        Error::internal_error()
                            .data(format!("could not convert the refusal to v2: {error}"))
                    })?;
                    responder.respond(response)
                },
                on_receive_request!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("loom", env!("CARGO_PKG_VERSION")),
                    ))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V2 {
                    return Err(Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version for the \
                         history load",
                    ));
                }
                connection
                    .send_request(
                        v2::ResumeSessionRequest::new(session_id, cwd).replay_from(
                            v2::ReplayFrom::Start(v2::ReplayFromStart::new()),
                        ),
                    )
                    .block_task()
                    .await?;
                answered.answered();
                Ok(())
            })
            .await
    }
}

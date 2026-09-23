//! Listing an agent's own sessions, capability-gated.
//!
//! ACP makes enumeration optional: an agent advertises `session/list` in its
//! `initialize` response or it does not, and loom must respect that. This module
//! is the whole of loom's session-import surface, and its most important
//! property is what it does when the capability is absent:
//!
//! * **`Unsupported` is a distinct outcome, not an empty list.** "This agent
//!   cannot list its sessions" and "this agent has no sessions" are different
//!   facts, and collapsing them would make an unsupported capability look like
//!   an empty account. A client that shows an import picker can therefore say
//!   *why* it is empty.
//! * **Nothing is scanned.** loom never walks an agent's session directory, in
//!   any branch. That was the decision behind the ACP-only migration: the format
//!   knowledge lives in the adapter that already has it, and a second parser in
//!   loom would be a second thing to keep in step with each agent's storage. The
//!   file-reading code that exists is `pi-acp`'s, reached only through this ACP
//!   call.
//!
//! The result carries the agent's capabilities back with it, so a caller that
//! asks "can I import?" and a caller that asks "list them" need only one round
//! trip. See `docs/provider-sessions-research.md` for the design history and
//! `docs/acp-adapter.md` for the adapter boundary this sits on.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    InitializeRequest, ListSessionsRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse,
};
use agent_client_protocol::schema::{v2, ProtocolVersion};
use agent_client_protocol::{on_receive_request, Agent, Client, ConnectTo, ConnectionTo, Error};
use serde_json::Value;

use super::session::{agent_argv, embedded_agent_factory, Transport};

/// What an ACP agent says it can do, learned once from `initialize`.
///
/// Reported alongside a listing so the control plane can record it without a
/// second probe. An absent capability is absent: loom does not infer one from an
/// agent's name and does not look for the agent's storage instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentity {
    /// Stable programmatic name from the ACP initialize response.
    pub name: Option<String>,
    /// Human-readable title, when the agent supplies one.
    pub title: Option<String>,
    /// Agent implementation version, when supplied.
    pub version: Option<String>,
}

/// The ACP protocol version used for the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpProtocolVersion {
    /// Stable ACP v1.
    V1,
    /// Draft ACP v2.
    V2,
}

/// Capabilities and identity captured from one initialize response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// The version selected by protocol negotiation.
    pub protocol_version: AcpProtocolVersion,
    /// The agent identity returned by initialize.
    pub identity: AgentIdentity,
    /// The raw typed capability object, kept opaque to the domain/server.
    pub capability_snapshot: Value,
    /// The agent supports `session/load`, which restores a session and replays its history.
    pub load_session: bool,
    /// The agent supports `session/resume`, which restores a session without replaying history.
    pub resume_session: bool,
    /// The agent supports `session/list`. `false` means the import picker is
    /// empty **by capability**, and nothing is scanned to fill it.
    pub list_sessions: bool,
}

impl AgentCapabilities {
    fn v1(response: &agent_client_protocol::schema::v1::InitializeResponse) -> Self {
        let identity = response.agent_info.as_ref().map(identity_from_v1);
        Self {
            protocol_version: AcpProtocolVersion::V1,
            identity: identity.unwrap_or(AgentIdentity {
                name: None,
                title: None,
                version: None,
            }),
            capability_snapshot: serde_json::to_value(&response.agent_capabilities)
                .unwrap_or(Value::Null),
            load_session: response.agent_capabilities.load_session,
            resume_session: response
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some(),
            list_sessions: response
                .agent_capabilities
                .session_capabilities
                .list
                .is_some(),
        }
    }

    fn v2(response: &v2::InitializeResponse) -> Self {
        let session = response.capabilities.session.is_some();
        Self {
            protocol_version: AcpProtocolVersion::V2,
            identity: identity_from_v2(&response.info),
            capability_snapshot: serde_json::to_value(&response.capabilities)
                .unwrap_or(Value::Null),
            // v2 has one baseline session capability, including resume.
            load_session: session,
            resume_session: session,
            // v2 likewise defines session/list as a baseline session method;
            // there is no v1-style nested `list` flag in the draft schema.
            list_sessions: session,
        }
    }
}

fn identity_from_v1(info: &agent_client_protocol::schema::v1::Implementation) -> AgentIdentity {
    AgentIdentity {
        name: Some(info.name.clone()),
        title: info.title.clone(),
        version: Some(info.version.clone()),
    }
}

fn identity_from_v2(info: &v2::Implementation) -> AgentIdentity {
    AgentIdentity {
        name: Some(info.name.clone()),
        title: info.title.clone(),
        version: Some(info.version.clone()),
    }
}

/// One session an agent reports, ready to be bound to a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionInfo {
    /// The agent's own session id.
    pub session_id: String,
    /// The workspace the session was opened in.
    ///
    /// Carried because it is half of the identity a resume is keyed by: see
    /// [`loom_domain::ProviderSessionBinding`]. A session whose workspace loom
    /// does not know could not be resumed safely, and ACP declares the field
    /// required for exactly that reason.
    pub cwd: String,
    /// A human-readable title, when the agent has one.
    pub title: Option<String>,
}

/// The result of asking an agent for its sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionListOutcome {
    /// The agent advertises `session/list`, and these are its sessions.
    Listed {
        /// The capabilities the agent declared.
        capabilities: AgentCapabilities,
        /// The sessions, already filtered by the agent when a `cwd` was named.
        sessions: Vec<AgentSessionInfo>,
    },
    /// The agent does not advertise `session/list`.
    ///
    /// The caller must render this as "not supported", not as "no sessions", and
    /// must not go looking for the agent's files.
    Unsupported {
        /// The capabilities the agent did declare.
        capabilities: AgentCapabilities,
    },
    /// The probe could not run: a missing executable, a refused handshake, or a
    /// deadline.
    Failed {
        /// Why, verbatim, so it can be shown to a user.
        error: String,
    },
}

/// How many pages of `session/list` to follow before giving up.
///
/// A cursor that never clears would otherwise loop forever against an agent that
/// mishandles pagination. The bound is far above any realistic session count; a
/// page is the agent's own choice of size.
const MAX_SESSION_LIST_PAGES: usize = 200;

/// Asks an ACP agent to list its own sessions, if it can.
///
/// `cwd` is passed to the agent as ACP's own filter rather than applied here:
/// the agent is authoritative about how its storage maps to a workspace path,
/// and re-filtering the answer would second-guess it.
pub async fn list_sessions(
    transport: Transport,
    cwd: Option<String>,
    budget: Duration,
) -> SessionListOutcome {
    let fallback_cwd = cwd
        .clone()
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_owned());
    let operation = async {
        match transport {
            Transport::Stdio { command, args } => {
                let argv = agent_argv(&command, &args, &fallback_cwd);
                agent_client_protocol::AcpAgent::from_args(argv.clone())
                    .map_err(|error| format!("could not describe the ACP agent: {error}"))?;
                probe(
                    move || {
                        agent_client_protocol::AcpAgent::from_args(argv.clone())
                            .expect("validated ACP agent arguments")
                    },
                    cwd,
                )
                .await
            }
            // Only `pi` is a child here; the adapter itself is in-process, so
            // there is no argv to build for it. `pi-acp` resolves its own
            // command, which is why the same `EmbeddedPi` shape works without
            // the `sh -c` cwd wrapper the stdio path needs.
            Transport::EmbeddedPi { command, args } => {
                if !args.is_empty() {
                    return Err(format!(
                        "the embedded pi-acp transport takes no provider arguments, but the \
                         request supplies {args:?}"
                    ));
                }
                // No turn runs here, so there is nothing for the settle
                // fallback to bound.
                probe(
                    embedded_agent_factory(command, std::time::Duration::ZERO),
                    cwd,
                )
                .await
            }
        }
    };

    match tokio::time::timeout(budget, operation).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => SessionListOutcome::Failed { error },
        Err(_) => SessionListOutcome::Failed {
            error: format!(
                "the ACP agent did not answer the session listing within {}ms",
                budget.as_millis()
            ),
        },
    }
}

/// Runs the initialize-and-list conversation against a connected agent.
async fn probe<C, F>(agent_factory: F, cwd: Option<String>) -> Result<SessionListOutcome, String>
where
    C: agent_client_protocol::ConnectTo<Client>,
    F: FnMut() -> C + Send + 'static,
{
    let state = Arc::new(ProbeState::default());
    let result = Client
        .protocol_connector()
        .with_v1({
            let state = Arc::clone(&state);
            let cwd = cwd.clone();
            move || V1ListClient {
                state: Arc::clone(&state),
                cwd: cwd.clone(),
            }
        })
        .with_v2({
            let state = Arc::clone(&state);
            move || V2ListClient {
                state: Arc::clone(&state),
                cwd: cwd.clone(),
            }
        })
        .connect_to(agent_factory)
        .await;

    result.map_err(|error| format!("the ACP connection ended: {error}"))?;
    state
        .take()
        .ok_or_else(|| "the ACP agent ended without returning a session-list result".to_owned())
}

#[derive(Default)]
struct ProbeState {
    outcome: Mutex<Option<SessionListOutcome>>,
}

impl ProbeState {
    fn set(&self, outcome: SessionListOutcome) {
        *self.outcome.lock().expect("session probe result lock") = Some(outcome);
    }

    fn take(&self) -> Option<SessionListOutcome> {
        self.outcome
            .lock()
            .expect("session probe result lock")
            .take()
    }
}

struct V1ListClient {
    state: Arc<ProbeState>,
    cwd: Option<String>,
}

impl ConnectTo<Agent> for V1ListClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let state = self.state;
        let cwd = self.cwd;
        Client
            .builder()
            .on_receive_request(
                async move |_request: RequestPermissionRequest, responder, _cx| {
                    let _ = responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V1 {
                    return Err(Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version for the v1 probe",
                    ));
                }
                let capabilities = AgentCapabilities::v1(&initialized);
                let outcome = if capabilities.list_sessions {
                    SessionListOutcome::Listed {
                        capabilities: capabilities.clone(),
                        sessions: list_v1(&connection, cwd).await?,
                    }
                } else {
                    SessionListOutcome::Unsupported { capabilities }
                };
                state.set(outcome);
                Ok(())
            })
            .await
    }
}

struct V2ListClient {
    state: Arc<ProbeState>,
    cwd: Option<String>,
}

impl ConnectTo<Agent> for V2ListClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let state = self.state;
        let cwd = self.cwd;
        Client
            .v2()
            .on_receive_request(
                async move |_request: v2::RequestPermissionRequest, responder, _cx| {
                    let _ = responder.respond(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Cancelled,
                    ));
                    Ok(())
                },
                on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                let initialized = connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("loom", env!("CARGO_PKG_VERSION")),
                    ))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V2 {
                    return Err(Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version for the v2 probe",
                    ));
                }
                let capabilities = AgentCapabilities::v2(&initialized);
                let outcome = if capabilities.list_sessions {
                    SessionListOutcome::Listed {
                        capabilities: capabilities.clone(),
                        sessions: list_v2(&connection, cwd).await?,
                    }
                } else {
                    SessionListOutcome::Unsupported { capabilities }
                };
                state.set(outcome);
                Ok(())
            })
            .await
    }
}

async fn list_v1(
    connection: &ConnectionTo<Agent>,
    cwd: Option<String>,
) -> Result<Vec<AgentSessionInfo>, Error> {
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_SESSION_LIST_PAGES {
        let mut request = ListSessionsRequest::new();
        if let Some(cwd) = &cwd {
            request = request.cwd(std::path::PathBuf::from(cwd));
        }
        if let Some(cursor) = &cursor {
            request = request.cursor(cursor.clone());
        }
        let response = connection.send_request(request).block_task().await?;
        sessions.extend(response.sessions.into_iter().map(|info| AgentSessionInfo {
            session_id: info.session_id.0.to_string(),
            cwd: info.cwd.to_string_lossy().into_owned(),
            title: info.title,
        }));
        match response.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => return Ok(sessions),
        }
    }
    Err(Error::internal_error().data(format!(
        "the ACP agent kept returning a session-list cursor after {MAX_SESSION_LIST_PAGES} pages"
    )))
}

async fn list_v2(
    connection: &ConnectionTo<Agent>,
    cwd: Option<String>,
) -> Result<Vec<AgentSessionInfo>, Error> {
    let mut sessions = Vec::new();
    let mut cursor: Option<v2::SessionListCursor> = None;
    for _ in 0..MAX_SESSION_LIST_PAGES {
        let mut request = v2::ListSessionsRequest::new();
        if let Some(cwd) = &cwd {
            request = request.cwd(cwd.clone());
        }
        if let Some(cursor) = &cursor {
            request = request.cursor(cursor.clone());
        }
        let response = connection.send_request(request).block_task().await?;
        sessions.extend(response.sessions.into_iter().map(|info| AgentSessionInfo {
            session_id: info.session_id.0.to_string(),
            cwd: info.cwd.0.to_string_lossy().into_owned(),
            title: info.title,
        }));
        match response.next_cursor {
            Some(next) if !next.as_ref().is_empty() => cursor = Some(next),
            _ => return Ok(sessions),
        }
    }
    Err(Error::internal_error().data(format!(
        "the ACP agent kept returning a session-list cursor after {MAX_SESSION_LIST_PAGES} pages"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Writes an executable ACP agent stub and returns its path.
    ///
    /// `session_capabilities` is injected into the `initialize` result verbatim,
    /// so a test can advertise `session/list`, omit it, or advertise something
    /// else. `list_body` is the `session/list` shell body; an empty one leaves
    /// the method unanswered, which is what the deadline test wants.
    ///
    /// The template is substituted with `str::replace` rather than built with
    /// `format!`: the shell fragment is full of JSON braces, which `format!`
    /// would read as placeholders.
    fn write_agent(dir: &Path, name: &str, session_capabilities: &str, list_body: &str) -> PathBuf {
        /// A shell fragment that answers each JSON-RPC method by name. The
        /// `%SESSION%` and `%LIST%` markers are substituted below.
        const TEMPLATE: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s
' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true%SESSION%}}}'
      ;;
%LIST%  esac
done
"#;
        let path = dir.join(name);
        // An absent capability means the key is absent, which is what the ACP
        // schema's `Option` reads as "does not support".
        let session = if session_capabilities.is_empty() {
            String::new()
        } else {
            format!(r#","sessionCapabilities":{session_capabilities}"#)
        };
        // An empty body answers nothing, so `session/list` would hang — which is
        // what the deadline test is for.
        let list = if list_body.is_empty() {
            String::new()
        } else {
            format!(
                "    session/list)
{list_body}
      ;;
"
            )
        };
        let script = TEMPLATE
            .replace("%SESSION%", &session)
            .replace("%LIST%", &list);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn write_v2_agent(dir: &Path) -> PathBuf {
        let path = dir.join("v2-list.sh");
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":2,"info":{"name":"fake-v2","version":"1"},"capabilities":{"session":{}}}}'
      ;;
    session/list)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessions":[{"sessionId":"v2-session","cwd":"/srv/v2","title":"v2 test"}]}}'
      ;;
  esac
done
"#;
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn stdio(agent: &Path) -> Transport {
        Transport::Stdio {
            command: agent.to_string_lossy().into_owned(),
            args: Vec::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_without_the_capability_is_reported_as_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        // No `sessionCapabilities` at all: the agent cannot list, and no scan
        // happens in its place.
        let agent = write_agent(dir.path(), "no-list.sh", "", "");
        let outcome = list_sessions(stdio(&agent), None, Duration::from_secs(10)).await;
        match outcome {
            SessionListOutcome::Unsupported { capabilities } => {
                assert!(!capabilities.list_sessions);
                assert!(capabilities.load_session, "load is still advertised");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_capable_agent_lists_its_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let agent = write_agent(
            dir.path(),
            "list.sh",
            r#"{"list":{}}"#,
            r#"      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessions":[{"sessionId":"acp-1","cwd":"/srv/project-a","title":"fix the test"},{"sessionId":"acp-2","cwd":"/srv/project-b"}]}}'"#,
        );
        let outcome = list_sessions(stdio(&agent), None, Duration::from_secs(10)).await;
        match outcome {
            SessionListOutcome::Listed {
                capabilities,
                sessions,
            } => {
                assert!(capabilities.list_sessions);
                assert_eq!(sessions.len(), 2);
                assert_eq!(sessions[0].session_id, "acp-1");
                assert_eq!(sessions[0].cwd, "/srv/project-a");
                assert_eq!(sessions[0].title.as_deref(), Some("fix the test"));
                assert_eq!(sessions[1].title, None);
            }
            other => panic!("expected Listed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cursor_is_followed_until_it_clears() {
        let dir = tempfile::tempdir().unwrap();
        // The first page names a cursor, the second clears it. A probe that read
        // only the first page would report one session.
        let agent = write_agent(
            dir.path(),
            "paged.sh",
            r#"{"list":{}}"#,
            r#"      if [ -f "$0.first" ]; then
  printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessions":[{"sessionId":"acp-2","cwd":"/srv/b"}]}}'
else
  touch "$0.first"
  printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessions":[{"sessionId":"acp-1","cwd":"/srv/a"}],"nextCursor":"page-2"}}'
fi"#,
        );
        let outcome = list_sessions(stdio(&agent), None, Duration::from_secs(10)).await;
        match outcome {
            SessionListOutcome::Listed { sessions, .. } => {
                assert_eq!(
                    sessions
                        .iter()
                        .map(|s| s.session_id.as_str())
                        .collect::<Vec<_>>(),
                    vec!["acp-1", "acp-2"],
                    "every page is followed"
                );
            }
            other => panic!("expected Listed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_v2_agent_is_negotiated_and_lists_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let agent = write_v2_agent(dir.path());
        let outcome = list_sessions(stdio(&agent), None, Duration::from_secs(10)).await;
        match outcome {
            SessionListOutcome::Listed {
                capabilities,
                sessions,
            } => {
                assert_eq!(capabilities.protocol_version, AcpProtocolVersion::V2);
                assert_eq!(capabilities.identity.name.as_deref(), Some("fake-v2"));
                assert!(capabilities.load_session);
                assert!(capabilities.list_sessions);
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session_id, "v2-session");
                assert_eq!(sessions[0].cwd, "/srv/v2");
                assert_eq!(sessions[0].title.as_deref(), Some("v2 test"));
            }
            other => panic!("expected a v2 session listing, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_executable_is_reported_not_treated_as_empty() {
        let outcome = list_sessions(
            Transport::Stdio {
                command: "/nonexistent/acp-agent".into(),
                args: Vec::new(),
            },
            None,
            Duration::from_secs(10),
        )
        .await;
        assert!(
            matches!(outcome, SessionListOutcome::Failed { .. }),
            "a probe that could not run must say so: {outcome:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_probe_is_bounded_by_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        // It *advertises* listing but never answers, which is the wedged-agent
        // case the budget exists for. An agent that does not advertise the
        // capability never reaches the deadline at all.
        let agent = write_agent(dir.path(), "slow.sh", r#"{"list":{}}"#, "");
        // An agent that never answers must not hang the caller forever.
        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            list_sessions(stdio(&agent), None, Duration::from_millis(50)),
        )
        .await
        .expect("the probe must return within its budget");
        assert!(
            matches!(outcome, SessionListOutcome::Failed { .. }),
            "a deadline is a failure, never an empty list: {outcome:?}"
        );
    }
}

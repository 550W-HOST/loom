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

use std::time::Duration;

use agent_client_protocol::schema::v1::{
    InitializeRequest, ListSessionsRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{on_receive_request, Client, ConnectionTo};

use super::session::{agent_argv, Transport};

/// What an ACP agent says it can do, learned once from `initialize`.
///
/// Reported alongside a listing so the control plane can record it without a
/// second probe. An absent capability is absent: loom does not infer one from an
/// agent's name and does not look for the agent's storage instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// The agent supports `session/load`, so a thread's known session id can be
    /// resumed. This is what makes a second turn continue the first.
    pub load_session: bool,
    /// The agent supports `session/list`. `false` means the import picker is
    /// empty **by capability**, and nothing is scanned to fill it.
    pub list_sessions: bool,
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
                let agent = agent_client_protocol::AcpAgent::from_args(agent_argv(
                    &command,
                    &args,
                    &fallback_cwd,
                ))
                .map_err(|error| format!("could not describe the ACP agent: {error}"))?;
                probe(agent, cwd).await
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
                let mut config = pi_acp::config::Config::default();
                if !command.is_empty() {
                    config.pi_command = command;
                }
                let agent = std::sync::Arc::new(pi_acp::agent::AcpAgent::new(config));
                let (adapter_side, client_side) = agent_client_protocol::Channel::duplex();
                let mut running = AbortOnDrop(tokio::spawn(async move {
                    let _ = agent.run_with(adapter_side).await;
                }));
                let outcome = probe(client_side, cwd).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut running.0).await;
                outcome
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
async fn probe(
    agent: impl agent_client_protocol::ConnectTo<Client> + 'static,
    cwd: Option<String>,
) -> Result<SessionListOutcome, String> {
    // A listing is not a run, so there is no thread to record an interaction
    // against and no client that could answer one. Cancelling is therefore the
    // only truthful reply to a permission request here — and it must never be an
    // allow, which is what the previous auto-allow policy got wrong.
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
        .connect_with(
            agent,
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V1 {
                    return Err(agent_client_protocol::Error::internal_error().data(
                        "the ACP agent negotiated an unsupported protocol version; loom currently \
                         supports v1",
                    ));
                }
                let capabilities = AgentCapabilities {
                    load_session: initialized.agent_capabilities.load_session,
                    // The capability is an `Option`: `Some` means the agent
                    // supports listing, `None`/absent means it does not. An
                    // agent that cannot list gets no scan in its place.
                    list_sessions: initialized
                        .agent_capabilities
                        .session_capabilities
                        .list
                        .is_some(),
                };
                if !capabilities.list_sessions {
                    return Ok(SessionListOutcome::Unsupported { capabilities });
                }

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
                        _ => {
                            return Ok(SessionListOutcome::Listed {
                                capabilities,
                                sessions,
                            })
                        }
                    }
                }
                Err(agent_client_protocol::Error::internal_error().data(format!(
                    "the ACP agent kept returning a session-list cursor after \
                     {MAX_SESSION_LIST_PAGES} pages"
                )))
            },
        )
        .await
        // `connect_with` returns the closure's own `R` flat, so the only `Err`
        // here is a connection-level failure; a listing problem was turned into
        // `SessionListOutcome::Failed` inside the closure, where it could name
        // what went wrong.
        .map_err(|error| format!("the ACP connection ended: {error}"))
}

/// Aborts an embedded adapter if the probe times out.
///
/// Dropping a bare `JoinHandle` would detach `pi-acp` and leave its child alive
/// after the daemon had given up on the answer.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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

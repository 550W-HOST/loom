//! The provider execution contract: what the control plane dispatches to a
//! daemon, and what a daemon reports back.
//!
//! This is loom's answer to bb's `packages/host-daemon-contract`, scoped to the
//! one thing that matters here: **running a provider for a thread**. It is a
//! plain-data crate with no runtime, no IO and no relay dependency, so both the
//! Rust control plane and any future Node execution plane can depend on it
//! without dragging a runtime along.
//!
//! # The two directions
//!
//! ```text
//!   server ── RunDispatch ──▶ relay host:{id} ──▶ daemon
//!   server ◀── ProviderReport ── daemon socket
//! ```
//!
//! * [`RunDispatch`] travels **through the relay**, published to the target
//!   host's scope. That is what gives dispatch replay: a daemon that was
//!   disconnected while a run was dispatched receives it on reconnect. The
//!   `run_id` is the idempotency key, so a redelivery of a run the daemon
//!   already started is dropped rather than run twice.
//! * [`ProviderReport`] travels **up the daemon's own socket**. A report is an
//!   observation, not a command: the server turns it into a
//!   [`loom_domain::DomainEvent::ThreadRunEvent`] and publishes it to the thread
//!   scope through the relay, so it is replayable like any other event.
//!
//! # Why the payload is opaque
//!
//! A dispatch carries only what a provider process needs. Everything a provider
//! CLI emits is normalised by the daemon into [`RunEvent`] before it is
//! reported; the control plane never parses provider output. That is what keeps
//! a provider's stdout format from leaking into the control plane — the class
//! of coupling that wedged bb turns.
//!
//! [`RunEvent`]: loom_domain::RunEvent

use loom_domain::{EnvironmentId, HostId, ProjectId, RunEvent, RunId, ThreadId};
use serde::{Deserialize, Serialize};

/// How to start a provider process.
///
/// Deliberately not a plugin descriptor: there is no manifest, no lifecycle
/// hooks and no discovery. A provider is "a command the daemon can spawn",
/// which is the whole first-class story.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSpec {
    /// Stable provider name, for example `"pi"`. Echoed in run events.
    pub name: String,
    /// How the daemon should reach this agent.
    ///
    /// Defaults to [`ProviderLaunch::JsonRpc`] so an existing dispatch keeps
    /// its meaning: the field is additive on the wire, and a daemon that does
    /// not know a launch kind refuses rather than guessing at one.
    #[serde(default)]
    pub launch: ProviderLaunch,
    /// The executable to spawn.
    pub command: String,
    /// Arguments, in order.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the provider, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl ProviderSpec {
    /// The Pi provider, driven through its JSON-RPC stdin/stdout mode.
    ///
    /// `pi --mode rpc` is headless, sessionless and line-delimited JSON in both
    /// directions; see the Pi `docs/rpc.md`. The daemon writes a `prompt`
    /// command and consumes streamed events, so nothing here is Pi-specific
    /// beyond the argv.
    pub fn pi() -> Self {
        Self {
            name: "pi".into(),
            launch: ProviderLaunch::JsonRpc,
            command: "pi".into(),
            args: vec!["--mode".into(), "rpc".into(), "--no-session".into()],
            cwd: None,
        }
    }

    /// An agent reached over ACP, spawned as a child process.
    pub fn acp(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: "acp".into(),
            launch: ProviderLaunch::AcpStdio,
            command: command.into(),
            args,
            cwd: None,
        }
    }

    /// Pi reached through `pi-acp` linked into the daemon.
    ///
    /// Only `pi` itself is a child process; the adapter is in-process, which is
    /// why this names no command.
    pub fn acp_pi() -> Self {
        Self {
            name: "pi".into(),
            launch: ProviderLaunch::AcpEmbeddedPi,
            command: "pi".into(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// An arbitrary command, used by tests and by operators overriding the
    /// provider on a machine (`LOOM_PROVIDER_CMD`).
    pub fn custom(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: "custom".into(),
            launch: ProviderLaunch::JsonRpc,
            command: command.into(),
            args,
            cwd: None,
        }
    }

    /// The program plus arguments, as argv.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.command.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

impl Default for ProviderSpec {
    fn default() -> Self {
        Self::pi()
    }
}

/// How an agent is driven.
///
/// loom's provider strategy is ACP, so this exists to say *which* agent is
/// being reached rather than to offer a choice of protocols. The JSON-RPC arm
/// is the Pi path as it stands today, kept so an existing dispatch keeps
/// working while the ACP path replaces it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLaunch {
    /// Pi's own line-delimited JSON-RPC over stdio, as loom drives it today.
    #[default]
    JsonRpc,
    /// Agent Client Protocol over stdio, for an agent that speaks it natively.
    AcpStdio,
    /// Agent Client Protocol against `pi-acp` linked into the daemon.
    ///
    /// Nothing is spawned for the adapter itself; only `pi` is a child.
    AcpEmbeddedPi,
}

/// A request to run one provider turn for one thread.
///
/// Published to `host:{host_id}` through the relay. Every field the daemon
/// needs to run in isolation is present: the daemon never has to call back for
/// context before it can start, which keeps the control plane out of the
/// dispatch path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDispatch {
    /// Idempotency key and the run's identity.
    pub run_id: RunId,
    /// The thread being advanced.
    pub thread_id: ThreadId,
    /// Its project, carried so events need no lookup.
    pub project_id: ProjectId,
    /// The host expected to execute it.
    pub host_id: HostId,
    /// The user turn that started the run.
    pub prompt: String,
    /// How to start the provider.
    pub provider: ProviderSpec,
    /// Wall-clock milliseconds by which the run must have a terminal event.
    pub deadline_ms: u64,
    /// When the control plane minted the dispatch.
    pub created_at_ms: u64,
}

/// A request to provision a managed environment's workspace on a host.
///
/// Like [`RunDispatch`] this travels **through the relay**, published to the
/// target host's scope, so a daemon that was disconnected while it was sent
/// still receives it on reconnect. The daemon owns the directory layout and
/// chooses the actual path under its configured workspace root; the control
/// plane only learns it from [`EnvironmentProvisionReport`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProvision {
    /// The environment to provision.
    pub environment_id: EnvironmentId,
    /// Its project, carried so a report needs no lookup.
    pub project_id: ProjectId,
    /// The host expected to provision it.
    pub host_id: HostId,
    /// Wall-clock milliseconds when the control plane minted the request.
    pub created_at_ms: u64,
}

/// What a daemon did with an [`EnvironmentProvision`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EnvironmentProvisionOutcome {
    /// The workspace exists and is usable at `path`.
    Provisioned {
        /// Absolute path the daemon created.
        path: String,
    },
    /// Provisioning failed; the environment moves to `error`.
    Failed {
        /// Why, verbatim, so it can be shown to a user.
        error: String,
    },
}

/// A daemon's report about one provisioning attempt.
///
/// Sent up the daemon's own socket, exactly like [`ProviderReport`]: the
/// server turns it into environment status events and publishes them to the
/// project scope through the relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProvisionReport {
    /// The host making the report.
    pub host_id: HostId,
    /// The environment being provisioned.
    pub environment_id: EnvironmentId,
    /// What happened.
    pub outcome: EnvironmentProvisionOutcome,
}

/// A daemon's observation about an in-flight run.
///
/// `host_id` is what lets the server reject a report for a run this connection
/// is not allowed to speak for; it must match the host the socket enrolled as.
/// The [`RunEvent`] already carries the run and thread identity (and the
/// bb-contract event), so those are not repeated here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderReport {
    /// The host making the report.
    pub host_id: HostId,
    /// What happened.
    pub event: RunEvent,
}

/// A line read from a provider's stdout, after the guard.
///
/// Providers are not required to keep stdout clean. bb's #1180 is the canonical
/// failure: Pi emitted an OSC 777 desktop notification on stdout, the bridge
/// fed it to a JSON parser, and the thread was stuck in `working` forever.
/// Every byte a provider writes therefore goes through [`guard_stdout_line`]
/// first, and only [`GuardedLine::Frame`] ever reaches the event mapper.
#[derive(Clone, Debug, PartialEq)]
pub enum GuardedLine {
    /// A JSON object with a string `type` — a protocol frame.
    Frame(serde_json::Value),
    /// Anything else: a log line, a stray escape sequence, a partial write.
    /// The caller routes it to stderr, never to the frame parser.
    Stray(String),
}

/// Classifies one stdout line.
///
/// The rule is intentionally strict, because a false positive is what wedges a
/// turn: a line is a frame only when it is a JSON *object* carrying a string
/// `type`. JSON scalars, arrays and objects without `type` are stray.
pub fn guard_stdout_line(line: &str) -> GuardedLine {
    let trimmed = line.strip_suffix('\r').unwrap_or(line);
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value @ serde_json::Value::Object(_))
            if value.get("type").is_some_and(|t| t.is_string()) =>
        {
            GuardedLine::Frame(value)
        }
        _ => GuardedLine::Stray(trimmed.to_owned()),
    }
}

/// Whether a line is a protocol frame. A convenience for tests and logs.
pub fn is_protocol_frame(line: &str) -> bool {
    matches!(guard_stdout_line(line), GuardedLine::Frame(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::RunEvent;

    fn sample_dispatch() -> RunDispatch {
        RunDispatch {
            run_id: RunId::mint(),
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            host_id: HostId::mint(),
            prompt: "hello".into(),
            provider: ProviderSpec::pi(),
            deadline_ms: 12,
            created_at_ms: 1,
        }
    }

    #[test]
    fn a_dispatch_round_trips() {
        let dispatch = sample_dispatch();
        let encoded = serde_json::to_string(&dispatch).unwrap();
        assert_eq!(
            serde_json::from_str::<RunDispatch>(&encoded).unwrap(),
            dispatch
        );
    }

    #[test]
    fn a_report_round_trips() {
        let report = ProviderReport {
            host_id: HostId::mint(),
            event: RunEvent::new(
                ThreadId::mint(),
                ProjectId::mint(),
                RunId::mint(),
                1,
                loom_domain::ProviderEvent::ProviderWarning {
                    provider_thread_id: "p".into(),
                    category: loom_domain::ProviderWarningCategory::General,
                    summary: Some("careful".into()),
                    details: None,
                },
            ),
        };
        let encoded = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderReport>(&encoded).unwrap(),
            report
        );
    }

    #[test]
    fn the_pi_spec_is_headless_rpc() {
        let spec = ProviderSpec::pi();
        assert_eq!(spec.command, "pi");
        assert_eq!(spec.argv(), ["pi", "--mode", "rpc", "--no-session"]);
    }

    #[test]
    fn the_guard_accepts_a_protocol_object() {
        assert_eq!(
            guard_stdout_line(r#"{"type":"agent_settled"}"#),
            GuardedLine::Frame(serde_json::json!({"type": "agent_settled"}))
        );
    }

    #[test]
    fn the_guard_diverts_pollution_to_stderr() {
        // The exact class of bb #1180: an OSC 777 notification on stdout.
        let osc = "\u{1b}]777;notify;pi;done\u{7}";
        assert_eq!(guard_stdout_line(osc), GuardedLine::Stray(osc.into()));
        assert!(!is_protocol_frame(osc));

        // Log lines, JSON scalars, arrays and objects without a type are all
        // stray, not frames.
        for stray in [
            "warning: something happened",
            "42",
            "[1,2,3]",
            r#"{"message":"no type"}"#,
            r#"{"type":7}"#,
            "",
        ] {
            assert_eq!(guard_stdout_line(stray), GuardedLine::Stray(stray.into()));
        }
    }

    #[test]
    fn the_guard_tolerates_a_crlf_terminator() {
        assert!(is_protocol_frame("{\"type\":\"agent_start\"}\r"));
    }

    #[test]
    fn an_environment_provision_round_trips_both_outcomes() {
        let provision = EnvironmentProvision {
            environment_id: EnvironmentId::mint(),
            project_id: ProjectId::mint(),
            host_id: HostId::mint(),
            created_at_ms: 7,
        };
        let encoded = serde_json::to_string(&provision).unwrap();
        assert_eq!(
            serde_json::from_str::<EnvironmentProvision>(&encoded).unwrap(),
            provision
        );

        let ok = EnvironmentProvisionReport {
            host_id: HostId::mint(),
            environment_id: EnvironmentId::mint(),
            outcome: EnvironmentProvisionOutcome::Provisioned {
                path: "/srv/loom".into(),
            },
        };
        let value = serde_json::to_value(&ok).unwrap();
        assert_eq!(value["outcome"]["outcome"], "provisioned");
        assert_eq!(
            serde_json::from_str::<EnvironmentProvisionReport>(
                &serde_json::to_string(&ok).unwrap()
            )
            .unwrap(),
            ok
        );

        let failed = EnvironmentProvisionReport {
            host_id: HostId::mint(),
            environment_id: EnvironmentId::mint(),
            outcome: EnvironmentProvisionOutcome::Failed {
                error: "permission denied".into(),
            },
        };
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["outcome"]["outcome"], "failed");
        assert_eq!(value["outcome"]["error"], "permission denied");
    }

    #[test]
    fn a_finished_report_carries_the_outcome() {
        let report = ProviderReport {
            host_id: HostId::mint(),
            event: RunEvent::failed(
                ThreadId::mint(),
                ProjectId::mint(),
                RunId::mint(),
                1,
                loom_domain::TurnStatus::Failed,
                "exit 1",
            ),
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["event"]["event"]["type"], "turn/completed");
        assert_eq!(value["event"]["event"]["status"], "failed");
        assert_eq!(value["event"]["event"]["error"]["message"], "exit 1");
    }
}

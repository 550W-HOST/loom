//! A provider run: the lifecycle and the streamed detail of one agent turn.
//!
//! A *run* is what happens between a thread leaving `idle` and returning to a
//! terminal status. It is deliberately separate from [`Thread`](crate::Thread):
//! the thread is the durable conversation, the run is one attempt at advancing
//! it. A failed run leaves the thread in `error`, which a retry turns into a
//! *new* run with a new [`RunId`](crate::RunId).
//!
//! Every run produces a stream of [`RunEvent`]s. They are published to the
//! thread scope in order, so a replaying client reconstructs the same timeline
//! a live subscriber saw. The stream always ends in exactly one
//! [`RunEvent::Finished`]: that is the invariant that keeps a thread from being
//! stuck in `working` forever after a provider crash, a daemon that vanished
//! or a timeout.

use serde::{Deserialize, Serialize};

/// Which provider output stream a chunk belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// The provider's user-visible answer text.
    Assistant,
    /// The provider's reasoning/thinking text, when it exposes it.
    Thinking,
    /// Diagnostics a provider chose to send inside its protocol.
    Log,
}

/// A phase of the provider's turn loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// Freshly spawned, before the first turn.
    Started,
    /// A turn (assistant response plus its tool calls) began.
    Began,
    /// A turn completed.
    Ended,
}

/// How a run ended. Exactly one of these is always produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The provider settled normally.
    Completed,
    /// The provider process exited non-zero or reported a protocol failure.
    Failed,
    /// The run exceeded its deadline and was killed.
    TimedOut,
    /// The daemon holding the run stopped heartbeating; the run was reaped.
    HostStale,
    /// The operator or a client cancelled the run.
    Cancelled,
}

impl RunOutcome {
    /// Whether the thread should return to `idle` (vs. `error`) afterwards.
    pub fn is_success(self) -> bool {
        matches!(self, RunOutcome::Completed)
    }

    /// The stable wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Failed => "failed",
            RunOutcome::TimedOut => "timed_out",
            RunOutcome::HostStale => "host_stale",
            RunOutcome::Cancelled => "cancelled",
        }
    }
}

/// Severity of a [`RunEvent::Notice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Informational.
    Info,
    /// Worth showing, not fatal.
    Warning,
    /// Fatal or near-fatal.
    Error,
}

/// One fact about an in-flight provider run.
///
/// The variant set is the execution plane's vocabulary. It is intentionally
/// narrow: anything a provider CLI emits that is not one of these is provider
/// noise, and the daemon's stdout guard keeps noise out of the stream rather
/// than inventing an event for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// The provider process is up and the run has started.
    Started {
        /// The provider name, as the control plane asked for it.
        provider: String,
    },
    /// A chunk of streamed provider output.
    Output {
        /// Which stream the chunk belongs to.
        stream: OutputStream,
        /// The chunk text.
        text: String,
    },
    /// The provider began a tool call.
    ToolCall {
        /// Provider-assigned call id, used to match the result.
        tool_call_id: String,
        /// Tool name.
        name: String,
        /// Tool arguments, opaque to loom.
        args: serde_json::Value,
    },
    /// A tool call finished.
    ToolResult {
        /// The call id this result answers.
        tool_call_id: String,
        /// Tool name.
        name: String,
        /// Whether the tool reported failure.
        ok: bool,
        /// The result text.
        output: String,
    },
    /// The provider moved through a turn boundary.
    Turn {
        /// Which boundary.
        phase: TurnPhase,
    },
    /// Anything the provider reported that is not output, tool or turn.
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// The message, verbatim.
        message: String,
    },
    /// The run stopped. Always the last event of a run.
    Finished {
        /// How it ended.
        outcome: RunOutcome,
        /// A human-readable reason, when the outcome is not `completed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl RunEvent {
    /// The stable `type` tag, matching the serialized form.
    pub fn kind(&self) -> &'static str {
        match self {
            RunEvent::Started { .. } => "started",
            RunEvent::Output { .. } => "output",
            RunEvent::ToolCall { .. } => "tool_call",
            RunEvent::ToolResult { .. } => "tool_result",
            RunEvent::Turn { .. } => "turn",
            RunEvent::Notice { .. } => "notice",
            RunEvent::Finished { .. } => "finished",
        }
    }

    /// Whether this event terminates the run.
    pub fn is_terminal(&self) -> bool {
        matches!(self, RunEvent::Finished { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_serializes_with_its_type_tag() {
        let events = [
            RunEvent::Started {
                provider: "pi".into(),
            },
            RunEvent::Output {
                stream: OutputStream::Assistant,
                text: "hi".into(),
            },
            RunEvent::ToolCall {
                tool_call_id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
            },
            RunEvent::ToolResult {
                tool_call_id: "c1".into(),
                name: "bash".into(),
                ok: true,
                output: ".".into(),
            },
            RunEvent::Turn {
                phase: TurnPhase::Ended,
            },
            RunEvent::Notice {
                level: NoticeLevel::Warning,
                message: "heads up".into(),
            },
            RunEvent::Finished {
                outcome: RunOutcome::Completed,
                error: None,
            },
        ];
        for event in events {
            assert_eq!(serde_json::to_value(&event).unwrap()["type"], event.kind());
        }
    }

    #[test]
    fn a_terminal_event_is_the_thenable_one() {
        assert!(!RunEvent::Started {
            provider: "pi".into()
        }
        .is_terminal());
        assert!(RunEvent::Finished {
            outcome: RunOutcome::Failed,
            error: Some("boom".into())
        }
        .is_terminal());
    }

    #[test]
    fn an_optional_error_is_omitted_when_absent() {
        let value = serde_json::to_value(RunEvent::Finished {
            outcome: RunOutcome::Completed,
            error: None,
        })
        .unwrap();
        assert!(value.get("error").is_none());

        let value = serde_json::to_value(RunEvent::Finished {
            outcome: RunOutcome::Failed,
            error: Some("boom".into()),
        })
        .unwrap();
        assert_eq!(value["error"], "boom");
    }
}

//! The domain's error type.
//!
//! Every rejection a [`crate::Thread`], a [`crate::Environment`] or an
//! identifier can produce is one variant here. There is deliberately no IO
//! error: this crate has no IO to fail at.

use std::fmt;

use crate::environment::EnvironmentStatus;
use crate::thread::{ThreadStatus, ThreadTrigger};

/// A rejected domain operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainError {
    /// The string is not `<prefix>_<26-char Crockford base32>`.
    MalformedId {
        /// The entity kind whose identifier was expected.
        expected: &'static str,
        /// The rejected input, kept verbatim for the message.
        value: String,
    },
    /// A lifecycle trigger is not legal from the current thread status.
    IllegalThreadTransition {
        /// The status the thread was in.
        from: ThreadStatus,
        /// The trigger that had no transition.
        trigger: ThreadTrigger,
    },
    /// An environment status change is not a legal transition.
    IllegalEnvironmentTransition {
        /// The status the environment was in.
        from: EnvironmentStatus,
        /// The status that was requested.
        to: EnvironmentStatus,
    },
    /// An archived entity is read-only.
    Archived {
        /// The entity kind, e.g. `"thread"`.
        entity: &'static str,
    },
    /// A required field was empty or otherwise unusable.
    InvalidField {
        /// The field name.
        field: &'static str,
        /// Why it was rejected.
        reason: String,
    },
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::MalformedId { expected, value } => {
                write!(f, "not a valid {expected} id: {value:?}")
            }
            DomainError::IllegalThreadTransition { from, trigger } => {
                write!(f, "a thread in status {from} cannot accept {trigger}")
            }
            DomainError::IllegalEnvironmentTransition { from, to } => {
                write!(f, "an environment cannot go from {from} to {to}")
            }
            DomainError::Archived { entity } => write!(f, "the {entity} is archived"),
            DomainError::InvalidField { field, reason } => write!(f, "field {field}: {reason}"),
        }
    }
}

impl std::error::Error for DomainError {}

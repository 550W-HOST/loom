//! Sidebar sections.
//!
//! A section is a named group a client files threads under. loom stores the
//! grouping; it does not decide what belongs in it. The relationship is
//! deliberately **one-sided**: [`Thread::section_id`](crate::Thread::section_id)
//! holds the section's id as an opaque string, and nothing enforces that the
//! section still exists.
//!
//! # Why deleting a section does not rewrite its threads
//!
//! bb's `threadSections.delete` answers with the number of threads its deletion
//! touched (`updatedThreadCount`). loom reports the count of threads that
//! *referenced* the section, but does not rewrite them: the thread's stored
//! grouping is the client's last write, and a delete that silently
//! re-filed every thread would be an unrequested mutation of a different
//! entity. A client that wants its threads back in the default group sends
//! `threads.update` itself, which is the only path that already owns that
//! field. See `docs/projects.md`.
//!
//! Section names are **unique case-sensitively after trimming**. That is what
//! makes `threadSections.create`'s `409` meaningful: creating a section whose
//! name already exists is the conflict the contract declares, rather than two
//! rows a client cannot tell apart.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::event::DomainEvent;
use crate::id::ThreadSectionId;

/// The longest a section name may be.
///
/// A bound rather than none, for the same reason every other client-supplied
/// string has one: a name is stored, echoed to every client in the sidebar
/// bootstrap, and rendered.
pub const MAX_SECTION_NAME_LEN: usize = 128;

/// A named sidebar group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSection {
    /// Identity, `sec_…`.
    pub id: ThreadSectionId,
    /// Display name, unique among sections.
    pub name: String,
    /// Wall-clock milliseconds when the section was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds of the last rename.
    pub updated_at_ms: u64,
}

impl ThreadSection {
    /// Creates a section, validating its name, and returns the event.
    pub fn create(
        name: impl Into<String>,
        now_ms: u64,
    ) -> Result<(Self, DomainEvent), DomainError> {
        let name = normalise_name(name)?;
        let section = Self {
            id: ThreadSectionId::mint(),
            name,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = DomainEvent::ThreadSectionCreated {
            section: section.clone(),
        };
        Ok((section, event))
    }

    /// Renames the section and returns the update event.
    ///
    /// Renaming to the current name is an idempotent success that still
    /// announces the new value: the client asked for a state the section is
    /// already in, and answering with the row it already has is the truthful
    /// reply.
    pub fn rename(
        &mut self,
        name: impl Into<String>,
        now_ms: u64,
    ) -> Result<DomainEvent, DomainError> {
        self.name = normalise_name(name)?;
        self.updated_at_ms = now_ms;
        Ok(DomainEvent::ThreadSectionUpdated {
            section: self.clone(),
        })
    }
}

/// Trims a name and rejects an empty or over-long one.
fn normalise_name(name: impl Into<String>) -> Result<String, DomainError> {
    let name = name.into().trim().to_owned();
    if name.is_empty() {
        return Err(DomainError::InvalidField {
            field: "name",
            reason: "must not be empty".into(),
        });
    }
    if name.chars().count() > MAX_SECTION_NAME_LEN {
        return Err(DomainError::InvalidField {
            field: "name",
            reason: format!("must be at most {MAX_SECTION_NAME_LEN} characters"),
        });
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_is_created_with_a_trimmed_name() {
        let (section, event) = ThreadSection::create("  Backlog  ", 7).unwrap();
        assert_eq!(section.name, "Backlog");
        assert_eq!(section.created_at_ms, 7);
        assert_eq!(section.updated_at_ms, 7);
        assert!(matches!(event, DomainEvent::ThreadSectionCreated { .. }));
        assert_eq!(event.scope(), crate::DomainScope::Global);
    }

    #[test]
    fn an_empty_or_blank_name_is_rejected() {
        for name in ["", "   ", "\t\n"] {
            assert!(matches!(
                ThreadSection::create(name, 1),
                Err(DomainError::InvalidField { field: "name", .. })
            ));
        }
        let (mut section, _) = ThreadSection::create("ok", 1).unwrap();
        assert!(section.rename("  ", 2).is_err());
        // A rejected rename must not have moved the name or the timestamp.
        assert_eq!(section.name, "ok");
        assert_eq!(section.updated_at_ms, 1);
    }

    #[test]
    fn an_over_long_name_is_rejected_by_characters_not_bytes() {
        let long = "é".repeat(MAX_SECTION_NAME_LEN + 1);
        assert!(ThreadSection::create(long, 1).is_err());
        let exact = "é".repeat(MAX_SECTION_NAME_LEN);
        assert!(ThreadSection::create(exact, 1).is_ok());
    }

    #[test]
    fn renaming_emits_an_update() {
        let (mut section, _) = ThreadSection::create("a", 1).unwrap();
        let event = section.rename("b", 2).unwrap();
        assert_eq!(section.name, "b");
        assert_eq!(section.updated_at_ms, 2);
        assert!(matches!(event, DomainEvent::ThreadSectionUpdated { .. }));
        assert_eq!(event.scope(), crate::DomainScope::Global);
    }
}

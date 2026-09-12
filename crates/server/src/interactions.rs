//! Interaction lifecycle: recording a provider's question and delivering the
//! answer.
//!
//! This mirrors [`crate::runs`] in shape, and the parallel is the point: a
//! provider's request for input is *observed* by the daemon on the same stream
//! as everything else, and the control plane turns the observation into a
//! durable entity exactly as it turns a terminal frame into a thread status
//! change.
//!
//! # Why the control plane owns this at all
//!
//! A question is not a runtime concern, because the answer has to survive a
//! disconnect: the daemon may report a question, lose its socket, and only
//! receive the answer after a restart. So the interaction lives in the entity
//! view ([`crate::domain_state`]) and its changes travel through the relay's
//! thread scope like every other fact. The daemon never holds a question open
//! on a socket.
//!
//! # What the protocol can and cannot do
//!
//! `loom_provider_protocol` has no "answer an interaction" frame. An answer is
//! therefore **recorded** — the interaction moves to `resolved` with its
//! machine-readable resolution, the same shape bb's clients render — and
//! published through the thread room as `thread_interaction_changed`, but the
//! provider process is not told and the route does not claim it was. That is
//! the same honest divergence `threads.stop` already documents: the control
//! plane is authoritative, the execution plane learns when the protocol grows
//! the frame. Until then a provider that is blocked on a question settles the
//! turn through its own timeout and the daemon reports the outcome.
//!
//! The `resolving` status exists in the domain state machine for the day the
//! confirmation frame exists; today a resolution settles in one step, and
//! [`crate::interactions`] is the single place that changes when it does not.

use loom_domain::{
    Interaction, InteractionId, InteractionKind, InteractionOrigin, InteractionPayload,
    NewInteraction, Resolution, ThreadId,
};

use crate::domain_state::CommandError;
use crate::state::AppState;

/// What happened when an answer was delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// The interaction moved to `resolved` and its change was published.
    Delivered(Box<Interaction>),
    /// The interaction does not exist.
    Unknown,
    /// The interaction was already settled by someone else.
    Settled(Box<Interaction>),
}

impl AppState {
    /// Records a provider's interaction request.
    ///
    /// `provider_request_id` is the provider's own identity for the request and
    /// is what makes a redelivery idempotent: the interaction id is derived
    /// from it, so the second report of the same question lands on the same
    /// row. A provider that reports no request id gets a fresh row, because
    /// there is nothing to deduplicate against.
    #[allow(clippy::too_many_arguments)]
    pub fn record_interaction(
        &self,
        thread_id: &ThreadId,
        turn_id: &str,
        kind: InteractionKind,
        origin: InteractionOrigin,
        payload: InteractionPayload,
        provider_request_id: Option<&str>,
        expires_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Interaction, CommandError> {
        let id = provider_request_id.map(deterministic_interaction_id);
        let (interaction, event) = self.registry.create_interaction(
            NewInteraction {
                thread_id: thread_id.clone(),
                turn_id: turn_id.to_owned(),
                kind,
                origin,
                payload,
                expires_at_ms,
                id,
            },
            now_ms,
        )?;
        if let Some(event) = event {
            let _ = self.publish_domain_event(&event);
        }
        Ok(interaction)
    }

    /// Answers an interaction and publishes the change.
    ///
    /// The order matters and is the crash-safety argument: the entity is
    /// updated **before** the frame is published, so a crash between the two
    /// leaves an interaction a client sees as answered and a frame it did not
    /// see — recoverable by re-reading the interaction — rather than a frame a
    /// provider might have acted on while the entity still says `pending`,
    /// which would let a second client answer the same question.
    pub fn deliver_interaction_resolution(
        &self,
        interaction_id: &InteractionId,
        resolution: Resolution,
        now_ms: u64,
    ) -> DeliverOutcome {
        let Some(existing) = self.registry.interaction(interaction_id) else {
            return DeliverOutcome::Unknown;
        };
        if !existing.status.is_open() {
            return DeliverOutcome::Settled(Box::new(existing));
        }
        match self
            .registry
            .resolve_interaction(interaction_id, resolution, now_ms)
        {
            Ok((interaction, event)) => {
                let _ = self.publish_domain_event(&event);
                DeliverOutcome::Delivered(Box::new(interaction))
            }
            // A conflict means another client settled it while this request was
            // in flight. Report what the interaction actually is; that is what
            // the caller has to know, and it is not an error.
            Err(CommandError::Conflict(_)) => match self.registry.interaction(interaction_id) {
                Some(current) => DeliverOutcome::Settled(Box::new(current)),
                None => DeliverOutcome::Unknown,
            },
            // A kind mismatch was validated by the route before this call, so
            // reaching here means the interaction changed underneath us.
            Err(_) => match self.registry.interaction(interaction_id) {
                Some(current) => DeliverOutcome::Settled(Box::new(current)),
                None => DeliverOutcome::Unknown,
            },
        }
    }

    /// Settles an interaction without an answer.
    ///
    /// Cancelling is what a stopped run leaves behind: the question will never
    /// be answered by the provider, and leaving it `pending` would keep the
    /// thread's pending-interaction flag set forever.
    pub fn cancel_interaction(
        &self,
        interaction_id: &InteractionId,
        reason: Option<String>,
        now_ms: u64,
    ) -> Result<Interaction, CommandError> {
        let (interaction, event) =
            self.registry
                .cancel_interaction(interaction_id, reason, now_ms)?;
        let _ = self.publish_domain_event(&event);
        Ok(interaction)
    }

    /// Cancels every open interaction a thread has, returning how many.
    ///
    /// Called when a run ends: a terminal turn cannot still be waiting on an
    /// answer, so the interactions it raised are settled with it. This is the
    /// invariant that keeps `hasPendingInteraction` honest in the thread list.
    pub fn cancel_thread_interactions(&self, thread_id: &ThreadId, now_ms: u64) -> usize {
        let open = self.registry.pending_interactions(thread_id);
        let mut cancelled = 0;
        for interaction in open {
            if self
                .registry
                .cancel_interaction(
                    &interaction.id,
                    Some("the turn ended before the interaction was answered".into()),
                    now_ms,
                )
                .is_ok()
            {
                cancelled += 1;
            }
        }
        cancelled
    }
}

/// A provider request id as an interaction id.
///
/// The id has to be a valid `intr_…` value and stable for one provider request,
/// so the provider's own string cannot be used verbatim. A hash over it gives
/// both: the same request id always yields the same interaction, so a
/// redelivered question is one row rather than two. FNV-1a is deliberate — the
/// relay already uses it for sharding, so the codebase has one hash function and
/// not two.
fn deterministic_interaction_id(provider_request_id: &str) -> InteractionId {
    /// The Crockford base32 alphabet the id body uses.
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in provider_request_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // 26 base32 characters carry 130 bits, and a 64-bit hash padded into that
    // value leaves the top two bits clear, which is exactly the ULID shape
    // `InteractionId::parse` enforces. The id is not time-ordered — it names a
    // provider request, not a creation — but it is stable and unique per hash.
    let mut bytes = [0u8; 26];
    let mut value = u128::from(hash);
    for slot in bytes.iter_mut().rev() {
        *slot = ALPHABET[(value & 0x1F) as usize];
        value >>= 5;
    }
    let body = String::from_utf8(bytes.to_vec()).expect("the base32 alphabet is ASCII");
    InteractionId::parse(&format!("intr_{body}")).unwrap_or_else(|_| InteractionId::mint())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_request_id_maps_to_a_stable_interaction_id() {
        let first = deterministic_interaction_id("req-1");
        let again = deterministic_interaction_id("req-1");
        let other = deterministic_interaction_id("req-2");
        assert_eq!(first, again, "the same request must map to the same row");
        assert_ne!(first, other);
        assert!(first.to_string().starts_with("intr_"));
    }

    #[test]
    fn a_derived_id_is_a_valid_interaction_id() {
        // Whatever the provider sends, the derived id must parse — it is
        // deserialized on the way back out of the log and the snapshot.
        for request in [
            "",
            "a",
            "provider/request:42",
            "\u{1F600}",
            &"x".repeat(500),
        ] {
            let id = deterministic_interaction_id(request);
            assert_eq!(InteractionId::parse(&id.to_string()).unwrap(), id);
        }
    }
}

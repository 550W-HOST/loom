//! Prefixed, typed entity identifiers.
//!
//! Every entity kind gets its own [`Id`] instantiation
//! ([`ProjectId`](crate::ProjectId), [`ThreadId`](crate::ThreadId), …), so a
//! host id cannot be passed where a thread id is expected: mixing them is a
//! compile error, not a production incident.
//!
//! At the wire level an id is `"<prefix>_<26 Crockford base32 chars>"`, for
//! example `"thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z"`. The prefix is validated on
//! parse *and* on deserialize, so a frame carrying the wrong kind of id is
//! rejected before it reaches domain code.
//!
//! One id is not minted at all: the personal project is `proj_personal`,
//! because that literal is what the client addresses the projectless scope by.
//! A kind may reserve such an id with [`Entity::SENTINEL`]; it is accepted on
//! parse, never produced by [`Id::mint`], and there is exactly one.
//!
//! The 26-character body is ULID-shaped (48-bit millisecond timestamp + 80
//! bits of process entropy), which means identifiers sort in creation order.
//! That is the same shape the relay uses for its event ids, but it is a
//! separate type on purpose: an event id and a thread id are not
//! interchangeable, and neither crate needs the other's internals.

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::DomainError;

/// An entity kind that owns a prefixed identifier.
///
/// Implemented by the marker types below, never by the entities themselves.
pub trait Entity: Sized {
    /// Short lowercase prefix, unique per entity kind (for example `thr`).
    const PREFIX: &'static str;
    /// Human-readable name, used in error messages.
    const NAME: &'static str;
    /// One id this kind reserves, when the wire contract fixes one.
    ///
    /// Every id this crate mints is ULID-shaped, which is what makes ids sort
    /// in creation order. That is the wrong property for a *scope* the client
    /// addresses by a fixed name: the personal project is `proj_personal` on
    /// the wire, not something the server invents, so a client that built a
    /// route out of that literal must be able to send it back. A reserved id
    /// is therefore accepted by [`Id::parse`] — and so by deserialization,
    /// which validates through it — without being mintable.
    ///
    /// Only [`ProjectTag`] has one. Nothing else should: a reserved id is a
    /// fixed point in the wire contract, not a convenience.
    const SENTINEL: Option<&'static str> = None;
}

/// The id the personal project is fixed to on the wire.
///
/// The client addresses the projectless scope by this literal — it is the id
/// it puts in a `/threads/:id` route and sends back when creating a thread
/// outside any project — so the server has to answer with it too. It is not
/// minted and not derived: it is the fixed point the two sides agree on, which
/// is why it is the one entry in [`Entity::SENTINEL`].
pub const PERSONAL_PROJECT_ID: &str = "proj_personal";

macro_rules! entity_marker {
    ($marker:ident, $prefix:literal, $name:literal, $doc:literal) => {
        entity_marker!($marker, $prefix, $name, $doc, None);
    };
    ($marker:ident, $prefix:literal, $name:literal, $doc:literal, $sentinel:expr) => {
        #[doc = $doc]
        #[derive(Debug)]
        pub enum $marker {}

        impl Entity for $marker {
            const PREFIX: &'static str = $prefix;
            const NAME: &'static str = $name;
            const SENTINEL: Option<&'static str> = $sentinel;
        }
    };
}

entity_marker!(
    ProjectTag,
    "proj",
    "project",
    "Marker type for project identifiers.",
    Some(PERSONAL_PROJECT_ID)
);
entity_marker!(
    ThreadTag,
    "thr",
    "thread",
    "Marker type for thread identifiers."
);
entity_marker!(HostTag, "host", "host", "Marker type for host identifiers.");
entity_marker!(
    EnvironmentTag,
    "env",
    "environment",
    "Marker type for environment identifiers."
);
entity_marker!(
    MessageTag,
    "msg",
    "message",
    "Marker type for thread message identifiers."
);
entity_marker!(
    ProjectSourceTag,
    "src",
    "project source",
    "Marker type for project source identifiers."
);
entity_marker!(
    RunTag,
    "run",
    "run",
    "Marker type for provider run identifiers."
);
entity_marker!(UserTag, "user", "user", "Marker type for user identifiers.");
entity_marker!(
    QueuedMessageTag,
    "qmsg",
    "queued message",
    "Marker type for queued-message identifiers."
);
entity_marker!(
    InteractionTag,
    "intr",
    "interaction",
    "Marker type for interaction identifiers."
);
entity_marker!(
    ThreadSectionTag,
    "sec",
    "thread section",
    "Marker type for thread-section identifiers."
);
entity_marker!(
    AttachmentTag,
    "att",
    "attachment",
    "Marker type for project-attachment identifiers."
);
entity_marker!(
    AutomationTag,
    "auto",
    "automation",
    "Marker type for automation identifiers."
);
entity_marker!(
    AutomationRunTag,
    "arun",
    "automation run",
    "Marker type for automation-run identifiers."
);

/// A project id, `proj_…`.
pub type ProjectId = Id<ProjectTag>;
/// A thread id, `thr_…`.
pub type ThreadId = Id<ThreadTag>;
/// A host id, `host_…`.
pub type HostId = Id<HostTag>;
/// An environment id, `env_…`.
pub type EnvironmentId = Id<EnvironmentTag>;
/// A thread message id, `msg_…`.
pub type MessageId = Id<MessageTag>;
/// A project source id, `src_…`.
pub type ProjectSourceId = Id<ProjectSourceTag>;
/// A provider run id, `run_…`.
///
/// One dispatch of one thread to one provider process. It is minted by the
/// control plane and is the idempotency key for dispatch delivery: a worker
/// that reconnects and receives the same run again recognises it and does not
/// start a second provider.
pub type RunId = Id<RunTag>;
/// A user id, `user_…`. Reserved: there is no user entity yet, only the scope
/// it names.
pub type UserId = Id<UserTag>;
/// A queued-message id, `qmsg_…`.
pub type QueuedMessageId = Id<QueuedMessageTag>;
/// An interaction id, `intr_…`.
pub type InteractionId = Id<InteractionTag>;
/// A thread-section id, `sec_…`.
pub type ThreadSectionId = Id<ThreadSectionTag>;
/// A project-attachment id, `att_…`.
pub type AttachmentId = Id<AttachmentTag>;
/// An automation id, `auto_…`.
pub type AutomationId = Id<AutomationTag>;
/// An automation-run id, `arun_…`.
///
/// Distinct from [`RunId`] (`run_…`): that one identifies a provider run the
/// control plane dispatches, this one a row in an automation's history.
pub type AutomationRunId = Id<AutomationRunTag>;

/// A prefixed identifier for entity kind `T`.
///
/// Constructed with [`Id::mint`] (a fresh ULID) or [`Id::parse`] (validated
/// text). The phantom parameter carries no data; `fn() -> T` keeps the type
/// covariant and makes `Id<T>` unconditionally `Send`/`Sync`.
pub struct Id<T> {
    value: String,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    /// The full `prefix_body` text.
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<T: Entity> Id<T> {
    /// Mints a new identifier with this entity's prefix.
    pub fn mint() -> Self {
        Self {
            value: format!("{}_{}", T::PREFIX, mint_ulid()),
            _marker: PhantomData,
        }
    }

    /// This kind's reserved id, when it has one. See [`Entity::SENTINEL`].
    ///
    /// `None` for every kind but the project, whose reserved id is the scope
    /// the client calls personal.
    pub fn sentinel() -> Option<Self> {
        T::SENTINEL.map(|value| Self {
            value: value.to_owned(),
            _marker: PhantomData,
        })
    }

    /// Parses `<prefix>_<26-char ULID>`, or the kind's reserved id.
    ///
    /// Rejects a well-formed ULID carrying the wrong prefix, which is the
    /// whole point of typed ids.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        if T::SENTINEL == Some(value) {
            return Ok(Self {
                value: value.to_owned(),
                _marker: PhantomData,
            });
        }
        let malformed = || DomainError::MalformedId {
            expected: T::NAME,
            value: value.to_owned(),
        };
        let rest = value.strip_prefix(T::PREFIX).ok_or_else(malformed)?;
        let body = rest.strip_prefix('_').ok_or_else(malformed)?;
        if !is_ulid(body) {
            return Err(malformed());
        }
        Ok(Self {
            value: value.to_owned(),
            _marker: PhantomData,
        })
    }

    /// The body below the prefix.
    ///
    /// The 26-character ULID for a minted id, or the reserved word for the one
    /// kind that has a reserved id.
    pub fn body(&self) -> &str {
        // A parsed or minted id always has `PREFIX` followed by `_`.
        &self.value[T::PREFIX.len() + 1..]
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Eq for Id<T> {}

impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<T> std::hash::Hash for Id<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<T: Entity> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", T::NAME, self.value)
    }
}

impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl<T> Serialize for Id<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.value)
    }
}

impl<'de, T: Entity> Deserialize<'de> for Id<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Id::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl<T: Entity> FromStr for Id<T> {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Id::parse(value)
    }
}

// --- ULID-shaped body -------------------------------------------------------

const BODY_LEN: usize = 26;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const RAND_BITS: u32 = 80;
const RAND_MASK: u128 = (1u128 << RAND_BITS) - 1;

fn is_ulid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != BODY_LEN {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let Some(digit) = decode_char(*byte) else {
            return false;
        };
        // 26 base32 characters carry 130 bits; the top two must be zero.
        if index == 0 && digit > 7 {
            return false;
        }
    }
    true
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Process-wide `(last_timestamp, last_entropy)` so ids never regress, even
/// when two are minted in the same millisecond or the wall clock steps back.
fn clock() -> &'static Mutex<(u64, u128)> {
    static CLOCK: OnceLock<Mutex<(u64, u128)>> = OnceLock::new();
    CLOCK.get_or_init(|| Mutex::new((0, random_80())))
}

fn mint_ulid() -> String {
    let now = now_ms();
    let mut guard = clock().lock().unwrap_or_else(|poison| poison.into_inner());
    let (last_ts, last_rand) = *guard;
    let (ts, rand) = if now > last_ts {
        (now, random_80())
    } else {
        let next = last_rand.wrapping_add(1) & RAND_MASK;
        if next == 0 {
            (last_ts.wrapping_add(1), random_80())
        } else {
            (last_ts, next)
        }
    };
    *guard = (ts, rand);
    encode_base32(((ts as u128) << RAND_BITS) | rand)
}

fn encode_base32(value: u128) -> String {
    let mut out = [0u8; BODY_LEN];
    let mut remaining = value;
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(remaining & 0x1F) as usize];
        remaining >>= 5;
    }
    // The alphabet is ASCII, so this cannot fail.
    String::from_utf8(out.to_vec()).expect("base32 alphabet is ASCII")
}

// --- client turn request ids -------------------------------------------------

/// The alphabet bb's `turnRequestIdSchema` allows below the prefix: the digits
/// `2`-`9` and the lowercase letters except `l` and `o`.
const REQUEST_ALPHABET: &[u8; 32] = b"23456789abcdefghijkmnpqrstuvwxyz";
/// How many characters follow `creq_` in a turn request id.
const REQUEST_BODY_LEN: usize = 10;

/// Mints a client turn request id, `creq_<10 chars>`.
///
/// The shape is bb's (`^creq_[23456789abcdefghijkmnpqrstuvwxyz]{10}$`): shorter
/// than a ULID and over a narrower alphabet, so it is deliberately not an
/// [`Id<T>`]. It is the low 50 bits of a fresh ULID, which is what makes two
/// ids minted in the same millisecond differ — the timestamp lives in the high
/// bits, and taking those would repeat the whole millisecond.
pub fn mint_turn_request_id() -> String {
    let source = mint_ulid();
    let mut body = String::with_capacity(REQUEST_BODY_LEN);
    for byte in source.bytes().skip(source.len() - REQUEST_BODY_LEN) {
        let digit = decode_char(byte).expect("a minted body is base32") as usize;
        body.push(REQUEST_ALPHABET[digit] as char);
    }
    format!("creq_{body}")
}

/// Whether `value` is shaped like a client turn request id.
///
/// The contract's own validator deliberately does not enforce `pattern`
/// (`loom_contract::schema`), so a route that accepts a client-supplied id
/// checks it here rather than trusting a check that does not run.
pub fn is_turn_request_id(value: &str) -> bool {
    let Some(body) = value.strip_prefix("creq_") else {
        return false;
    };
    body.len() == REQUEST_BODY_LEN && body.bytes().all(|byte| REQUEST_ALPHABET.contains(&byte))
}

/// 80 bits of process entropy, seeded from the OS via `RandomState`.
fn random_80() -> u128 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut first = RandomState::new().build_hasher();
    first.write_u64(now_ms());
    let high = first.finish();

    let mut second = RandomState::new().build_hasher();
    second.write_u64(high);
    let low = second.finish();

    (((high as u128) << 16) ^ ((low as u128) & 0xFFFF)) & RAND_MASK
}

fn decode_char(byte: u8) -> Option<u8> {
    let index = match byte {
        b'0'..=b'9' => byte - b'0',
        b'A'..=b'H' => byte - b'A' + 10,
        b'J'..=b'K' => byte - b'J' + 18,
        b'M'..=b'N' => byte - b'M' + 20,
        b'P'..=b'T' => byte - b'P' + 22,
        b'V'..=b'Z' => byte - b'V' + 27,
        // Crockford treats these as their visually-ambiguous counterparts.
        b'I' | b'L' => 1,
        b'O' => 0,
        b'a'..=b'z' => return decode_char(byte - 32),
        _ => return None,
    };
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_carry_their_prefix_and_round_trip() {
        let thread = ThreadId::mint();
        let text = thread.to_string();
        assert!(text.starts_with("thr_"));
        assert_eq!(text.len(), "thr_".len() + BODY_LEN);
        assert_eq!(ThreadId::parse(&text).unwrap(), thread);

        let host = HostId::mint();
        assert!(host.to_string().starts_with("host_"));
        assert_eq!(HostId::parse(&host.to_string()).unwrap(), host);
    }

    #[test]
    fn ids_sort_in_minting_order() {
        let mut previous = ThreadId::mint();
        for _ in 0..1_000 {
            let next = ThreadId::mint();
            assert!(next > previous, "{next:?} must sort after {previous:?}");
            previous = next;
        }
    }

    #[test]
    fn the_prefix_makes_kinds_incompatible() {
        let thread = ThreadId::mint();
        assert!(matches!(
            HostId::parse(thread.as_str()),
            Err(DomainError::MalformedId {
                expected: "host",
                ..
            })
        ));
        assert!(matches!(
            ThreadId::parse("host_01M27Y6Q0J8V4W2C7K5N3P1R9Z"),
            Err(DomainError::MalformedId {
                expected: "thread",
                ..
            })
        ));
    }

    #[test]
    fn rejects_malformed_bodies() {
        for value in [
            "",
            "thr_",
            "thr_too-short",
            "thr_01M27Y6Q0J8V4W2C7K5N3P1R9",   // 25 chars
            "thr_01M27Y6Q0J8V4W2C7K5N3P1R9Z0", // 27 chars
            "thr_Z1M27Y6Q0J8V4W2C7K5N3P1R9Z",  // top bits set
            "thr_01M27Y6Q0J8V4W2C7K5N3P1R9U",  // `U` is not in the alphabet
            "thr01M27Y6Q0J8V4W2C7K5N3P1R9Z",   // no separator
        ] {
            assert!(
                ThreadId::parse(value).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    #[test]
    fn accepts_lowercase_and_ambiguous_digits() {
        let preserved = "thr_01m27y6q0j8v4w2c7k5n3p1r9z";
        assert!(ThreadId::parse(preserved).is_ok());
        assert!(ThreadId::parse("thr_01M27Y6Q0J8V4W2C7K5N3P1R9O").is_ok());
        assert!(ThreadId::parse("thr_01M27Y6Q0J8V4W2C7K5N3P1R9I").is_ok());
    }

    #[test]
    fn serde_uses_the_text_form_and_validates() {
        let thread = ThreadId::mint();
        let json = serde_json::to_string(&thread).unwrap();
        assert_eq!(json, format!("\"{thread}\""));
        assert_eq!(serde_json::from_str::<ThreadId>(&json).unwrap(), thread);

        // A well-formed string for the wrong kind is rejected at deserialize.
        assert!(serde_json::from_str::<ThreadId>("\"host_01M27Y6Q0J8V4W2C7K5N3P1R9Z\"").is_err());
    }

    #[test]
    fn the_reserved_project_id_parses_and_round_trips() {
        // The one id that is not ULID-shaped. A client addresses the personal
        // scope by this literal and sends it back as a thread's `projectId`, so
        // the server has to accept it — and answer with it.
        let personal = ProjectId::parse(PERSONAL_PROJECT_ID).unwrap();
        assert_eq!(personal.as_str(), "proj_personal");
        assert_eq!(personal.body(), "personal");
        assert_eq!(ProjectId::sentinel(), Some(personal.clone()));

        // Deserialization validates through `parse`, so a frame carrying it is
        // accepted too.
        let from_json: ProjectId =
            serde_json::from_str(&format!("\"{PERSONAL_PROJECT_ID}\"")).unwrap();
        assert_eq!(from_json, personal);

        // It is reserved, not minted: minting still produces a ULID body, and
        // no other kind reserves anything.
        let minted = ProjectId::mint();
        assert_ne!(minted, personal);
        assert_eq!(minted.body().len(), BODY_LEN);
        assert_eq!(ThreadId::sentinel(), None);
        assert_eq!(HostId::sentinel(), None);
    }

    #[test]
    fn a_reserved_id_belongs_to_one_kind_only() {
        // The reserved id is a project id, so it is not a thread id; and the
        // reserved word is not an id on its own, in another case, or with a
        // stray character.
        assert!(ThreadId::parse(PERSONAL_PROJECT_ID).is_err());
        assert!(ProjectId::parse("personal").is_err());
        assert!(ProjectId::parse("PROJ_PERSONAL").is_err());
        assert!(ProjectId::parse("proj_personal ").is_err());
    }

    #[test]
    fn turn_request_ids_match_bb_shape_and_do_not_repeat() {
        let mut previous = mint_turn_request_id();
        assert!(is_turn_request_id(&previous));
        assert_eq!(previous.len(), "creq_".len() + REQUEST_BODY_LEN);
        for _ in 0..1_000 {
            let next = mint_turn_request_id();
            assert!(is_turn_request_id(&next), "{next:?} is outside bb's shape");
            assert_ne!(next, previous, "a minted id repeated");
            previous = next;
        }
    }

    #[test]
    fn turn_request_ids_outside_the_pattern_are_rejected() {
        for value in [
            "",
            "creq_",
            "creq_23456789a",   // 9 characters
            "creq_23456789abc", // 11 characters
            "creq_23456789a0",  // `0` is not in the alphabet
            "creq_23456789a1",  // `1` is not in the alphabet
            "creq_23456789al",  // `l` is not in the alphabet
            "creq_23456789ao",  // `o` is not in the alphabet
            "creq_23456789AB",  // upper case is not the contract's spelling
            "CREQ_23456789ab",  // wrong prefix
            "req_23456789ab",   // wrong prefix
        ] {
            assert!(!is_turn_request_id(value), "{value:?} must be rejected");
        }
    }
}

//! Monotonic 128-bit event identifiers.
//!
//! An [`EventId`] is ULID-shaped: a 48-bit millisecond timestamp followed by
//! 80 bits of entropy, rendered as 26 Crockford base32 characters. Two
//! properties matter to the relay:
//!
//! * **Total ordering equal to creation order.** The base32 text sorts the
//!   same way the numeric value does, so "replay everything after X" is a
//!   string/number comparison, not a separate sequence allocation.
//! * **Monotonicity inside a process.** Two ids minted in the same
//!   millisecond still sort in call order. Cross-process monotonicity is not
//!   required: the relay orders by `(shard append order)`, and ties across
//!   nodes are broken by the timestamps that seed each id.

use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Timestamp occupies the high 48 bits; entropy the low 80.
const RAND_BITS: u32 = 80;
const RAND_MASK: u128 = (1u128 << RAND_BITS) - 1;
const ENCODED_LEN: usize = 26;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Returned when a string is not a well-formed event id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseEventIdError;

impl fmt::Display for ParseEventIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a 26-character Crockford base32 event id")
    }
}

impl std::error::Error for ParseEventIdError {}

/// A monotonic, sortable identifier for one relayed event.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(u128);

impl EventId {
    /// Mints a new id that sorts strictly after every id this process has
    /// already minted.
    pub fn new() -> Self {
        let now = now_ms();
        let mut guard = clock().lock().unwrap_or_else(|poison| poison.into_inner());
        let (last_ts, last_rand) = *guard;
        let (ts, rand) = if now > last_ts {
            (now, random_80())
        } else {
            // Same millisecond, or the wall clock went backwards: advance the
            // entropy instead so ordering never regresses.
            let next = last_rand.wrapping_add(1) & RAND_MASK;
            if next == 0 {
                (last_ts.wrapping_add(1), random_80())
            } else {
                (last_ts, next)
            }
        };
        *guard = (ts, rand);
        EventId(((ts as u128) << RAND_BITS) | rand)
    }

    /// Wraps a raw 128-bit value (used when decoding from storage).
    pub const fn from_raw(raw: u128) -> Self {
        EventId(raw)
    }

    /// The raw 128-bit value.
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    /// Millisecond timestamp embedded in the id.
    pub const fn timestamp_ms(self) -> u64 {
        (self.0 >> RAND_BITS) as u64
    }

    /// The 80 bits of entropy below the timestamp.
    pub const fn entropy(self) -> u128 {
        self.0 & RAND_MASK
    }

    /// The 26-character Crockford base32 form.
    pub fn to_base32(self) -> String {
        encode_base32(self.0)
    }
}

impl Default for EventId {
    fn default() -> Self {
        EventId::new()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base32())
    }
}

impl fmt::Debug for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EventId({})", self.to_base32())
    }
}

impl FromStr for EventId {
    type Err = ParseEventIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.len() != ENCODED_LEN {
            return Err(ParseEventIdError);
        }
        // 26 characters carry 130 bits; a 128-bit value leaves the top two
        // bits of the first character zero.
        let first = decode_char(bytes[0]).ok_or(ParseEventIdError)?;
        if first > 7 {
            return Err(ParseEventIdError);
        }
        let mut raw: u128 = 0;
        for byte in bytes {
            let digit = decode_char(*byte).ok_or(ParseEventIdError)?;
            raw = (raw << 5) | digit;
        }
        Ok(EventId(raw))
    }
}

impl Serialize for EventId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_base32())
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        EventId::from_str(&text).map_err(serde::de::Error::custom)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Process-wide `(last_timestamp, last_entropy)` so ids never regress.
fn clock() -> &'static Mutex<(u64, u128)> {
    static CLOCK: OnceLock<Mutex<(u64, u128)>> = OnceLock::new();
    CLOCK.get_or_init(|| Mutex::new((0, random_80())))
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

fn encode_base32(value: u128) -> String {
    let mut out = [0u8; ENCODED_LEN];
    let mut remaining = value;
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(remaining & 0x1F) as usize];
        remaining >>= 5;
    }
    // The alphabet is ASCII, so this cannot fail.
    String::from_utf8(out.to_vec()).expect("base32 alphabet is ASCII")
}

fn decode_char(byte: u8) -> Option<u128> {
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
    Some(u128::from(index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_monotonic() {
        let mut previous = EventId::new();
        for _ in 0..10_000 {
            let next = EventId::new();
            assert!(next > previous, "{next:?} must sort after {previous:?}");
            previous = next;
        }
    }

    #[test]
    fn text_order_matches_numeric_order() {
        let mut ids: Vec<EventId> = (0..1_000).map(|_| EventId::new()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        ids.sort_by_key(|id| id.to_base32());
        assert_eq!(ids, sorted);
    }

    #[test]
    fn encoding_is_26_chars_and_round_trips() {
        for _ in 0..1_000 {
            let id = EventId::new();
            let text = id.to_base32();
            assert_eq!(text.len(), ENCODED_LEN);
            assert_eq!(text.parse::<EventId>().unwrap(), id);
        }
    }

    #[test]
    fn timestamp_is_recoverable() {
        let before = now_ms();
        let id = EventId::new();
        let after = now_ms();
        assert!((before..=after).contains(&id.timestamp_ms()));
        assert_eq!(
            (u128::from(id.timestamp_ms()) << RAND_BITS) | id.entropy(),
            id.as_u128()
        );
    }

    #[test]
    fn rejects_malformed_text() {
        assert!("".parse::<EventId>().is_err());
        assert!("too-short".parse::<EventId>().is_err());
        // 26 chars, but the first digit sets bits above 128.
        assert!("Z0000000000000000000000000".parse::<EventId>().is_err());
        // 'U' is not in the Crockford alphabet.
        assert!("U0000000000000000000000000".parse::<EventId>().is_err());
    }

    #[test]
    fn accepts_lowercase_and_ambiguous_digits() {
        let one = EventId::from_raw(1);
        assert_eq!(one.to_base32(), "00000000000000000000000001");
        assert_eq!(
            "0000000000000000000000000I".parse::<EventId>().unwrap(),
            one,
            "Crockford maps I to one"
        );
        assert_eq!(
            "0000000000000000000000000o".parse::<EventId>().unwrap(),
            EventId::from_raw(0),
            "Crockford maps O to zero"
        );
    }

    #[test]
    fn serde_uses_the_text_form() {
        let id = EventId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<EventId>(&json).unwrap(), id);
    }
}

//! Retention policy for the relay log.
//!
//! Retention is expressed as three nested horizons so that the guarantees do
//! not fight each other:
//!
//! ```text
//!   now ─────────────────────────────────────────────▶ time
//!       │<── replay_grace ──>│<── trim_horizon ──>│
//!       │  guaranteed replay │  retained, may be  │
//!       │  for every reader  │  trimmed at will   │
//!       │                    │                    │
//!       └──────── ttl ───────┴────────────────────┘
//! ```
//!
//! * `replay_grace` is how far back a freshly started (or reconnected) reader
//!   is guaranteed to be able to replay.
//! * `trim_horizon` must be strictly greater than `replay_grace`, so trimming
//!   can never eat into the replay window. It is what maintenance actually
//!   uses as its cut-off.
//! * `ttl` bounds how long an idle shard's storage lives at all, and must be at
//!   least `trim_horizon`.
//!
//! These relationships are validated rather than assumed, because getting them
//! backwards silently breaks replay exactly when it is needed (after an
//! outage).

use crate::error::{RelayError, Result};

/// Default replay grace: five minutes.
pub const DEFAULT_REPLAY_GRACE_MS: u64 = 5 * 60 * 1_000;
/// Default trim horizon: twice the replay grace.
pub const DEFAULT_TRIM_HORIZON_MS: u64 = 2 * DEFAULT_REPLAY_GRACE_MS;
/// Default overall TTL: trim horizon plus one grace window.
pub const DEFAULT_TTL_MS: u64 = DEFAULT_TRIM_HORIZON_MS + DEFAULT_REPLAY_GRACE_MS;
/// How often maintenance runs.
pub const DEFAULT_MAINTENANCE_INTERVAL_MS: u64 = 60 * 1_000;
/// Default approximate per-shard entry cap.
pub const DEFAULT_MAX_LEN: u64 = 2_000;

/// Retention settings for the relay log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retention {
    /// Approximate maximum records kept per shard before the oldest are
    /// dropped on append.
    pub max_len: u64,
    /// Guaranteed replay window.
    pub replay_grace_ms: u64,
    /// Cut-off used by maintenance. Must exceed `replay_grace_ms`.
    pub trim_horizon_ms: u64,
    /// Lifetime of an idle shard. Must be at least `trim_horizon_ms`.
    pub ttl_ms: u64,
    /// How often maintenance runs. Must be positive and below `ttl_ms`.
    pub maintenance_interval_ms: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_len: DEFAULT_MAX_LEN,
            replay_grace_ms: DEFAULT_REPLAY_GRACE_MS,
            trim_horizon_ms: DEFAULT_TRIM_HORIZON_MS,
            ttl_ms: DEFAULT_TTL_MS,
            maintenance_interval_ms: DEFAULT_MAINTENANCE_INTERVAL_MS,
        }
    }
}

impl Retention {
    /// Rejects settings whose horizons are inconsistent.
    pub fn validate(&self) -> Result<()> {
        if self.max_len == 0 {
            return Err(RelayError::config("max_len must be positive"));
        }
        if self.replay_grace_ms == 0 {
            return Err(RelayError::config("replay_grace_ms must be positive"));
        }
        if self.trim_horizon_ms <= self.replay_grace_ms {
            return Err(RelayError::config(format!(
                "trim_horizon_ms ({}) must exceed replay_grace_ms ({})",
                self.trim_horizon_ms, self.replay_grace_ms
            )));
        }
        if self.ttl_ms < self.trim_horizon_ms {
            return Err(RelayError::config(format!(
                "ttl_ms ({}) must be at least trim_horizon_ms ({})",
                self.ttl_ms, self.trim_horizon_ms
            )));
        }
        if self.maintenance_interval_ms == 0 || self.maintenance_interval_ms >= self.ttl_ms {
            return Err(RelayError::config(format!(
                "maintenance_interval_ms ({}) must be positive and below ttl_ms ({})",
                self.maintenance_interval_ms, self.ttl_ms
            )));
        }
        Ok(())
    }

    /// Repairs inconsistent settings instead of failing, in the same order the
    /// relationships depend on. Intended for operator-supplied values.
    pub fn repaired(mut self) -> Self {
        let defaults = Retention::default();
        if self.max_len == 0 {
            self.max_len = defaults.max_len;
        }
        if self.replay_grace_ms == 0 {
            self.replay_grace_ms = defaults.replay_grace_ms;
        }
        if self.trim_horizon_ms <= self.replay_grace_ms {
            self.trim_horizon_ms = self.replay_grace_ms.saturating_mul(2);
        }
        if self.ttl_ms < self.trim_horizon_ms {
            self.ttl_ms = self.trim_horizon_ms.saturating_add(self.replay_grace_ms);
        }
        if self.maintenance_interval_ms == 0 || self.maintenance_interval_ms >= self.ttl_ms {
            self.maintenance_interval_ms = (self.ttl_ms / 3).max(1);
        }
        self
    }

    /// The oldest timestamp a fresh reader is guaranteed to replay from.
    pub fn replay_start_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.replay_grace_ms)
    }

    /// The cut-off maintenance passes to the backend.
    pub fn trim_before_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.trim_horizon_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_ordered() {
        let retention = Retention::default();
        retention.validate().unwrap();
        assert!(retention.trim_horizon_ms > retention.replay_grace_ms);
        assert!(retention.ttl_ms >= retention.trim_horizon_ms);
        assert!(retention.maintenance_interval_ms < retention.ttl_ms);
    }

    #[test]
    fn trim_never_eats_the_replay_window() {
        let retention = Retention::default();
        let now = 1_000_000_000;
        assert!(retention.trim_before_ms(now) < retention.replay_start_ms(now));
    }

    #[test]
    fn rejects_inverted_horizons() {
        let bad = Retention {
            trim_horizon_ms: 1_000,
            replay_grace_ms: 5_000,
            ..Retention::default()
        };
        assert!(matches!(bad.validate(), Err(RelayError::Config(_))));
    }

    #[test]
    fn rejects_ttl_below_trim_horizon() {
        let bad = Retention {
            ttl_ms: 1_000,
            ..Retention::default()
        };
        assert!(matches!(bad.validate(), Err(RelayError::Config(_))));
    }

    #[test]
    fn rejects_maintenance_interval_at_or_above_ttl() {
        let bad = Retention {
            maintenance_interval_ms: DEFAULT_TTL_MS,
            ..Retention::default()
        };
        assert!(matches!(bad.validate(), Err(RelayError::Config(_))));
    }

    #[test]
    fn repair_restores_a_usable_policy() {
        let broken = Retention {
            max_len: 0,
            replay_grace_ms: 10_000,
            trim_horizon_ms: 1,
            ttl_ms: 1,
            maintenance_interval_ms: 0,
        };
        let fixed = broken.repaired();
        fixed.validate().unwrap();
        assert_eq!(fixed.replay_grace_ms, 10_000);
        assert!(fixed.trim_horizon_ms > fixed.replay_grace_ms);
        assert!(fixed.ttl_ms >= fixed.trim_horizon_ms);
    }

    #[test]
    fn horizons_saturate_instead_of_underflowing() {
        let retention = Retention::default();
        assert_eq!(retention.replay_start_ms(1), 0);
        assert_eq!(retention.trim_before_ms(1), 0);
    }
}

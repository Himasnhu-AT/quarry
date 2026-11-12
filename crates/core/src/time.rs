//! Wall-clock instants as plain numbers. Core never reads the clock: callers pass `now` in.

use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch (UTC). Serialized as a JSON integer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// `self` plus `ms`, saturating.
    pub fn plus_ms(self, ms: u64) -> Self {
        Self(self.0.saturating_add(ms))
    }
    /// Milliseconds from `earlier` to `self` (0 if `earlier` is later).
    pub fn since(self, earlier: Timestamp) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

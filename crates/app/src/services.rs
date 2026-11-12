//! Runtime state shared by commands, views and jobs that is not the document: the proposal book
//! now; the open engine connections, the audit sink and the agent pause flag as the streams add
//! them (additively).
//!
//! It is process-wide because an ashlar 0.2 `Session` has no slot for app state (see
//! `docs/contracts.md`, "ashlar 0.2 gaps"), and must not be the document: undo must never touch a
//! decision. Locks recover from poisoning (AGENTS.md §0).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use quarry_core::{ProposalBook, Timestamp};

/// The app's runtime state.
#[derive(Default)]
pub struct Services {
    proposals: Mutex<ProposalBook>,
    agent_paused: AtomicBool,
}

impl Services {
    /// The proposal book (decisions go through `crate::human::perform` only).
    pub fn proposals(&self) -> MutexGuard<'_, ProposalBook> {
        self.proposals.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the person paused the agent (tool calls are refused `agent_paused`).
    pub fn agent_paused(&self) -> bool {
        self.agent_paused.load(Ordering::Relaxed)
    }

    /// Only `crate::human` changes it (pause / resume are human-only).
    pub(crate) fn set_agent_paused(&self, paused: bool) {
        self.agent_paused.store(paused, Ordering::Relaxed);
    }
}

/// The process's services.
pub fn services() -> &'static Services {
    static S: OnceLock<Services> = OnceLock::new();
    S.get_or_init(Services::default)
}

/// Now, as a [`Timestamp`] (0 if the clock is before 1970).
pub fn now() -> Timestamp {
    Timestamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_flag_round_trips_and_clock_is_after_2020() {
        let s = Services::default();
        assert!(!s.agent_paused());
        s.set_agent_paused(true);
        assert!(s.agent_paused());
        assert!(now() > Timestamp(1_577_836_800_000));
        assert_eq!(s.proposals().pending().count(), 0);
    }
}

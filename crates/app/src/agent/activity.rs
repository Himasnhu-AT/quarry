//! What the agent did, for the agent panel: every tool call (queries run, rows returned, charts
//! made, proposals) with its outcome and where it led. Process-wide, capped, newest last.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use quarry_core::{PanelId, ProposalId, TabId, Timestamp};

/// Entries kept (the oldest go first).
pub const KEPT: usize = 500;

/// Where an activity entry leads when clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    Tab(TabId),
    Panel(PanelId),
    Proposal(ProposalId),
}

/// One tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub seq: u64,
    pub at: Timestamp,
    pub tool: &'static str,
    /// A short line: the SQL, the table, the region…
    pub summary: String,
    /// Rows returned (reads) or drawn (charts).
    pub rows: Option<u64>,
    pub elapsed_ms: u64,
    /// `None`: it worked; `Some(refusal)`: the `"code: message"` it failed with.
    pub refused: Option<String>,
    pub jump: Option<Jump>,
}

struct Log {
    items: VecDeque<Activity>,
}

fn log() -> MutexGuard<'static, Log> {
    static L: OnceLock<Mutex<Log>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Log { items: VecDeque::new() })).lock().unwrap_or_else(PoisonError::into_inner)
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Record a tool call; returns its sequence number.
pub fn record(
    tool: &'static str,
    summary: impl Into<String>,
    rows: Option<u64>,
    elapsed_ms: u64,
    refused: Option<String>,
    jump: Option<Jump>,
) -> u64 {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    let summary: String = summary.into().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect();
    let mut l = log();
    if l.items.len() >= KEPT {
        l.items.pop_front();
    }
    l.items.push_back(Activity { seq, at: crate::services::now(), tool, summary, rows, elapsed_ms, refused, jump });
    seq
}

/// The latest `n` entries, newest first.
pub fn latest(n: usize) -> Vec<Activity> {
    log().items.iter().rev().take(n).cloned().collect()
}

/// When the newest tool call was recorded (`None`: none yet).
pub fn last_at() -> Option<Timestamp> {
    log().items.back().map(|a| a.at)
}

/// The sequence number of the newest entry (0: none yet); the panel redraws when it moves.
pub fn revision() -> u64 {
    SEQ.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_capped_and_newest_first() {
        let first = record("db.query", "SELECT   1\n FROM t", Some(1), 2, None, None);
        for _ in 0..KEPT {
            record("db.schema", "x", None, 0, None, None);
        }
        let last = record("view.focus", "grid", None, 0, Some("agent_paused: paused".into()), None);
        let l = latest(3);
        assert_eq!(l.first().map(|a| a.seq), Some(last));
        assert!(latest(KEPT + 10).iter().all(|a| a.seq != first), "the oldest entry was dropped");
        assert!(revision() >= last);
    }
}

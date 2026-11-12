//! The agent stream: MCP tools ([`tools`]), the read path ([`read`]), write approval
//! ([`approval`]), the activity log ([`activity`]) and the agent panel ([`show`]).
//!
//! The product promise lives here: an agent reads, drives the UI and makes charts, but never
//! writes unless the person approves the exact SQL on a card in this window (SECURITY.md).

pub mod activity;
pub mod approval;
mod panel;
pub mod read;
pub mod scenes;
pub mod tools;

pub use panel::{APPROVE_SHORTCUT, FOCUS_KEY, SCENE_KEY, show};

use ashlar::prelude::*;
use quarry_core::Workspace;

/// How long after its last tool call the agent counts as active (the status bar's badge).
pub const ACTIVE_FOR_MS: u64 = 10_000;

/// Whether the agent is active at `now`: a tool call in the last [`ACTIVE_FOR_MS`], or an
/// approved write of its running. `Some(ms)`: active, for at least `ms` more (the status bar
/// repaints then); `None`: idle.
pub fn active_for(now: quarry_core::Timestamp) -> Option<u64> {
    let running = crate::services::services().proposals().iter().any(|p| p.status == quarry_core::ProposalStatus::Running);
    if running {
        return Some(250);
    }
    let last = activity::last_at()?;
    let left = last.0.saturating_add(ACTIVE_FOR_MS).checked_sub(now.0)?;
    (left > 0).then_some(left)
}

/// `proposal.focus` (⌘⇧A, a harmless human command): open the panel and focus the newest
/// pending card's Deny button.
pub fn proposal_focus(s: &mut Session<Workspace>, _: &Value) -> Result<Value, CommandError> {
    s.set_view("agent_panel", &true);
    s.set_view(FOCUS_KEY, &true);
    let pending = crate::services::services().proposals().pending().map(|p| p.id).max();
    Ok(json!({"proposal_id": pending}))
}

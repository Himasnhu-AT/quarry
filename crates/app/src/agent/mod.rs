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

/// `proposal.focus` (⌘⇧A, a harmless human command): open the panel and focus the newest
/// pending card's Deny button.
pub fn proposal_focus(s: &mut Session<Workspace>, _: &Value) -> Result<Value, CommandError> {
    s.set_view("agent_panel", &true);
    s.set_view(FOCUS_KEY, &true);
    let pending = crate::services::services().proposals().pending().map(|p| p.id).max();
    Ok(json!({"proposal_id": pending}))
}

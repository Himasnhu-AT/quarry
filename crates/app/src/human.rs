//! Human-only actions: the window-only path (SECURITY.md layer C, `docs/contracts.md`).
//!
//! ashlar 0.2 cannot hide a registered command from MCP, the CLI or the control port, cannot tell
//! a command body who called it, and lets a control-port client inject clicks and keys into the
//! window (`ui.click`, `ui.press`, `ui.menu.invoke`). So the actions that decide a proposal, run
//! SQL on the human's connection, commit edits or loosen the agent's limits are **not commands**:
//!
//! 1. They are not in the registry, so no agent surface lists or runs them.
//! 2. They run only through [`perform`], which takes a [`HumanTicket`].
//! 3. A ticket is minted only by view code, from a real click ([`HumanTicket::from_click`]) or key
//!    press ([`HumanTicket::from_shortcut`]) on a frame where no injected agent input is playing
//!    ([`agent_driving`]). It cannot be built elsewhere, cloned, stored or sent.
//!
//! When ashlar gains a per-command surface filter (see `docs/contracts.md`, "ashlar 0.2 gaps"),
//! these become registered human-only commands with the same ids.

use ashlar::egui;
use ashlar::prelude::*;
use quarry_core::{ConnectionId, ConnectionSpec, ProposalId, Refusal, RefusalCode, TabId, TableRef, Workspace};

use crate::ids;

/// Proof that a person, not an agent, asked for an action on this frame.
#[derive(Debug)]
pub struct HumanTicket {
    _private: (),
}

/// Whether an agent is driving the window right now: injected input, waits or screenshots from
/// the control port are pending. Human-only actions are refused while it holds.
pub fn agent_driving(ctx: &egui::Context) -> bool {
    ashlar::ui::devtools::has_pending(ctx)
}

impl HumanTicket {
    /// A ticket when `response` was clicked this frame by the person (not while an agent drives).
    pub fn from_click(ctx: &egui::Context, response: &egui::Response) -> Option<Self> {
        (response.clicked() && !agent_driving(ctx)).then_some(Self { _private: () })
    }

    /// A ticket when the person pressed `shortcut` this frame (consumed); none while an agent
    /// drives, and the key press is then left alone.
    pub fn from_shortcut(ctx: &egui::Context, shortcut: &egui::KeyboardShortcut) -> Option<Self> {
        if agent_driving(ctx) {
            return None;
        }
        ctx.input_mut(|i| i.consume_shortcut(shortcut)).then_some(Self { _private: () })
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self { _private: () }
    }
}

/// Which statements `query.run` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunScope {
    /// The statement under the cursor (⌘↵).
    Statement,
    Selection,
    /// Everything in the editor (⇧⌘↵).
    All,
}

/// One human-only action ([`ids::HUMAN_ONLY`]).
#[derive(Debug)]
pub enum HumanAction {
    /// Approve the exact statement on the card. Window shortcut: `Mod+Alt+Enter` while the card
    /// has focus (⇧⌘↵ is Run all; DESIGN.md's ⌘⇧↵ collided).
    ApproveProposal(ProposalId),
    /// Deny (the card's default button).
    DenyProposal(ProposalId),
    /// Run SQL from an editor tab on the human's connection.
    RunQuery {
        tab: TabId,
        scope: RunScope,
    },
    /// EXPLAIN ANALYZE (executes the statement) on the human's connection.
    ExplainAnalyze {
        tab: TabId,
    },
    /// Commit a tab's pending edits in one transaction (after the diff).
    CommitEdits {
        tab: TabId,
    },
    /// Save a connection from the dialog (may change its agent access).
    SaveConnection(Box<ConnectionSpec>),
    DeleteConnection(ConnectionId),
    PauseAgent,
    ResumeAgent,
    /// Mark or unmark a column sensitive (redacted for agents).
    MarkSensitive {
        connection: ConnectionId,
        table: TableRef,
        column: String,
        sensitive: bool,
    },
}

impl HumanAction {
    /// The action's id (for the audit log, the journal-like agent panel and the docs).
    pub fn id(&self) -> &'static str {
        match self {
            HumanAction::ApproveProposal(_) => ids::PROPOSAL_APPROVE,
            HumanAction::DenyProposal(_) => ids::PROPOSAL_DENY,
            HumanAction::RunQuery { scope: RunScope::All, .. } => ids::QUERY_RUN_ALL,
            HumanAction::RunQuery { .. } => ids::QUERY_RUN,
            HumanAction::ExplainAnalyze { .. } => ids::QUERY_EXPLAIN_ANALYZE,
            HumanAction::CommitEdits { .. } => ids::EDIT_COMMIT,
            HumanAction::SaveConnection(_) => ids::CONN_SAVE,
            HumanAction::DeleteConnection(_) => ids::CONN_DELETE,
            HumanAction::PauseAgent => ids::AGENT_PAUSE,
            HumanAction::ResumeAgent => ids::AGENT_RESUME,
            HumanAction::MarkSensitive { .. } => ids::COLUMN_MARK_SENSITIVE,
        }
    }
}

/// Run a human-only action. The ticket is consumed: one click, one action. M0: stub.
pub fn perform(_session: &mut Session<Workspace>, _ticket: HumanTicket, action: HumanAction) -> Result<Value, CommandError> {
    log::info!("human action {}", action.id());
    match action {
        HumanAction::PauseAgent | HumanAction::ResumeAgent => {
            let paused = matches!(action, HumanAction::PauseAgent);
            crate::services::services().set_agent_paused(paused);
            Ok(json!({"paused": paused}))
        }
        _ => Err(CommandError::failed(action.id(), Refusal::new(RefusalCode::NotYet, "not yet").to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_maps_to_a_human_only_id() {
        let actions = [
            HumanAction::ApproveProposal(ProposalId(1)),
            HumanAction::DenyProposal(ProposalId(1)),
            HumanAction::RunQuery { tab: TabId(1), scope: RunScope::Statement },
            HumanAction::RunQuery { tab: TabId(1), scope: RunScope::All },
            HumanAction::ExplainAnalyze { tab: TabId(1) },
            HumanAction::CommitEdits { tab: TabId(1) },
            HumanAction::DeleteConnection(ConnectionId::new("x")),
            HumanAction::PauseAgent,
            HumanAction::ResumeAgent,
            HumanAction::MarkSensitive { connection: ConnectionId::new("x"), table: TableRef::new(None, "t"), column: "c".into(), sensitive: true },
        ];
        for a in &actions {
            assert!(ids::HUMAN_ONLY.contains(&a.id()), "{a:?}");
        }
    }

    #[test]
    fn no_ticket_without_a_click() {
        let ctx = egui::Context::default();
        let mut got = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let r = ui.button("Approve");
            got = Some(HumanTicket::from_click(ui.ctx(), &r).is_some());
        });
        out.textures_delta.clear();
        assert_eq!(got, Some(false));
    }

    #[test]
    fn perform_is_stubbed() {
        let mut s = crate::app().headless();
        let e = perform(s.session_mut(), HumanTicket::for_tests(), HumanAction::DenyProposal(ProposalId(1))).unwrap_err();
        assert_eq!(e.id(), ids::PROPOSAL_DENY);
    }
}

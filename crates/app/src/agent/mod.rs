//! The agent panel: transcript of tool calls and approval cards (`agent` stream). M0: stub card.

use ashlar::egui;
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Role};
use quarry_core::{ProposalId, Workspace};

use crate::human::{self, HumanAction, HumanTicket};

pub fn show(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
    let paused = crate::services::services().agent_paused() || cx.view_or("agent_state", String::new()) == "paused";
    ui.horizontal(|ui| {
        ui.strong("Agent");
        ui.weak(if paused { "paused" } else { "via MCP · read-only" });
    });
    ui.separator();
    let proposal: Option<String> = cx.view("proposal");
    let Some(state) = proposal else {
        ui.weak("No tool calls yet.");
        return;
    };
    let r = ui.group(|ui| {
        ui.strong("Agent wants to change data");
        ui.monospace("UPDATE orders SET status = 'refunded' WHERE id = 42");
        ui.weak("estimate ~1 row · runs in one transaction · logged in the audit log");
        if state == "pending" {
            ui.horizontal(|ui| {
                let deny = ui.button("Deny");
                automation::register(ui, &deny, "quarry.approval.deny", "Deny", Role::Button);
                let approve = ui.button("Approve");
                automation::register(ui, &approve, "quarry.approval.approve", "Approve", Role::Button);
                let ctx = ui.ctx().clone();
                if let Some(t) = HumanTicket::from_click(&ctx, &deny) {
                    let _ = human::perform(cx.session_mut(), t, HumanAction::DenyProposal(ProposalId(1)));
                }
                if let Some(t) = HumanTicket::from_click(&ctx, &approve) {
                    let _ = human::perform(cx.session_mut(), t, HumanAction::ApproveProposal(ProposalId(1)));
                }
            });
        } else {
            ui.label(format!("Proposal {state}"));
        }
    });
    automation::register(ui, &r.response, "quarry.approval", "Approval request", Role::Alert);
}

//! The agent panel (DESIGN.md, right, 360): header with the pause switch, approval cards for
//! pending proposals, recent decisions, and the activity list with jump-to.
//!
//! Approve, Deny, Pause and Resume are human-only: they run only through `human::perform` with a
//! `HumanTicket` minted from this frame's real click or key press, never while the control port is
//! injecting input. An approved write then runs in a job ([`approval::execute`]).

use ashlar::egui::{self, Color32, CornerRadius, Key, KeyboardShortcut, Modifiers, RichText, Sense, Stroke, vec2};
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Role};
use ashlar::ui::code_view::CodeView;
use quarry_core::{ConnColor, Proposal, ProposalId, ProposalOutcome, ProposalStatus, Tab, TabId, TabKind, Timestamp, Workspace};

use super::activity::{self, Jump};
use super::approval;
use crate::human::{self, HumanAction, HumanTicket};
use crate::services::{now, services};

use crate::theme;

/// Approve while the card has focus (⇧⌘↵ is Run all; see `crate::human::HumanAction`).
pub const APPROVE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::ALT), Key::Enter);
/// Decided proposals shown under the pending ones.
const RECENT_DECIDED: usize = 8;
/// Activity rows shown.
const ACTIVITY_ROWS: usize = 100;

/// View-state key `proposal.focus` sets: focus the newest pending card's Deny button.
pub const FOCUS_KEY: &str = "proposal_focus";

/// Approve's keys as the person reads them (a scene pins macOS: `theme::platform`).
fn shortcut_label(scene: bool) -> &'static str {
    match theme::platform(scene) {
        ashlar::command::shortcut::Platform::Mac => "Cmd+Option+Enter",
        ashlar::command::shortcut::Platform::Other => "Ctrl+Alt+Enter",
    }
}

/// What the panel draws: the live proposals and activity, or a scene's fixed demo.
pub(crate) struct Model {
    pub proposals: Vec<Proposal>,
    pub cards: std::collections::HashMap<ProposalId, approval::Card>,
    pub activity: Vec<activity::Activity>,
    pub now: Timestamp,
    /// A scene: buttons do nothing.
    pub demo: bool,
}

impl Model {
    fn live() -> Self {
        approval::expire_due(now());
        let mut proposals: Vec<Proposal> = services().proposals().iter().cloned().collect();
        proposals.reverse();
        proposals.truncate(64);
        let cards = proposals.iter().filter_map(|p| approval::card(p.id).map(|c| (p.id, c))).collect();
        Self { proposals, cards, activity: activity::latest(ACTIVITY_ROWS), now: now(), demo: false }
    }
}

/// View-state key of the agent scenes: the scene whose demo the panel shows.
pub const SCENE_KEY: &str = "agent.scene";

pub fn show(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
    let scene: Option<String> = cx.view::<String>(SCENE_KEY).filter(|s| !s.is_empty());
    let m = match scene.as_deref() {
        Some(name) => super::scenes::model(name),
        None => Model::live(),
    };
    let paused = services().agent_paused();
    // scenes show the paused look without pausing anything
    let shown_paused = paused || cx.view_or("agent_state", String::new()) == "paused";
    header(ui, cx, &m, paused, shown_paused);
    ui.add_space(4.0);
    ui.separator();
    let (pending, decided): (Vec<&Proposal>, Vec<&Proposal>) = m.proposals.iter().partition(|p| p.status == ProposalStatus::Pending);
    // ⌘⇧A: focus the newest card's Deny, for a few frames (other regions may claim focus as the
    // panel opens)
    let focus_id = egui::Id::new("quarry.agent.focus_frames");
    if cx.view_or(FOCUS_KEY, false) {
        cx.set_view(FOCUS_KEY, &false);
        ui.ctx().data_mut(|d| d.insert_temp(focus_id, 3u8));
    }
    let frames: u8 = ui.ctx().data(|d| d.get_temp(focus_id)).unwrap_or(0);
    let focus_newest = frames > 0;
    if focus_newest {
        ui.ctx().data_mut(|d| d.insert_temp(focus_id, frames - 1));
        ui.ctx().request_repaint();
    }
    let scroll_to: Option<ProposalId> = ui.ctx().data_mut(|d| d.remove_temp::<Option<ProposalId>>(egui::Id::new("quarry.agent.scroll_to"))).flatten();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        for (i, p) in pending.iter().enumerate() {
            pending_card(ui, cx, &m, p, i == 0, focus_newest && i == 0, scroll_to == Some(p.id));
        }
        for p in decided.iter().take(RECENT_DECIDED) {
            decided_card(ui, cx, &m, p, scroll_to == Some(p.id));
        }
        ui.add_space(4.0);
        activity_list(ui, cx, &m);
    });
    if m.demo {
        return;
    }
    if let Some(next) = pending.iter().map(|p| p.expires_at).min() {
        // the countdown, and the expiry itself
        let ms = next.0.saturating_sub(m.now.0).clamp(50, 1_000);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(ms));
    }
    if decided.iter().any(|p| matches!(p.status, ProposalStatus::Approved | ProposalStatus::Running)) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn header(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, m: &Model, paused: bool, shown_paused: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Agent").strong().color(theme::TEXT));
        ui.label(RichText::new("via MCP").color(theme::MUTED));
        let (text, color) = if shown_paused { ("paused", theme::WARN) } else { ("read-only", theme::OK) };
        quarry_viz::chip(ui, text, color);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if shown_paused { "Resume" } else { "Pause" };
            let b = ui.button(label);
            automation::register(ui, &b, "quarry.agent.pause", label, Role::Toggle);
            let ctx = ui.ctx().clone();
            if let Some(t) = HumanTicket::from_click(&ctx, &b).filter(|_| !m.demo) {
                let action = if paused { HumanAction::ResumeAgent } else { HumanAction::PauseAgent };
                if let Err(e) = human::perform(cx.session_mut(), t, action) {
                    log::warn!("{e}");
                }
            }
        });
    });
    let pending = m.proposals.iter().filter(|p| p.status == ProposalStatus::Pending).count();
    let line = if shown_paused {
        "Tool calls are refused until you resume.".to_string()
    } else if pending > 0 {
        format!("{pending} change{} waiting for you", if pending == 1 { "" } else { "s" })
    } else {
        "Reads, charts and UI only. Writes need your approval.".to_string()
    };
    let r = ui.label(RichText::new(line).color(theme::MUTED).small());
    automation::register(ui, &r, "quarry.agent.state", if shown_paused { "paused" } else { "active" }, Role::Status);
}

fn conn_color(c: ConnColor) -> Color32 {
    match c {
        ConnColor::Prod => theme::ERROR,
        ConnColor::Staging => theme::WARN,
        ConnColor::Local => theme::OK,
        ConnColor::None => theme::MUTED,
    }
}

fn lock_icon(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
    let p = ui.painter();
    p.circle_stroke(r.center_top() + vec2(0.0, 6.0), 3.5, Stroke::new(1.6, color));
    p.rect_filled(egui::Rect::from_min_size(r.min + vec2(1.5, 7.0), vec2(11.0, 8.0)), CornerRadius::same(2), color);
}

fn countdown(until: Timestamp, now: Timestamp) -> String {
    let s = until.0.saturating_sub(now.0) / 1_000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Register `r` under the card's own id and, for the newest pending card, the plain alias.
fn register(ui: &egui::Ui, r: &egui::Response, id: ProposalId, part: &str, alias: bool, label: &str, role: Role) {
    let suffix = if part.is_empty() { String::new() } else { format!(".{part}") };
    automation::register(ui, r, &format!("quarry.approval.{}{suffix}", id.0), label, role);
    if alias {
        automation::register(ui, r, &format!("quarry.approval{suffix}"), label, role);
    }
}

fn pending_card(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, m: &Model, p: &Proposal, alias: bool, focus: bool, scroll: bool) {
    let card = m.cards.get(&p.id).cloned();
    let ctx = ui.ctx().clone();
    let ids_key = egui::Id::new(("quarry.approval.buttons", p.id.0));
    let prev: Option<(egui::Id, egui::Id)> = ctx.data(|d| d.get_temp(ids_key));
    let focused = ctx.memory(|m| m.focused());
    // the shortcut is taken before the buttons exist, so its Enter never also presses Deny
    let key_ticket =
        if prev.is_some_and(|(a, b)| focused == Some(a) || focused == Some(b)) { HumanTicket::from_shortcut(&ctx, &APPROVE_SHORTCUT) } else { None };
    let r = egui::Frame::new()
        .fill(theme::RAISED)
        .stroke(Stroke::new(1.0, theme::WARN))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                lock_icon(ui, theme::WARN);
                ui.label(RichText::new("Agent wants to change data").strong().color(theme::TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (name, color) =
                        card.as_ref().map_or((p.connection.to_string(), theme::MUTED), |c| (c.connection_name.clone(), conn_color(c.color)));
                    quarry_viz::chip(ui, &name, color);
                });
            });
            ui.label(RichText::new(format!("\u{201C}{}\u{201D}", p.reason)).italics().color(theme::MUTED));
            egui::Frame::new().fill(theme::BG).corner_radius(CornerRadius::same(4)).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let id = format!("quarry.approval.sql.{}", p.id.0);
                // the exact text the agent sent, highlighted, never reformatted
                let lines = p.sql.lines().count().max(1) as f32 + p.sql.len() as f32 / 40.0;
                CodeView::new(&id, &p.sql).lang(Some("sql")).wrap(true).max_height((lines * 16.0 + 8.0).min(180.0)).show(ui);
            });
            facts(ui, p, card.as_ref(), m.now);
            if let Some(pv) = card.as_ref().and_then(|c| c.preview.as_ref()) {
                preview(ui, p.id, pv);
            }
            ui.label(RichText::new("Runs in one transaction · logged in the audit log · undo when possible").color(theme::MUTED).small());
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let deny = ui.add(egui::Button::new(RichText::new("Deny").color(theme::TEXT)).min_size(vec2(72.0, 28.0)));
                register(ui, &deny, p.id, "deny", alias, "Deny", Role::Button);
                let approve = ui
                    .add(egui::Button::new(RichText::new("Approve").color(theme::ON_ACCENT).strong()).fill(theme::ACCENT).min_size(vec2(88.0, 28.0)));
                register(ui, &approve, p.id, "approve", alias, "Approve", Role::Button);
                ctx.data_mut(|d| d.insert_temp(ids_key, (deny.id, approve.id)));
                if focus || (focused.is_none() && prev.is_none() && alias) {
                    deny.request_focus();
                }
                if m.demo {
                    // a scene: nothing to decide
                } else if let Some(t) = HumanTicket::from_click(&ctx, &deny) {
                    if let Err(e) = human::perform(cx.session_mut(), t, HumanAction::DenyProposal(p.id)) {
                        log::warn!("{e}");
                    }
                } else if let Some(t) = HumanTicket::from_click(&ctx, &approve).or(key_ticket) {
                    approve_and_run(cx, t, p.id);
                }
            });
            ui.label(RichText::new(format!("Deny is the default · {} approves", shortcut_label(m.demo))).color(theme::MUTED).small());
        });
    register(ui, &r.response, p.id, "", alias, "Agent wants to change data", Role::Alert);
    if scroll {
        r.response.scroll_to_me(Some(egui::Align::Center));
    }
}

/// Approve (human only) and run the write in a job.
fn approve_and_run(cx: &mut Cx<'_, Workspace>, t: HumanTicket, id: ProposalId) {
    match human::perform(cx.session_mut(), t, HumanAction::ApproveProposal(id)) {
        Ok(_) => {
            cx.jobs.job("quarry.approved_write", format!("Run approved change {}", id.0)).spawn(id, |id, _| {
                let status = approval::execute(id);
                Ok(apply(move |_: &mut Session<Workspace>| Ok(json!({"proposal_id": id, "status": status}))))
            });
        }
        Err(e) => log::warn!("{e}"),
    }
}

fn facts(ui: &mut egui::Ui, p: &Proposal, card: Option<&approval::Card>, now: Timestamp) {
    egui::Grid::new(("quarry.approval.facts", p.id.0)).num_columns(2).min_row_height(18.0).spacing(vec2(12.0, 2.0)).show(ui, |ui| {
        let key = |ui: &mut egui::Ui, k: &str| {
            ui.label(RichText::new(k).color(theme::MUTED).small());
        };
        key(ui, "Kind");
        ui.label(RichText::new(card.map_or("change", |c| c.kind.as_str())).color(theme::TEXT).small());
        ui.end_row();
        key(ui, "Tables");
        let tables: Vec<String> = p.tables.iter().map(ToString::to_string).collect();
        ui.label(RichText::new(if tables.is_empty() { "unknown".into() } else { tables.join(", ") }).monospace().color(theme::TEXT).small());
        ui.end_row();
        key(ui, "Estimate");
        match p.estimate {
            Some(e) => {
                ui.label(RichText::new(format!("~{} row{} (counted)", e.rows, if e.rows == 1 { "" } else { "s" })).color(theme::TEXT).small());
                ui.end_row();
                key(ui, "Check");
                ui.label(
                    RichText::new(format!("rolls back outside {}–{} rows", e.rows.saturating_sub(e.tolerance), e.rows.saturating_add(e.tolerance)))
                        .color(theme::TEXT)
                        .small(),
                );
            }
            None => {
                let note = card.and_then(|c| c.estimate_note.clone()).unwrap_or_else(|| "no estimate".into());
                ui.label(RichText::new(format!("{note}; runs without a row check")).color(theme::WARN).small());
            }
        }
        ui.end_row();
        if card.is_some_and(|c| c.unbounded) {
            key(ui, "Scope");
            ui.label(RichText::new("every row: no WHERE").color(theme::ERROR).strong().small());
            ui.end_row();
        }
        key(ui, "Expires");
        ui.label(RichText::new(format!("in {}", countdown(p.expires_at, now))).color(theme::MUTED).small());
        ui.end_row();
    });
}

fn preview(ui: &mut egui::Ui, id: ProposalId, pv: &approval::Preview) {
    ui.label(RichText::new(format!("Rows it touches ({})", pv.rows.len())).color(theme::MUTED).small());
    egui::ScrollArea::horizontal().id_salt(("quarry.approval.preview", id.0)).max_height(140.0).show(ui, |ui| {
        egui::Grid::new(("quarry.approval.preview.grid", id.0)).striped(true).min_row_height(18.0).spacing(vec2(10.0, 2.0)).show(ui, |ui| {
            for c in &pv.columns {
                ui.label(RichText::new(c).monospace().color(theme::MUTED).small());
            }
            ui.end_row();
            for row in &pv.rows {
                for v in row {
                    ui.label(RichText::new(v).monospace().color(theme::TEXT).small());
                }
                ui.end_row();
            }
        });
    });
}

fn status_color(s: ProposalStatus) -> Color32 {
    match s {
        ProposalStatus::Pending => theme::WARN,
        ProposalStatus::Approved | ProposalStatus::Running => theme::ACCENT,
        ProposalStatus::Done => theme::OK,
        ProposalStatus::Failed => theme::ERROR,
        ProposalStatus::Denied | ProposalStatus::Expired => theme::MUTED,
    }
}

fn decided_card(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, m: &Model, p: &Proposal, scroll: bool) {
    let card = m.cards.get(&p.id).cloned();
    let r = egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                quarry_viz::chip(ui, p.status.as_str(), status_color(p.status));
                let line: String = p.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                let short: String = line.chars().take(48).collect();
                ui.label(RichText::new(if line.chars().count() > 48 { format!("{short}…") } else { short }).monospace().color(theme::MUTED).small());
            });
            match &p.result {
                Some(ProposalOutcome::Done { rows_affected }) => {
                    ui.label(
                        RichText::new(format!("{rows_affected} row{} changed", if *rows_affected == 1 { "" } else { "s" }))
                            .color(theme::TEXT)
                            .small(),
                    );
                }
                Some(ProposalOutcome::Failed { error }) => {
                    ui.label(RichText::new(format!("rolled back: {error}")).color(theme::ERROR).small());
                }
                None => {}
            }
            if let Some(undo) = card.and_then(|c| c.undo).filter(|u| !u.is_empty()) {
                let b = ui.small_button("Open undo SQL in an editor");
                automation::register(ui, &b, &format!("quarry.approval.{}.undo", p.id.0), "Open undo SQL", Role::Button);
                if b.clicked() {
                    // shown, not run: running it is the person's own ⌘↵
                    let sql = format!("{};\n", undo.join(";\n"));
                    let connection = Some(p.connection.clone());
                    cx.edit("Undo SQL in Editor", |w| {
                        let id = TabId(w.next_id());
                        w.tabs.push(Tab { id, title: format!("Undo change {}", p.id.0), connection, kind: TabKind::Editor { sql } });
                        w.active = Some(id);
                    });
                }
            }
        });
    automation::register(ui, &r.response, &format!("quarry.approval.{}", p.id.0), p.status.as_str(), Role::Group);
    if scroll {
        r.response.scroll_to_me(Some(egui::Align::Center));
    }
}

fn clock(t: Timestamp) -> String {
    let s = (t.0 / 1_000) % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3_600, (s / 60) % 60, s % 60)
}

fn activity_list(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, m: &Model) {
    ui.label(RichText::new("Activity").strong().color(theme::TEXT));
    let items = &m.activity;
    if items.is_empty() {
        let r = ui.label(RichText::new("No tool calls yet.").color(theme::MUTED));
        automation::register(ui, &r, "quarry.agent.activity.empty", "No tool calls yet", Role::Label);
        return;
    }
    for a in items {
        let r = ui
            .horizontal(|ui| {
                ui.label(RichText::new(clock(a.at)).monospace().color(theme::MUTED).small());
                ui.label(RichText::new(a.tool).monospace().color(if a.refused.is_some() { theme::ERROR } else { theme::ACCENT }).small());
                let tail = match (&a.refused, a.rows) {
                    (Some(r), _) => r.split(':').next().unwrap_or_default().to_string(),
                    (None, Some(n)) => format!("{n} row{} · {} ms", if n == 1 { "" } else { "s" }, a.elapsed_ms),
                    (None, None) => format!("{} ms", a.elapsed_ms),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(tail).color(theme::MUTED).small());
                    let short: String = a.summary.chars().take(40).collect();
                    ui.add(egui::Label::new(RichText::new(short).color(theme::TEXT).small()).truncate());
                });
            })
            .response
            .interact(Sense::click());
        let r = if let Some(why) = &a.refused { r.on_hover_text(why.clone()) } else { r.on_hover_text(a.summary.clone()) };
        let a = a.clone();
        automation::register(ui, &r, &format!("quarry.agent.activity.{}", a.seq), a.tool, Role::Row);
        if r.clicked()
            && let Some(j) = a.jump
        {
            jump(ui, cx, j);
        }
    }
}

fn jump(ui: &egui::Ui, cx: &mut Cx<'_, Workspace>, j: Jump) {
    let tab = match j {
        Jump::Tab(t) => Some(t),
        Jump::Panel(p) => cx.doc().charts.iter().find(|c| c.panel == p).and_then(|c| c.tab),
        Jump::Proposal(id) => {
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("quarry.agent.scroll_to"), Some(id)));
            None
        }
    };
    if let Some(t) = tab.filter(|t| cx.doc().tab(*t).is_some()) {
        cx.edit("Select Tab", |w| w.active = Some(t));
    }
}

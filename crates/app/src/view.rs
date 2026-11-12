//! The main window: toolbar, sidebar, center tabs, agent panel, status bar (DESIGN.md). Owned by
//! the `shell` stream; each region is drawn by its stream's module.

use ashlar::egui;
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Role};
use quarry_core::Workspace;

use crate::human::{self, HumanAction, HumanTicket, RunScope};
use crate::{agent, dialogs, editor, grid, ids, sidebar, theme};

pub fn view() -> impl FnMut(&mut egui::Ui, &mut Cx<'_, Workspace>) {
    move |ui, cx| {
        let screen: String = cx.view_or("screen", String::new());
        toolbar(ui, cx);
        egui::Panel::bottom("quarry.status").exact_size(theme::STATUS_BAR).show(ui, |ui| {
            let r = ui.weak("rows — · no connection · agent: read-only · built with ashlar");
            automation::register(ui, &r, "quarry.status", "Status", Role::Status);
        });
        egui::Panel::left("quarry.sidebar").default_size(theme::SIDEBAR).show(ui, |ui| sidebar::show(ui, cx));
        if cx.view_or("agent_panel", false) {
            egui::Panel::right("quarry.agent").exact_size(theme::AGENT_PANEL).show(ui, |ui| agent::show(ui, cx));
        }
        if !screen.is_empty() {
            let r = ui.weak(format!("screen: {screen}"));
            automation::register(ui, &r, &format!("quarry.screen.{screen}"), &screen, Role::Label);
        }
        if cx.view_or("dialog", String::new()) == "connection" {
            dialogs::show_connection(ui);
        }
        center(ui, cx);
    }
}

fn toolbar(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
    egui::Panel::top("quarry.toolbar").exact_size(theme::TOOLBAR).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            let active = cx.doc().active;
            let run = ui.add_enabled(active.is_some(), egui::Button::new("Run"));
            automation::register(ui, &run, "quarry.toolbar.run", "Run", Role::Button);
            let ctx = ui.ctx().clone();
            if let (Some(tab), Some(t)) = (active, HumanTicket::from_click(&ctx, &run))
                && let Err(e) = human::perform(cx.session_mut(), t, HumanAction::RunQuery { tab, scope: RunScope::Statement })
            {
                log::info!("{e}");
            }
            for (id, label, aid) in [
                (ids::QUERY_CANCEL, "Cancel", "quarry.toolbar.cancel"),
                (ids::QUERY_EXPLAIN, "Explain", "quarry.toolbar.explain"),
                (ids::HISTORY_SHOW, "History", "quarry.toolbar.history"),
                (ids::AGENT_TOGGLE_PANEL, "Agent", "quarry.toolbar.agent"),
            ] {
                let r = ui.button(label);
                automation::register(ui, &r, aid, label, Role::Button);
                if r.clicked() {
                    cx.dispatch(id, json!({}));
                }
            }
        });
    });
}

fn center(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
    let doc = cx.snapshot();
    let Some(tab) = doc.active.and_then(|id| doc.tab(id)) else {
        ui.vertical_centered(|ui| {
            ui.add_space(120.0);
            let r = ui.heading("Connect to a database");
            automation::register(ui, &r, "quarry.empty", "Connect to a database", Role::Label);
            ui.weak("PostgreSQL or SQLite. The agent gets its own read-only connection.");
            let b = ui.button("New Connection…");
            automation::register(ui, &b, "quarry.empty.new_connection", "New Connection…", Role::Button);
            if b.clicked() {
                cx.dispatch(ids::CONN_NEW, json!({}));
            }
        });
        return;
    };
    ui.horizontal(|ui| {
        for t in &doc.tabs {
            let r = ui.selectable_label(Some(t.id) == doc.active, &t.title);
            automation::register(ui, &r, &format!("quarry.tab.{}", t.id), &t.title, Role::Tab);
            if r.clicked() {
                cx.dispatch(ids::TAB_SELECT, json!({"tab": t.id}));
            }
        }
    });
    ui.separator();
    editor::show(ui, tab);
    ui.separator();
    grid::show(ui, &cx.view_or("query_state", String::new()));
    for chart in doc.charts.iter().filter(|c| c.tab == Some(tab.id)) {
        quarry_viz::chart(ui, chart);
    }
}

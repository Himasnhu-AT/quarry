//! SQL editor tabs (`shell` stream). M0: shows the tab's SQL read-only.

use ashlar::egui;
use quarry_core::{Tab, TabKind};

pub fn show(ui: &mut egui::Ui, tab: &Tab) {
    match &tab.kind {
        TabKind::Editor { sql } if sql.is_empty() => {
            ui.weak("-- write SQL here");
        }
        TabKind::Editor { sql } => {
            ui.monospace(sql);
        }
        TabKind::Table { table, .. } => {
            ui.monospace(format!("-- table {table}"));
        }
    }
}

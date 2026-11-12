//! The virtualized result grid (`grid` stream). M0: a placeholder.

use ashlar::egui;
use ashlar::ui::automation::{self, Role};

pub fn show(ui: &mut egui::Ui, state: &str) {
    let r = ui.weak(match state {
        "running" => "Running… (Cancel ⌘.)",
        "error" => "ERROR: relation \"orderz\" does not exist (line 1)",
        _ => "No results yet. Run a statement with ⌘↵.",
    });
    automation::register(ui, &r, "quarry.grid", "Results", Role::Table);
}

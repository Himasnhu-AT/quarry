//! The connection dialog and other dialogs (`shell` stream). M0: a placeholder window.

use ashlar::egui;
use ashlar::ui::automation::{self, Role};

pub fn show_connection(ui: &mut egui::Ui) {
    ui.group(|ui| {
        let r = ui.heading("New connection");
        automation::register(ui, &r, "quarry.dialog.connection", "New connection", Role::Dialog);
        ui.label("Engine: PostgreSQL / SQLite");
        ui.label("Agent access: Read-only (default)");
    });
}

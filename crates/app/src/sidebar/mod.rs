//! Connection switcher and schema tree (`shell` stream). M0: empty state.

use ashlar::egui;
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Role};
use quarry_core::Workspace;

use crate::ids;

pub fn show(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
    ui.heading("Connections");
    ui.weak("No connections yet.");
    let r = ui.button("New Connection…");
    automation::register(ui, &r, "quarry.sidebar.new_connection", "New Connection…", Role::Button);
    if r.clicked() {
        cx.dispatch(ids::CONN_NEW, json!({}));
    }
}

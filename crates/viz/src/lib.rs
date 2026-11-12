//! Charts and pivot tables (M4: `agent` stream). Rank 2. Stub: a titled placeholder.

use ashlar::egui;
use quarry_core::ChartSpec;

/// Draw a chart panel. M0 draws its title and kind only.
pub fn chart(ui: &mut egui::Ui, spec: &ChartSpec) -> egui::Response {
    ui.group(|ui| {
        ui.label(&spec.title);
        ui.weak(format!("{:?} chart of {} (not drawn yet)", spec.kind, spec.x));
    })
    .response
}

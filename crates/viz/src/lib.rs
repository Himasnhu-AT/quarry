//! Charts and pivot tables drawn with the egui painter (no web view). Rank 2.
//!
//! - [`data`]: rows → [`ChartData`] / [`PivotData`] (pure, tested).
//! - [`store`]: what each panel draws, by [`PanelId`]. A chart's spec (its SQL, columns, kind) is
//!   pinned in the workspace document; its rows are runtime data, filled when the query runs
//!   (`viz.chart`, `viz.pivot`, or a refresh from the window) and never saved.
//! - [`chart`]: one chart or pivot panel: title, "made by agent" chip, the plot.

pub mod data;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use ashlar::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, pos2, vec2};
use ashlar::ui::automation::{self, Role};
use quarry_core::{ChartKind, ChartSpec, PanelId};

pub use data::{Agg, ChartData, PivotData, Series, XAxis};

/// What a panel draws.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelData {
    Chart(ChartData),
    Pivot(PivotData),
}

/// Most panels with data in memory (the oldest is dropped first).
const MAX_PANELS: usize = 256;

#[derive(Default)]
struct Store {
    seq: u64,
    panels: HashMap<PanelId, (u64, Arc<PanelData>)>,
}

fn store_lock() -> std::sync::MutexGuard<'static, Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner)
}

/// The data panels draw, by panel id (process-wide: the document holds only the spec).
pub mod store {
    use super::*;

    /// Keep `data` for `panel`.
    pub fn put(panel: PanelId, data: PanelData) {
        let mut s = store_lock();
        s.seq = s.seq.saturating_add(1);
        let seq = s.seq;
        s.panels.insert(panel, (seq, Arc::new(data)));
        if s.panels.len() > MAX_PANELS
            && let Some(oldest) = s.panels.iter().min_by_key(|(_, (n, _))| *n).map(|(k, _)| *k)
        {
            s.panels.remove(&oldest);
        }
    }

    pub fn get(panel: PanelId) -> Option<Arc<PanelData>> {
        store_lock().panels.get(&panel).map(|(_, d)| Arc::clone(d))
    }

    pub fn remove(panel: PanelId) {
        store_lock().panels.remove(&panel);
    }
}

// DESIGN.md tokens (the viz crate does not depend on the app's theme module).
const PANEL: Color32 = Color32::from_rgb(0x16, 0x1A, 0x20);
const BORDER: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3A);
const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEB);
const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA3, 0xAE);
const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x9D, 0xFF);
const INFO: Color32 = Color32::from_rgb(0x8F, 0xB8, 0xFF);

/// Series colors: the accent first, then hues that stay apart on the dark panel.
pub const PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x5B, 0x9D, 0xFF),
    Color32::from_rgb(0x3C, 0xC8, 0xA0),
    Color32::from_rgb(0xF2, 0xA9, 0x3B),
    Color32::from_rgb(0xC3, 0x8B, 0xF5),
    Color32::from_rgb(0xF2, 0x7A, 0x9A),
    Color32::from_rgb(0x5C, 0xD3, 0xE6),
    Color32::from_rgb(0xD9, 0xD2, 0x6A),
    Color32::from_rgb(0xA8, 0xB3, 0xC2),
];

fn series_color(i: usize) -> Color32 {
    PALETTE.get(i % PALETTE.len()).copied().unwrap_or(ACCENT)
}

/// Draw a chart or pivot panel for `spec` with the data in [`store`] (a note when there is none
/// yet). Automation ids: `quarry.chart.<panel>`.
pub fn chart(ui: &mut egui::Ui, spec: &ChartSpec) -> egui::Response {
    let data = store::get(spec.panel);
    let r = egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&spec.title).color(TEXT).strong());
                if spec.by_agent {
                    chip(ui, "made by agent", INFO);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(kind_name(spec.kind)).color(MUTED).small());
                });
            });
            ui.label(egui::RichText::new(one_line(&spec.sql, 120)).monospace().color(MUTED).small());
            ui.add_space(6.0);
            match data.as_deref() {
                Some(PanelData::Chart(d)) => {
                    plot(ui, d, vec2(ui.available_width(), 220.0));
                    notes(ui, d.skipped, d.capped);
                }
                Some(PanelData::Pivot(p)) => {
                    pivot(ui, p);
                    notes(ui, p.skipped, p.capped);
                }
                None => {
                    ui.label(egui::RichText::new("No rows loaded: the query runs again when the panel is refreshed.").color(MUTED));
                }
            }
        })
        .response;
    automation::register(ui, &r, &format!("quarry.chart.{}", spec.panel), &spec.title, Role::Group);
    r
}

fn notes(ui: &mut egui::Ui, skipped: usize, capped: bool) {
    let mut parts = Vec::new();
    if skipped > 0 {
        parts.push(format!("{skipped} rows without a usable value"));
    }
    if capped {
        parts.push("more categories than shown".to_string());
    }
    if !parts.is_empty() {
        ui.label(egui::RichText::new(parts.join(" · ")).color(MUTED).small());
    }
}

fn kind_name(k: ChartKind) -> &'static str {
    match k {
        ChartKind::Bar => "bar",
        ChartKind::Line => "line",
        ChartKind::Area => "area",
        ChartKind::Scatter => "scatter",
        ChartKind::Histogram => "histogram",
        ChartKind::Pivot => "pivot",
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max { format!("{}…", flat.chars().take(max).collect::<String>()) } else { flat }
}

/// A small rounded label.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let font = FontId::proportional(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = galley.size() + vec2(12.0, 4.0);
    let (rect, r) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(4), color.gamma_multiply(0.14), Stroke::new(1.0, color.gamma_multiply(0.5)), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    r
}

/// Round `x` to 1, 2 or 5 × a power of ten.
fn nice(x: f64) -> f64 {
    if !(x.is_finite() && x > 0.0) {
        return 1.0;
    }
    let p = 10f64.powf(x.log10().floor());
    let f = x / p;
    p * if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    }
}

/// Tick values covering `lo..hi` (about `n` of them) and the widened range.
fn ticks(lo: f64, hi: f64, n: usize) -> (Vec<f64>, f64, f64) {
    let (lo, hi) = if !(lo.is_finite() && hi.is_finite()) {
        (0.0, 1.0)
    } else if hi > lo {
        (lo, hi)
    } else {
        (lo - 1.0, hi + 1.0)
    };
    let step = nice((hi - lo) / n.max(1) as f64);
    let start = (lo / step).floor() * step;
    let end = (hi / step).ceil() * step;
    let mut out = Vec::new();
    let mut t = start;
    while t <= end + step * 0.5 && out.len() < 50 {
        out.push(t);
        t += step;
    }
    (out, start, end)
}

fn fmt_num(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if (v - v.round()).abs() < 1e-9 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// Draw `d` into a `size` area: axes, grid, marks, legend, and a readout of the point under the
/// pointer.
pub fn plot(ui: &mut egui::Ui, d: &ChartData, size: Vec2) -> egui::Response {
    if d.series.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            for (i, s) in d.series.iter().enumerate() {
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(2), series_color(i));
                ui.label(egui::RichText::new(&s.name).color(MUTED).small());
            }
        });
    }
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let plot = Rect::from_min_max(pos2(rect.left() + 52.0, rect.top() + 6.0), pos2(rect.right() - 8.0, rect.bottom() - 24.0));
    let small = FontId::monospace(10.0);

    let points = d.series.iter().flat_map(|s| s.points.iter());
    let bars = matches!(d.kind, ChartKind::Bar | ChartKind::Histogram);
    let (mut ylo, mut yhi) = points.clone().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| (a.min(p[1]), b.max(p[1])));
    if bars || d.kind == ChartKind::Area {
        ylo = ylo.min(0.0);
        yhi = yhi.max(0.0);
    }
    let (yt, ylo, yhi) = ticks(ylo, yhi, 5);
    let (xlo, xhi) = match &d.x {
        XAxis::Category(c) => (-0.5, c.len() as f64 - 0.5),
        XAxis::Number { .. } => {
            let (a, b) = points.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| (a.min(p[0]), b.max(p[0])));
            let half = d.bin_width.unwrap_or(0.0) / 2.0;
            if !(a.is_finite() && b.is_finite()) {
                (0.0, 1.0)
            } else if b > a {
                (a - half, b + half)
            } else {
                (a - 1.0, a + 1.0)
            }
        }
    };
    let sx = |x: f64| plot.left() + ((x - xlo) / (xhi - xlo)) as f32 * plot.width();
    let sy = |y: f64| plot.bottom() - ((y - ylo) / (yhi - ylo)) as f32 * plot.height();

    for t in &yt {
        let y = sy(*t);
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, BORDER));
        painter.text(pos2(plot.left() - 6.0, y), Align2::RIGHT_CENTER, fmt_num(*t), small.clone(), MUTED);
    }
    painter.line_segment([plot.left_bottom(), plot.right_bottom()], Stroke::new(1.0, MUTED.gamma_multiply(0.6)));

    match &d.x {
        XAxis::Category(c) => {
            let every = (c.len() as f32 * 70.0 / plot.width().max(1.0)).ceil().max(1.0) as usize;
            for (i, l) in c.iter().enumerate().step_by(every) {
                painter.text(pos2(sx(i as f64), plot.bottom() + 4.0), Align2::CENTER_TOP, one_line(l, 10), small.clone(), MUTED);
            }
        }
        XAxis::Number { time } => {
            let (xt, _, _) = ticks(xlo, xhi, ((plot.width() / 90.0) as usize).max(2));
            for t in xt.iter().filter(|t| **t >= xlo && **t <= xhi) {
                let label = if *time { data::format_date(*t) } else { fmt_num(*t) };
                painter.text(pos2(sx(*t), plot.bottom() + 4.0), Align2::CENTER_TOP, label, small.clone(), MUTED);
            }
        }
    }

    let n = d.series.len().max(1);
    let mut marks: Vec<(Pos2, usize, [f64; 2])> = Vec::new();
    for (si, s) in d.series.iter().enumerate() {
        let color = series_color(si);
        match d.kind {
            ChartKind::Bar => {
                let band = plot.width() / (xhi - xlo) as f32;
                let w = (band * 0.8 / n as f32).max(1.0);
                for p in &s.points {
                    let x0 = sx(p[0]) - band * 0.4 + w * si as f32;
                    let r = Rect::from_two_pos(pos2(x0, sy(0f64.max(ylo))), pos2(x0 + w - 1.0, sy(p[1])));
                    painter.rect_filled(r, CornerRadius::same(2), color);
                    marks.push((pos2(x0 + w / 2.0, sy(p[1])), si, *p));
                }
            }
            ChartKind::Histogram => {
                let half = d.bin_width.unwrap_or(1.0) / 2.0;
                for p in &s.points {
                    let r = Rect::from_two_pos(pos2(sx(p[0] - half) + 0.5, sy(0.0)), pos2(sx(p[0] + half) - 0.5, sy(p[1])));
                    painter.rect_filled(r, CornerRadius::ZERO, color);
                    marks.push((pos2(sx(p[0]), sy(p[1])), si, *p));
                }
            }
            ChartKind::Line | ChartKind::Area => {
                let line: Vec<Pos2> = s.points.iter().map(|p| pos2(sx(p[0]), sy(p[1]))).collect();
                if d.kind == ChartKind::Area {
                    let base = sy(0f64.clamp(ylo, yhi));
                    let mut mesh = egui::Mesh::default();
                    let fill = color.gamma_multiply(0.25);
                    for w in line.windows(2) {
                        if let [a, b] = w {
                            let i = mesh.vertices.len() as u32;
                            mesh.colored_vertex(*a, fill);
                            mesh.colored_vertex(*b, fill);
                            mesh.colored_vertex(pos2(b.x, base), fill);
                            mesh.colored_vertex(pos2(a.x, base), fill);
                            mesh.add_triangle(i, i + 1, i + 2);
                            mesh.add_triangle(i, i + 2, i + 3);
                        }
                    }
                    painter.add(Shape::mesh(mesh));
                }
                painter.add(Shape::line(line.clone(), Stroke::new(2.0, color)));
                for (q, p) in line.iter().zip(&s.points) {
                    marks.push((*q, si, *p));
                }
            }
            ChartKind::Scatter => {
                for p in &s.points {
                    let q = pos2(sx(p[0]), sy(p[1]));
                    painter.circle_filled(q, 2.5, color.gamma_multiply(0.85));
                    marks.push((q, si, *p));
                }
            }
            ChartKind::Pivot => {}
        }
    }

    if let Some(hover) = response.hover_pos()
        && let Some((q, si, p)) = marks.iter().min_by(|a, b| a.0.distance_sq(hover).total_cmp(&b.0.distance_sq(hover)))
        && q.distance(hover) < 40.0
    {
        painter.circle_stroke(*q, 4.0, Stroke::new(1.5, TEXT));
        let x = match &d.x {
            XAxis::Category(c) => c.get(p[0] as usize).cloned().unwrap_or_default(),
            XAxis::Number { time: true } => data::format_date(p[0]),
            XAxis::Number { time: false } => fmt_num(p[0]),
        };
        let name = d.series.get(*si).map(|s| s.name.as_str()).unwrap_or_default();
        let galley = painter.layout_no_wrap(format!("{x} · {name} {}", fmt_num(p[1])), small.clone(), TEXT);
        let at = pos2((q.x + 8.0).min(plot.right() - galley.size().x - 8.0), (q.y - 18.0).max(plot.top()));
        let bg = Rect::from_min_size(at, galley.size() + vec2(8.0, 4.0));
        painter.rect_filled(bg, CornerRadius::same(3), Color32::from_rgb(0x0F, 0x11, 0x15));
        painter.galley(at + vec2(4.0, 2.0), galley, TEXT);
    }
    response
}

/// Draw a pivot table, cells tinted by value.
pub fn pivot(ui: &mut egui::Ui, p: &PivotData) {
    let max = p.cells.iter().flatten().flatten().fold(0f64, |m, v| m.max(v.abs()));
    ui.label(egui::RichText::new(format!("{}({}) by {} × {}", p.agg.name(), p.value_label, p.row_label, p.column_label)).color(MUTED).small());
    egui::ScrollArea::both().max_height(320.0).id_salt(("pivot", p.row_label.as_str(), p.column_label.as_str())).show(ui, |ui| {
        egui::Grid::new(("pivot-grid", p.row_label.as_str(), p.column_label.as_str())).spacing(vec2(12.0, 4.0)).show(ui, |ui| {
            ui.label(egui::RichText::new(&p.row_label).color(MUTED).small());
            for c in &p.column_keys {
                ui.label(egui::RichText::new(one_line(c, 16)).color(MUTED).small().strong());
            }
            ui.end_row();
            for (r, row) in p.row_keys.iter().zip(&p.cells) {
                ui.label(egui::RichText::new(one_line(r, 24)).color(TEXT).small());
                for v in row {
                    match v {
                        Some(v) => {
                            let t = if max > 0.0 { (v.abs() / max) as f32 } else { 0.0 };
                            let text = egui::RichText::new(fmt_num(*v)).monospace().color(TEXT).small();
                            egui::Frame::new().fill(ACCENT.gamma_multiply(0.08 + 0.4 * t)).inner_margin(egui::Margin::symmetric(4, 1)).show(
                                ui,
                                |ui| {
                                    ui.label(text);
                                },
                            );
                        }
                        None => {
                            ui.label(egui::RichText::new("–").color(MUTED).small());
                        }
                    }
                }
                ui.end_row();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_nice_and_cover_the_range() {
        let (t, lo, hi) = ticks(3.0, 97.0, 5);
        assert_eq!((lo, hi), (0.0, 100.0));
        assert_eq!(t.first(), Some(&0.0));
        assert_eq!(t.last(), Some(&100.0));
        assert_eq!(fmt_num(12_500.0), "12k");
        assert_eq!(fmt_num(2.5), "2.50");
        assert_eq!(ticks(f64::INFINITY, f64::NEG_INFINITY, 5).1, 0.0);
    }

    #[test]
    fn the_store_keeps_and_drops_panels() {
        let id = PanelId(987_654);
        let cols = [quarry_core::Column::new("a", "")];
        store::put(id, PanelData::Pivot(PivotData::build(&cols, &[vec![quarry_core::Value::Int(1)]], "a", "a", "a", Agg::Count).unwrap()));
        assert!(store::get(id).is_some());
        store::remove(id);
        assert!(store::get(id).is_none());
    }

    #[test]
    fn charts_draw_without_panicking() {
        let ctx = egui::Context::default();
        let rows: Vec<Vec<quarry_core::Value>> = (0..30).map(|i| vec![quarry_core::Value::Int(i), quarry_core::Value::Int(i * i % 7)]).collect();
        let cols = [quarry_core::Column::new("x", ""), quarry_core::Column::new("y", "")];
        for kind in [ChartKind::Bar, ChartKind::Line, ChartKind::Area, ChartKind::Scatter, ChartKind::Histogram] {
            let d = ChartData::build(kind, &cols, &rows, "x", Some("y"), None).unwrap();
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                plot(ui, &d, vec2(400.0, 200.0));
            });
            out.textures_delta.clear();
        }
    }
}

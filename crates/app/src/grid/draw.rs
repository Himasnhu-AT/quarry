//! Drawing one grid: its bar (rows, sort, columns, copy), the table on ashlar's `VirtualTable`,
//! the cell popover, new rows and the pending-changes bar with its diff.
//!
//! Drawing never touches the document or spawns work: it returns [`Action`]s that
//! [`super::show_tab`] applies with the session and the job queue.

use std::ops::Range;

use ashlar::egui::{self, Align2, Color32, FontId, Key, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use ashlar::ui::automation::{self, Details, Role};
use ashlar::ui::code_view::CodeView;
use ashlar::ui::diff_view::{DiffView, DiffViewState};
use ashlar::ui::virtual_list::{Column as TableColumn, RowHeights, Sort, VirtualTable};
use quarry_core::{PendingEdits, Total, Value};

use super::cells::{self, Shown};
use super::copy::Format;
use super::model::{GridState, Status, WINDOW_ROWS};
use super::source::Order;
use crate::human::HumanTicket;
use crate::theme;

/// Row height (DESIGN.md: 28 px rows).
pub const ROW_H: f32 = theme::ROW;
/// The grid's own bar above the table.
pub const BAR_H: f32 = 28.0;
const MONO: f32 = 12.0;
/// Most new rows listed above the table.
const MAX_NEW_ROWS_SHOWN: usize = 8;

/// What the person asked for this frame.
#[derive(Debug)]
pub enum Action {
    Sort(Option<Order>),
    /// Set shown row `row`'s source column `col` (a pending edit).
    SetCell {
        row: u64,
        col: usize,
        value: Value,
    },
    /// Set column `col` of the `n`th new row.
    SetNewCell {
        n: usize,
        col: usize,
        value: Value,
    },
    DeleteRows(Vec<u64>),
    AddRow,
    Discard,
    ToggleReview,
    /// Commit… was clicked: show the diff and ask to confirm.
    AskCommit,
    /// The person confirmed the commit under the diff (a human-only action: the ticket proves
    /// the click).
    Commit(HumanTicket),
}

/// Per-grid drawing state that is not the result's.
#[derive(Default)]
pub struct DrawState {
    pub review: bool,
    /// The review is open to confirm a commit (Commit… was clicked).
    pub confirm: bool,
    pub diff: DiffViewState,
    /// Horizontal wheel travel not yet turned into whole columns.
    pub wheel_x: f32,
    /// Where the cell clicked last frame was (shown row, source column).
    pub clicked_cell: Option<(u64, usize)>,
    /// Rect of the cell with the popover, for placing it.
    pub detail_anchor: Option<Rect>,
    /// The new-row cell being edited: (n, source column, text).
    pub editing_new: Option<(usize, usize, String)>,
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1,000 rows`, `≥ 10,000 rows`.
pub fn rows_text(total: Total) -> String {
    match total {
        Total::Known(1) => "1 row".into(),
        Total::Known(n) => format!("{} rows", thousands(n)),
        Total::AtLeast(n) => format!("≥ {} rows", thousands(n)),
    }
}

fn paint_text(ui: &egui::Ui, rect: Rect, s: &Shown, color: Color32, muted: Color32) {
    let font = FontId::monospace(MONO);
    let color = if s.muted { muted } else { color };
    let mut job = egui::text::LayoutJob::default();
    job.append(&s.text, 0.0, egui::TextFormat { font_id: font, color, italics: s.italic, ..Default::default() });
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.max_width = f32::INFINITY;
    let galley = ui.painter().layout_job(job);
    let mut right_edge = rect.right();
    if s.more {
        // the expander: the popover has the rest
        let r = Rect::from_center_size(pos2(rect.right() - 7.0, rect.center().y), vec2(12.0, 12.0));
        ui.painter().rect_stroke(r, 3.0, Stroke::new(1.0, muted.gamma_multiply(0.6)), StrokeKind::Inside);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, "…", FontId::proportional(9.0), muted);
        right_edge -= 18.0;
    }
    let x = if s.right { (right_edge - galley.size().x).max(rect.left()) } else { rect.left() };
    let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
    let clip = Rect::from_min_max(rect.min, pos2(right_edge.max(rect.left()), rect.max.y));
    ui.painter().with_clip_rect(clip.intersect(ui.clip_rect())).galley(pos, galley, color);
}

/// Draw `g`. `edits`: the tab's pending edits; `id`: the automation id prefix (`quarry.grid`).
pub fn draw(ui: &mut egui::Ui, id: &str, g: &mut GridState, ds: &mut DrawState, edits: Option<&PendingEdits>) -> (Vec<Action>, Range<u64>) {
    let mut actions = Vec::new();
    let full = ui.available_rect_before_wrap();
    automation::register_fmt(ui, full, format_args!("{id}"), "Results", Role::Group, true, Details::default());
    bar(ui, id, g, &mut actions);
    if let Status::Error { message, position } = &g.status {
        let text = match position {
            Some(p) => format!("ERROR: {message} (at character {p})"),
            None => format!("ERROR: {message}"),
        };
        let frame = egui::Frame::new()
            .fill(theme::ERROR.gamma_multiply(0.14))
            .stroke(Stroke::new(1.0, theme::ERROR.gamma_multiply(0.6)))
            .inner_margin(8)
            .corner_radius(6);
        let r = frame.show(ui, |ui| ui.add(egui::Label::new(egui::RichText::new(text).color(theme::TEXT).monospace()).wrap())).inner;
        automation::register(ui, &r, &format!("{id}.error"), "Query error", Role::Alert);
        if g.columns.is_empty() {
            return (actions, 0..0);
        }
    }
    if g.columns.is_empty() {
        skeleton(ui, id, g);
        return (actions, 0..0);
    }
    if let Some(e) = edits
        && e.inserts().next().is_some()
    {
        new_rows(ui, id, g, ds, e, &mut actions);
    }
    // the pending bar and the review panel sit under the table
    let pending = edits.filter(|e| !e.is_empty());
    let below = match pending {
        Some(_) if ds.review => BAR_H + (ui.available_height() * 0.45).clamp(140.0, 420.0),
        Some(_) => BAR_H,
        None => 0.0,
    };
    let visible = table(ui, id, g, ds, edits, ui.available_height() - below, &mut actions);
    if let Some(e) = pending {
        pending_bar(ui, id, g, ds, e, &mut actions);
    }
    detail_popover(ui, id, g, ds);
    (actions, visible)
}

fn bar(ui: &mut egui::Ui, id: &str, g: &mut GridState, actions: &mut Vec<Action>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
    let mut b = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(4.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    let loading = g.is_loading();
    let mut status = match &g.status {
        Status::Running => "Running…".to_string(),
        Status::Error { .. } => "Failed".to_string(),
        Status::Ready => rows_text(g.total),
    };
    if g.status == Status::Ready && loading {
        status.push_str(" · loading");
    }
    if let Some(o) = g.sort
        && let Some(c) = g.columns.get(o.column)
    {
        status.push_str(&format!(" · sorted by {} {}", c.name, if o.descending { "desc" } else { "asc" }));
    }
    let r = b.label(egui::RichText::new(&status).color(theme::MUTED).size(12.0));
    automation::register_with(&b, &r, &format!("{id}.rows"), &status, Role::Status, Details { value: Some(&status), selected: None });
    let (before, after) = g.window_more();
    if before || after {
        let end = g.window.saturating_add(g.window_rows());
        b.label(
            egui::RichText::new(format!("· showing {}–{}", thousands(g.window.saturating_add(1)), thousands(end))).color(theme::MUTED).size(12.0),
        );
        let prev = b.add_enabled(before, egui::Button::new("◀").small());
        automation::register(&b, &prev, &format!("{id}.window.prev"), "Previous rows", Role::Button);
        if prev.clicked() {
            g.set_window(g.window.saturating_sub(WINDOW_ROWS));
            g.scroll_to = Some(WINDOW_ROWS - 1);
        }
        let next = b.add_enabled(after, egui::Button::new("▶").small());
        automation::register(&b, &next, &format!("{id}.window.next"), "Next rows", Role::Button);
        if next.clicked() {
            g.set_window(g.window.saturating_add(WINDOW_ROWS));
            g.scroll_to = Some(0);
        }
    }
    if let Some(n) = &g.read_only_note {
        let r = b.label(egui::RichText::new(format!("· {n}")).color(theme::MUTED).size(12.0));
        automation::register(&b, &r, &format!("{id}.read_only"), n, Role::Label);
    }
    if let Some(n) = &g.notice {
        b.label(egui::RichText::new(format!("· {n}")).color(theme::INFO).size(12.0));
    }
    if g.columns.is_empty() {
        return;
    }
    b.with_layout(egui::Layout::right_to_left(egui::Align::Center), |b| {
        let copy = b.menu_button("Copy", |m| {
            for (f, label) in [(Format::Tsv, "TSV"), (Format::Csv, "CSV"), (Format::Json, "JSON"), (Format::Insert, "SQL INSERT")] {
                let r = m.button(label);
                automation::register(m, &r, &format!("{id}.copy.{}", f.name()), label, Role::MenuItem);
                if r.clicked() {
                    g.copy(f);
                    m.close();
                }
            }
        });
        automation::register(b, &copy.response, &format!("{id}.copy"), "Copy", Role::Button);
        let hidden = g.layout.hidden_count();
        let label = if hidden > 0 { format!("Columns ({hidden} hidden)") } else { "Columns".to_string() };
        let cols = b.menu_button(label, |m| columns_menu(m, id, g));
        automation::register(b, &cols.response, &format!("{id}.columns"), "Columns", Role::Button);
        if g.target.is_some() {
            let add = b.button("+ Row");
            automation::register(b, &add, &format!("{id}.add_row"), "Add row", Role::Button);
            if add.clicked() {
                actions.push(Action::AddRow);
            }
            let del = b.add_enabled(!g.selection.is_empty(), egui::Button::new("Delete"));
            automation::register(b, &del, &format!("{id}.delete_rows"), "Delete rows", Role::Button);
            if del.clicked() {
                actions.push(Action::DeleteRows(g.selection.iter().map(|r| r as u64).collect()));
            }
        }
    });
}

fn columns_menu(m: &mut egui::Ui, id: &str, g: &mut GridState) {
    let mut frozen = g.layout.frozen;
    let r = m.checkbox(&mut frozen, "Freeze first column");
    automation::register(m, &r, &format!("{id}.columns.freeze"), "Freeze first column", Role::Toggle);
    g.layout.frozen = frozen;
    m.separator();
    let hidden: Vec<usize> = (0..g.columns.len()).filter(|c| g.layout.is_hidden(*c)).collect();
    for c in g.layout.shown().into_iter().chain(hidden) {
        let Some(name) = g.columns.get(c).map(|c| c.name.clone()) else { continue };
        m.horizontal(|m| {
            let mut on = !g.layout.is_hidden(c);
            let r = m.checkbox(&mut on, egui::RichText::new(&name).monospace());
            automation::register(m, &r, &format!("{id}.columns.{c}"), &name, Role::Toggle);
            if r.changed() {
                g.layout.set_hidden(c, !on);
                g.revision = g.revision.wrapping_add(1);
            }
            if m.small_button("◀").on_hover_text("Move left").clicked() {
                g.layout.shift(c, -1);
            }
            if m.small_button("▶").on_hover_text("Move right").clicked() {
                g.layout.shift(c, 1);
            }
        });
    }
    if g.layout.hidden_count() > 0 && m.button("Show all").clicked() {
        g.layout.show_all();
    }
}

/// Before the first rows: grey bars where rows will be (no animation: idle stays idle).
pub fn skeleton(ui: &mut egui::Ui, id: &str, g: &GridState) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ui.available_height().max(ROW_H)), Sense::hover());
    let p = ui.painter();
    p.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width(), ROW_H)), 0.0, theme::PANEL);
    let rows = ((rect.height() / ROW_H) as usize).min(12);
    for i in 1..rows {
        let y = rect.top() + i as f32 * ROW_H;
        for (k, w) in [(0, 40.0), (1, 120.0), (2, 90.0), (3, 160.0), (4, 70.0)] {
            let x = rect.left() + 12.0 + [0.0, 64.0, 210.0, 330.0, 520.0][k];
            let w = w * (0.6 + 0.4 * (((i * 7 + k * 3) % 5) as f32 / 4.0));
            p.rect_filled(Rect::from_min_size(pos2(x, y + 10.0), vec2(w, 8.0)), 4.0, theme::BORDER);
        }
    }
    let label = if matches!(g.status, Status::Running) { "Running… (Cancel ⌘.)" } else { "No rows" };
    p.text(pos2(rect.left() + 12.0, rect.top() + ROW_H / 2.0), Align2::LEFT_CENTER, label, FontId::proportional(12.0), theme::MUTED);
    automation::register(ui, &resp, &format!("{id}.loading"), label, Role::Progress);
}

/// A rect in AccessKit's physical pixels (the cell's bounds, so readers know it is on screen).
fn bounds(ui: &egui::Ui, r: Rect) -> egui::accesskit::Rect {
    let ppp = f64::from(ui.ctx().pixels_per_point());
    egui::accesskit::Rect { x0: f64::from(r.min.x) * ppp, y0: f64::from(r.min.y) * ppp, x1: f64::from(r.max.x) * ppp, y1: f64::from(r.max.y) * ppp }
}

fn row_number_width(last: u64) -> f32 {
    (thousands(last).len() as f32 * 7.5 + 20.0).max(44.0)
}

fn table(
    ui: &mut egui::Ui,
    id: &str,
    g: &mut GridState,
    ds: &mut DrawState,
    edits: Option<&PendingEdits>,
    height: f32,
    actions: &mut Vec<Action>,
) -> Range<u64> {
    // before the first page lands, placeholder rows under the header
    let rows = if g.window_rows() == 0 && g.is_loading() { 24 } else { g.window_rows() };
    g.layout.fit(ui.available_width() - row_number_width(g.window.saturating_add(rows)));
    let num_w = row_number_width(g.window.saturating_add(rows));
    let width = ui.available_width();
    let window = g.layout.window(width - num_w);
    // the column set is rebuilt each frame: row numbers, then the columns that fit
    let titles: Vec<&str> = window.iter().map(|&c| g.columns.get(c).map_or("", |c| c.name.as_str())).collect();
    let mut cols = Vec::with_capacity(window.len() + 1);
    cols.push(TableColumn { title: "#", width: num_w, min_width: num_w, resizable: false });
    for (k, &c) in window.iter().enumerate() {
        cols.push(TableColumn::new(titles.get(k).copied().unwrap_or(""), g.layout.width(c)));
    }
    // VirtualTable remembers widths by position; ours are by column, so hand them in each frame
    let table_id = format!("{id}.table");
    let base = ui.make_persistent_id(table_id.as_str());
    let widths: Vec<f32> = cols.iter().map(|c| c.width).collect();
    ui.data_mut(|d| d.insert_temp(base.with("widths"), widths));
    let sort = g.sort.and_then(|o| window.iter().position(|&c| c == o.column).map(|k| Sort { column: k + 1, ascending: !o.descending }));
    let span = g.cells.range();
    let shown = g.layout.shown();
    let pos_of = |c: usize| shown.iter().position(|&s| s == c);
    let font_h = FontId::proportional(10.5);
    let editing = g.editing.clone();
    let mut edit_done: Option<EditOutcome> = None;
    let mut clicked_cell = None;
    let primary_clicked = ui.input(|i| i.pointer.primary_clicked());
    let mut selection = std::mem::take(&mut g.selection);
    // the cells need the selection while the table holds it
    let selected_rows = if span.is_some() { selection.clone() } else { Default::default() };
    let scroll_to = g.scroll_to.take().and_then(|r| usize::try_from(r).ok());
    let header_top = ui.cursor().min;
    let gv: &GridState = g;
    let resp = VirtualTable::new(&table_id, &cols, usize::try_from(rows).unwrap_or(usize::MAX), RowHeights::Uniform(ROW_H))
        .label("Query results")
        .sort(sort)
        .max_height(height.max(ROW_H * 3.0))
        .scroll_to(scroll_to)
        .selection(&mut selection)
        .show(ui, |ui, row, c| {
            let shown_row = gv.window.saturating_add(row as u64);
            let rect = ui.max_rect();
            if c == 0 {
                let n = thousands(shown_row.saturating_add(1));
                ui.painter().text(pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, &n, FontId::monospace(11.0), theme::MUTED);
                // (the bounds are read before: the builder holds the context's lock)
                let b = bounds(ui, rect);
                ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                    node.set_label("row");
                    node.set_value(n.as_str());
                    node.set_bounds(b);
                });
                if primary_clicked && ui.rect_contains_pointer(rect) {
                    clicked_cell = Some((shown_row, usize::MAX));
                }
                return;
            }
            let Some(&src) = window.get(c - 1) else { return };
            if primary_clicked && ui.rect_contains_pointer(rect.expand2(vec2(8.0, 0.0))) {
                clicked_cell = Some((shown_row, src));
            }
            let Some(row_vals) = gv.row(shown_row) else {
                // not loaded yet: a placeholder bar
                let w = (rect.width() * 0.5).min(80.0);
                ui.painter().rect_filled(Rect::from_min_size(pos2(rect.left(), rect.center().y - 4.0), vec2(w, 8.0)), 4.0, theme::BORDER);
                return;
            };
            let value = row_vals.get(src).unwrap_or(&Value::Null);
            // pending edits: the new value on a warm tint; deleted rows struck through in red
            let key: Option<Vec<Value>> = edits.and_then(|_| gv.key_of(shown_row));
            let col_name = gv.columns.get(src).map_or("", |c| c.name.as_str());
            let mut shown_val = value;
            let mut tint = None;
            if let (Some(e), Some(k)) = (edits, key.as_ref()) {
                if e.deleted(k) {
                    tint = Some(theme::ERROR);
                } else if let Some(v) = e.updated(k, col_name) {
                    shown_val = v;
                    tint = Some(theme::WARN);
                }
            }
            let full = rect.expand2(vec2(8.0, 0.0));
            if let Some(t) = tint {
                ui.painter().rect_filled(full, 0.0, t.gamma_multiply(0.16));
            }
            if let (Some(r), Some(p)) = (&span, pos_of(src))
                && r.contains(&p)
                && selected_rows.contains(row)
            {
                ui.painter().rect_filled(full, 0.0, theme::ACCENT.gamma_multiply(0.35));
            }
            if let Some((er, ec, text)) = &editing
                && *er == shown_row
                && *ec == src
            {
                let mut t = text.clone();
                let r = ui.add_sized(rect.size(), egui::TextEdit::singleline(&mut t).font(FontId::monospace(MONO)).margin(vec2(2.0, 2.0)));
                automation::register(ui, &r, &format!("{id}.editor"), "Edit cell", Role::Field);
                r.request_focus();
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    edit_done = Some(EditOutcome::Cancel);
                } else if r.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    edit_done = Some(EditOutcome::Done(t));
                } else if t != *text {
                    edit_done = Some(EditOutcome::Typing(t));
                }
                return;
            }
            let s = cells::shown(shown_val);
            paint_text(ui, rect, &s, theme::TEXT, theme::MUTED);
            if tint == Some(theme::ERROR) {
                ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, theme::ERROR));
            }
            let b = bounds(ui, rect);
            ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                node.set_label(col_name);
                node.set_bounds(b);
                node.set_value(s.text.as_str());
            });
        });
    g.selection = selection;
    // the type under each name, muted, at the right of the header cell
    let mut x = header_top.x;
    let header_h = ui.style().spacing.interact_size.y.max(24.0);
    let tokens_h = ashlar::ui::tokens::current(ui.ctx()).control_height;
    let hh = if tokens_h > 0.0 { tokens_h } else { header_h };
    for (k, w) in resp.widths.iter().enumerate() {
        if k > 0
            && let Some(col) = window.get(k - 1).and_then(|&c| g.columns.get(c))
        {
            let cell = Rect::from_min_size(pos2(x, header_top.y), vec2(*w, hh));
            let p = ui.painter().with_clip_rect(cell.shrink(2.0).intersect(ui.clip_rect()));
            let title_w = (col.name.chars().count() as f32) * 6.6 + 30.0;
            if cell.width() > title_w + 24.0 {
                p.text(
                    pos2(cell.right() - 8.0, cell.center().y),
                    Align2::RIGHT_CENTER,
                    &col.type_name,
                    font_h.clone(),
                    theme::MUTED.gamma_multiply(0.8),
                );
            }
        }
        x += w;
    }
    // widths the person dragged go back to the columns
    for (k, w) in resp.widths.iter().enumerate().skip(1) {
        if let Some(&c) = window.get(k - 1)
            && (g.layout.width(c) - w).abs() > 0.01
        {
            g.layout.set_width(c, *w);
        }
    }
    let list = &resp.list;
    if let Some(k) = resp.header_clicked
        && k > 0
        && let Some(&c) = window.get(k - 1)
    {
        actions.push(Action::Sort(g.next_sort(c)));
    }
    // which cell was clicked: the list says which row, the cell said which column
    if let Some(r) = list.clicked
        && let Some((cr, cc)) = clicked_cell.or(ds.clicked_cell)
        && cr == g.window.saturating_add(r as u64)
    {
        let shift = ui.input(|i| i.modifiers.shift);
        g.cells.span = if cc == usize::MAX {
            None
        } else {
            let p = pos_of(cc).unwrap_or(0);
            match g.cells.span {
                Some((a, _)) if shift => Some((a, p)),
                _ => Some((p, p)),
            }
        };
        g.editing = None;
    }
    ds.clicked_cell = clicked_cell;
    if let Some(r) = list.double_clicked {
        let row = g.window.saturating_add(r as u64);
        if let Some((_, c)) = clicked_cell.filter(|(_, c)| *c != usize::MAX).or(ds.clicked_cell) {
            g.detail = Some((row, c));
        }
    }
    // keyboard on the focused grid
    let focused = list.response.has_focus();
    if focused && g.editing.is_none() {
        let (left, right, copy, f2, enter, esc, del) = ui.input_mut(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.consume_key(egui::Modifiers::COMMAND, Key::C),
                i.key_pressed(Key::F2),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::Delete),
            )
        });
        let shift = ui.input(|i| i.modifiers.shift);
        if left || right {
            let n = shown.len();
            let (a, cur) = g.cells.span.unwrap_or((0, 0));
            let next = if left { cur.saturating_sub(1) } else { (cur + 1).min(n.saturating_sub(1)) };
            g.cells.span = Some(if shift { (a, next) } else { (next, next) });
            if let Some(&c) = shown.get(next) {
                g.layout.reveal(c, width - num_w);
            }
        }
        if copy {
            g.copy(Format::Tsv);
        }
        let cursor = g.selection.cursor().map(|r| g.window.saturating_add(r as u64));
        let cur_col = g.cells.span.and_then(|(_, b)| shown.get(b).copied());
        if f2 && let (Some(r), Some(c)) = (cursor, cur_col) {
            start_edit(g, r, c);
        }
        if enter && let (Some(r), Some(c)) = (cursor, cur_col) {
            g.detail = Some((r, c));
        }
        if esc {
            g.detail = None;
        }
        if del && g.target.is_some() && !g.selection.is_empty() {
            actions.push(Action::DeleteRows(g.selection.iter().map(|r| r as u64).collect()));
        }
    }
    match edit_done {
        Some(EditOutcome::Typing(t)) => {
            if let Some(e) = &mut g.editing {
                e.2 = t;
            }
        }
        Some(EditOutcome::Done(t)) => {
            if let Some((r, c, _)) = g.editing.take() {
                let ty = g.columns.get(c).map_or("", |c| c.type_name.as_str()).to_string();
                let old = g.cell(r, c).cloned().unwrap_or(Value::Null);
                actions.push(Action::SetCell { row: r, col: c, value: super::edits::parse_input(&t, &ty, &old) });
            }
        }
        Some(EditOutcome::Cancel) => g.editing = None,
        None => {}
    }
    // sideways: horizontal wheel or shift + wheel moves whole columns
    if list.response.contains_pointer() {
        let dx = ui.input(|i| if i.modifiers.shift && i.smooth_scroll_delta.x == 0.0 { i.smooth_scroll_delta.y } else { i.smooth_scroll_delta.x });
        ds.wheel_x += dx;
        let step = 60.0;
        if ds.wheel_x.abs() >= step {
            let cols = (ds.wheel_x / step) as isize;
            g.layout.scroll_by(-cols);
            ds.wheel_x -= cols as f32 * step;
        }
    } else {
        ds.wheel_x = 0.0;
    }
    let (more_left, more_right) = g.layout.overflow(width - num_w);
    let body = list.response.rect;
    if more_left {
        ui.painter().vline(body.left() + num_w, body.y_range(), Stroke::new(2.0, theme::ACCENT.gamma_multiply(0.5)));
    }
    if more_right {
        ui.painter().vline(body.right() - 1.0, body.y_range(), Stroke::new(2.0, theme::ACCENT.gamma_multiply(0.5)));
    }
    if let Some((r, _)) = g.detail {
        let local = r.saturating_sub(g.window);
        if list.visible.contains(&usize::try_from(local).unwrap_or(usize::MAX)) {
            let top = body.top() + hh + local.saturating_sub(list.visible.start as u64) as f32 * ROW_H;
            ds.detail_anchor = Some(Rect::from_min_size(pos2(body.left() + num_w, top), vec2(body.width() - num_w, ROW_H)));
        }
    }
    let v = list.visible.clone();
    g.window.saturating_add(v.start as u64)..g.window.saturating_add(v.end as u64)
}

/// Open the inline editor on shown row `row`, source column `col` (editable grids only).
pub fn start_edit(g: &mut GridState, row: u64, col: usize) {
    if g.target.is_none() || g.key_of(row).is_none() {
        g.notice = Some("This grid cannot be edited (open the table with a primary key)".into());
        return;
    }
    match g.cell(row, col).and_then(super::edits::edit_text) {
        Some(text) => g.editing = Some((row, col, text)),
        None => g.notice = Some("Only part of this value is loaded; it cannot be edited here".into()),
    }
}

fn new_rows(ui: &mut egui::Ui, id: &str, g: &GridState, ds: &mut DrawState, e: &PendingEdits, actions: &mut Vec<Action>) {
    let inserts: Vec<&Vec<(String, Value)>> = e.inserts().collect();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("New rows ({})", inserts.len())).color(theme::OK).size(12.0));
    });
    for (n, values) in inserts.iter().enumerate().take(MAX_NEW_ROWS_SHOWN) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, theme::OK.gamma_multiply(0.10));
        // aligned with the table's columns below
        let num_w = row_number_width(g.window.saturating_add(g.window_rows()));
        ui.painter().text(pos2(rect.left() + num_w - 8.0, rect.center().y), Align2::RIGHT_CENTER, "new", FontId::monospace(11.0), theme::OK);
        let mut x = rect.left() + num_w + 8.0;
        for c in g.layout.window(rect.width() - num_w) {
            let Some(col) = g.columns.get(c) else { continue };
            let w = g.layout.width(c);
            let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(w - 8.0, ROW_H));
            x += w;
            if cell.left() > rect.right() {
                break;
            }
            let v = values.iter().find(|(k, _)| *k == col.name).map(|(_, v)| v);
            let editing = ds.editing_new.as_ref().filter(|(en, ec, _)| *en == n && *ec == c).map(|(_, _, t)| t.clone());
            let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(cell).layout(egui::Layout::left_to_right(egui::Align::Center)));
            if let Some(mut t) = editing {
                let r = cui.add_sized(cell.size(), egui::TextEdit::singleline(&mut t).font(FontId::monospace(MONO)));
                r.request_focus();
                if cui.input(|i| i.key_pressed(Key::Escape)) {
                    ds.editing_new = None;
                } else if r.lost_focus() || cui.input(|i| i.key_pressed(Key::Enter)) {
                    ds.editing_new = None;
                    let old = v.cloned().unwrap_or(Value::Null);
                    actions.push(Action::SetNewCell { n, col: c, value: super::edits::parse_input(&t, &col.type_name, &old) });
                } else {
                    ds.editing_new = Some((n, c, t));
                }
                continue;
            }
            let s = match v {
                Some(v) => cells::shown(v),
                None => Shown { text: "DEFAULT".into(), right: false, muted: true, italic: true, more: false },
            };
            let r = cui.interact(cell, cui.id().with(("new", n, c)), Sense::click());
            paint_text(&cui, cell, &s, theme::TEXT, theme::MUTED);
            automation::register(&cui, &r, &format!("{id}.new.{n}.{}", col.name), &s.text, Role::Cell);
            if r.double_clicked() {
                let text = v.and_then(super::edits::edit_text).unwrap_or_default();
                ds.editing_new = Some((n, c, text));
            }
        }
    }
    if inserts.len() > MAX_NEW_ROWS_SHOWN {
        ui.label(egui::RichText::new(format!("… and {} more (see Review)", inserts.len() - MAX_NEW_ROWS_SHOWN)).color(theme::MUTED).size(12.0));
    }
}

fn pending_bar(ui: &mut egui::Ui, id: &str, g: &GridState, ds: &mut DrawState, e: &PendingEdits, actions: &mut Vec<Action>) {
    let (u, i, d) = e.counts();
    let mut parts = Vec::new();
    if u > 0 {
        parts.push(format!("{u} {} changed", if u == 1 { "cell" } else { "cells" }));
    }
    if i > 0 {
        parts.push(format!("{i} new {}", if i == 1 { "row" } else { "rows" }));
    }
    if d > 0 {
        parts.push(format!("{d} deleted"));
    }
    let summary = format!("Pending: {}", parts.join(" · "));
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::WARN.gamma_multiply(0.12));
    ui.painter().hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, theme::WARN.gamma_multiply(0.6)));
    let mut b = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    let r = b.label(egui::RichText::new(&summary).color(theme::WARN).size(12.0));
    automation::register_with(&b, &r, &format!("{id}.pending"), &summary, Role::Status, Details { value: Some(&summary), selected: None });
    b.with_layout(egui::Layout::right_to_left(egui::Align::Center), |b| {
        let commit = b.button(egui::RichText::new("Commit…").color(theme::TEXT));
        automation::register(b, &commit, &format!("{id}.commit"), "Commit pending changes", Role::Button);
        // the diff first: committing is the confirm button under it
        if commit.clicked() {
            actions.push(Action::AskCommit);
        }
        let review = b.selectable_label(ds.review, "Review").on_hover_text("Review pending changes (Cmd+Alt+S)");
        automation::register_with(
            b,
            &review,
            &format!("{id}.review"),
            "Review pending changes",
            Role::Toggle,
            Details { value: None, selected: Some(ds.review) },
        );
        if review.clicked() {
            actions.push(Action::ToggleReview);
        }
        let discard = b.button("Discard");
        automation::register(b, &discard, &format!("{id}.discard"), "Discard pending changes", Role::Button);
        if discard.clicked() {
            actions.push(Action::Discard);
        }
    });
    if ds.review {
        let diff = [super::edits::diff(e)];
        let (sql, n) = match super::edits::statements(e, g.dialect) {
            Ok(s) => (s.join("\n"), Some(s.len())),
            Err(m) => (format!("-- cannot commit: {m}"), None),
        };
        if ds.confirm {
            confirm_strip(ui, id, g, ds, n, actions);
        }
        let h = ui.available_height();
        let rev = g.revision ^ (e.changes.len() as u64).rotate_left(32);
        ui.horizontal_top(|ui| {
            let w = ui.available_width();
            ui.allocate_ui(vec2(w * 0.55, h), |ui| {
                DiffView::new(&format!("{id}.diff"), &mut ds.diff, &diff, rev).max_height(h).show(ui);
            });
            ui.allocate_ui(vec2(ui.available_width(), h), |ui| {
                let r = CodeView::new(&format!("{id}.sql"), &sql).lang(Some("sql")).revision(rev).max_height(h).show(ui);
                let _ = r;
            });
        });
    }
}

/// Under the pending bar while confirming: what will run, Commit (needs the person's click) and
/// Cancel.
fn confirm_strip(ui: &mut egui::Ui, id: &str, g: &GridState, ds: &mut DrawState, n: Option<usize>, actions: &mut Vec<Action>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H + 4.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::RAISED);
    let mut b = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    let table = g.target.as_ref().map(|t| t.table.to_string()).unwrap_or_default();
    let text = match n {
        Some(n) => format!("Run these {n} statement(s) on {table} in one transaction? Logged in the audit log."),
        None => "These changes cannot be committed (see the SQL).".to_string(),
    };
    let r = b.label(egui::RichText::new(&text).color(theme::TEXT).size(12.0));
    automation::register(&b, &r, &format!("{id}.confirm.text"), &text, Role::Label);
    b.with_layout(egui::Layout::right_to_left(egui::Align::Center), |b| {
        let ok = b.add_enabled(n.is_some(), egui::Button::new(egui::RichText::new("Commit").color(theme::ON_ACCENT)).fill(theme::ACCENT));
        automation::register(b, &ok, &format!("{id}.confirm.commit"), "Commit", Role::Button);
        let ctx = b.ctx().clone();
        if let Some(t) = HumanTicket::from_click(&ctx, &ok) {
            actions.push(Action::Commit(t));
        }
        let cancel = b.button("Cancel");
        automation::register(b, &cancel, &format!("{id}.confirm.cancel"), "Cancel", Role::Button);
        if cancel.clicked() {
            ds.confirm = false;
        }
    });
}

fn detail_popover(ui: &mut egui::Ui, id: &str, g: &mut GridState, ds: &mut DrawState) {
    let Some((row, col)) = g.detail else { return };
    // not loaded (yet): wait for it
    let Some(value) = g.cell(row, col).cloned() else { return };
    let name = g.columns.get(col).map(|c| format!("{} · {}", c.name, c.type_name)).unwrap_or_default();
    let text = cells::detail(&value);
    let anchor = ds.detail_anchor.unwrap_or_else(|| ui.max_rect());
    let pos = pos2(anchor.left() + 24.0, anchor.bottom() + 4.0);
    let mut open = true;
    egui::Area::new(egui::Id::new((id, "detail"))).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).fill(theme::RAISED).stroke(Stroke::new(1.0, theme::BORDER)).corner_radius(6).inner_margin(10).show(ui, |ui| {
            ui.set_max_width(520.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&name).monospace().color(theme::MUTED).size(11.0));
                ui.label(egui::RichText::new(format!("row {}", thousands(row.saturating_add(1)))).color(theme::MUTED).size(11.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let c = ui.small_button("Close");
                    automation::register(ui, &c, &format!("{id}.detail.close"), "Close", Role::Button);
                    if c.clicked() {
                        open = false;
                    }
                    let cp = ui.small_button("Copy");
                    automation::register(ui, &cp, &format!("{id}.detail.copy"), "Copy value", Role::Button);
                    if cp.clicked() {
                        g.clipboard = Some(cells::plain(&value));
                        g.notice = Some("Copied the value".into());
                    }
                });
            });
            let r = egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(&text).monospace().size(12.0).color(theme::TEXT)).wrap().selectable(true))
            });
            automation::register_with(ui, &r.inner, &format!("{id}.detail"), &name, Role::Dialog, Details { value: Some(&text), selected: None });
        });
    });
    if !open || ui.input(|i| i.key_pressed(Key::Escape)) {
        g.detail = None;
    }
}

enum EditOutcome {
    Typing(String),
    Done(String),
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_with_separators() {
        assert_eq!(thousands(10_000_000), "10,000,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(rows_text(Total::AtLeast(1000)), "≥ 1,000 rows");
        assert_eq!(rows_text(Total::Known(1)), "1 row");
    }
}

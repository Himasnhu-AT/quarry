//! Dialogs (`shell` stream): the connection dialog, query history and the open-table picker.

pub mod connection;

use ashlar::egui::{self, vec2};
use ashlar::ui::automation::{self, Role};
use ashlar::ui::dialog::{Dialog, DialogButton, DialogOutcome, InitialFocus};
use ashlar::ui::search::SearchField;
use ashlar::ui::tokens;
use ashlar::ui::virtual_list::{RowHeights, VirtualList};
use quarry_core::{ConnectionId, SchemaTree, TableRef};

use crate::editor::history::{self, Entry};
use crate::sidebar::tree::thousands;
use crate::theme;

/// The open dialog, if any.
pub enum Open {
    Connection(Box<connection::Draft>),
    History(HistoryDialog),
    TablePicker(Picker),
}

/// Query history of one connection, searchable.
pub struct HistoryDialog {
    pub connection: Option<ConnectionId>,
    pub entries: Vec<Entry>,
    pub search: String,
    pub selected: Option<usize>,
}

/// What a history pick does.
pub enum HistoryOutcome {
    Close,
    /// Open the SQL in a new editor tab (it is not run).
    Open(Option<ConnectionId>, String),
}

pub fn show_history(ctx: &egui::Context, h: &mut HistoryDialog, now_ms: u64) -> Option<HistoryOutcome> {
    let buttons = [DialogButton::cancel("Close"), DialogButton::new("Open in Editor").default().enabled(h.selected.is_some())];
    let mut pick = None;
    let title = match &h.connection {
        Some(c) => format!("Query history · {c}"),
        None => "Query history".into(),
    };
    let out = Dialog::new("quarry.dialog.history", &title, &buttons).width(620.0).initial_focus(InitialFocus::Auto).show(ctx, |ui| {
        let t = tokens::current(ui.ctx());
        let s = SearchField::new("quarry.history.search", &mut h.search).label("Search history").hint("Search history").show(ui);
        if s.changed {
            h.selected = None;
        }
        ui.add_space(8.0);
        let hits = history::search(&h.entries, &h.search);
        if hits.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                let text = if h.entries.is_empty() { "No queries yet on this connection" } else { "No query matches" };
                let r = ui.label(egui::RichText::new(text).color(t.text.secondary));
                automation::register(ui, &r, "quarry.history.empty", text, Role::Label);
            });
            ui.add_space(24.0);
            return;
        }
        let label = |i: usize| hits.get(i).map_or("", |e| e.sql.lines().next().unwrap_or(""));
        let out = VirtualList::new("quarry.history.list", hits.len(), RowHeights::Uniform(46.0))
            .label("History")
            .selected(h.selected)
            .max_height(360.0)
            .row_label(&label)
            .show(ui, |ui, row| {
                let Some(e) = hits.get(row.index) else { return };
                let first = e.sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                let p = ui.painter();
                let mut job = egui::text::LayoutJob::simple_singleline(first.to_string(), egui::FontId::monospace(theme::MONO_SIZE), t.text.primary);
                job.wrap = egui::text::TextWrapping::truncate_at_width(row.rect.width() - 24.0);
                let g = p.layout_job(job);
                p.galley(row.rect.left_top() + vec2(10.0, 6.0), g, t.text.primary);
                let meta = match (&e.error, e.rows) {
                    (Some(err), _) => format!("failed · {}", err.chars().take(80).collect::<String>()),
                    (None, Some(n)) => format!("{} rows · {} ms · {}", thousands(n), e.ms, ago(now_ms, e.at)),
                    (None, None) => format!("{} ms · {}", e.ms, ago(now_ms, e.at)),
                };
                let color = if e.error.is_some() { theme::ERROR } else { t.text.secondary };
                let g = p.layout_no_wrap(meta, egui::FontId::proportional(11.5), color);
                p.galley(row.rect.left_top() + vec2(10.0, 26.0), g, color);
                automation::register_fmt(ui, row.rect, format_args!("quarry.history.row.{}", row.index), first, Role::Row, true, Default::default());
            });
        if let Some(i) = out.select {
            h.selected = Some(i);
        }
        if let Some(i) = out.activated.or(out.double_clicked) {
            pick = hits.get(i).map(|e| e.sql.clone());
        }
    });
    if let Some(sql) = pick {
        return Some(HistoryOutcome::Open(h.connection.clone(), sql));
    }
    match out {
        Some(DialogOutcome::Button(1)) => {
            let hits = history::search(&h.entries, &h.search);
            h.selected.and_then(|i| hits.get(i)).map(|e| HistoryOutcome::Open(h.connection.clone(), e.sql.clone())).or(Some(HistoryOutcome::Close))
        }
        Some(_) => Some(HistoryOutcome::Close),
        None => None,
    }
}

/// "5 s ago", "3 min ago", "2 h ago", "4 d ago".
pub fn ago(now_ms: u64, at_ms: u64) -> String {
    let s = now_ms.saturating_sub(at_ms) / 1000;
    match s {
        0..60 => format!("{s} s ago"),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// ⌘P: pick a table of a connection.
pub struct Picker {
    pub connection: Option<ConnectionId>,
    pub tables: Vec<TableRef>,
    pub search: String,
    pub selected: Option<usize>,
}

impl Picker {
    pub fn new(connection: Option<ConnectionId>, schema: Option<&SchemaTree>) -> Self {
        let mut tables = Vec::new();
        if let Some(s) = schema {
            let single = s.namespaces.len() == 1;
            for ns in &s.namespaces {
                for t in &ns.tables {
                    tables.push(TableRef::new((!single).then_some(ns.name.as_str()), &t.name));
                    if tables.len() >= 100_000 {
                        break;
                    }
                }
            }
        }
        Self { connection, tables, search: String::new(), selected: Some(0) }
    }
}

pub enum PickerOutcome {
    Close,
    Open(ConnectionId, TableRef),
}

pub fn show_picker(ctx: &egui::Context, p: &mut Picker) -> Option<PickerOutcome> {
    let buttons = [DialogButton::cancel("Cancel"), DialogButton::new("Open").default().enabled(p.connection.is_some())];
    let q = p.search.to_lowercase();
    let hits: Vec<TableRef> = p.tables.iter().filter(|t| q.is_empty() || t.to_string().to_lowercase().contains(&q)).take(5_000).cloned().collect();
    let mut pick = None;
    let out = Dialog::new("quarry.dialog.open_table", "Open table", &buttons).width(480.0).show(ctx, |ui| {
        let t = tokens::current(ui.ctx());
        let s = SearchField::new("quarry.open_table.search", &mut p.search).label("Table name").hint("Table name").show(ui);
        if s.changed {
            p.selected = Some(0);
        }
        ui.add_space(8.0);
        if p.connection.is_none() || p.tables.is_empty() {
            let text = if p.connection.is_none() { "Connect to a database first" } else { "No tables" };
            let r = ui.label(egui::RichText::new(text).color(t.text.secondary));
            automation::register(ui, &r, "quarry.open_table.empty", text, Role::Label);
            return;
        }
        let label_of: Vec<String> = hits.iter().map(ToString::to_string).collect();
        let label = |i: usize| label_of.get(i).map_or("", String::as_str);
        let out = VirtualList::new("quarry.open_table.list", hits.len(), RowHeights::Uniform(theme::ROW))
            .label("Tables")
            .selected(p.selected)
            .max_height(320.0)
            .row_label(&label)
            .show(ui, |ui, row| {
                let name = label(row.index);
                let color = if row.selected { t.selection_text } else { t.text.primary };
                let g = ui.painter().layout_no_wrap(name.to_string(), egui::FontId::proportional(theme::UI_SIZE), color);
                ui.painter().galley(row.rect.left_center() + vec2(10.0, -g.size().y / 2.0), g, color);
                automation::register_fmt(
                    ui,
                    row.rect,
                    format_args!("quarry.open_table.row.{}", row.index),
                    name,
                    Role::Row,
                    true,
                    Default::default(),
                );
            });
        if let Some(i) = out.select {
            p.selected = Some(i);
        }
        if let Some(i) = out.activated.or(out.double_clicked) {
            pick = hits.get(i).cloned();
        }
    });
    let conn = p.connection.clone();
    if let (Some(c), Some(t)) = (conn.clone(), pick) {
        return Some(PickerOutcome::Open(c, t));
    }
    match out {
        Some(DialogOutcome::Button(1)) => match (conn, p.selected.and_then(|i| hits.get(i)).cloned()) {
            (Some(c), Some(t)) => Some(PickerOutcome::Open(c, t)),
            _ => Some(PickerOutcome::Close),
        },
        Some(_) => Some(PickerOutcome::Close),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(ago(10_000, 5_000), "5 s ago");
        assert_eq!(ago(0, 5_000), "0 s ago");
        assert_eq!(ago(3_600_000, 0), "1 h ago");
        assert_eq!(ago(200_000, 0), "3 min ago");
        assert!(ago(u64::MAX, 0).ends_with("d ago"));
    }

    #[test]
    fn picker_lists_tables_with_schema_only_when_several() {
        let c = crate::sidebar::scene_connections("connected");
        let s = c.live.get(&ConnectionId::new("fixture")).and_then(|l| l.schema.clone());
        let p = Picker::new(Some(ConnectionId::new("fixture")), s.as_deref());
        assert_eq!(p.tables.iter().map(ToString::to_string).collect::<Vec<_>>(), ["customers", "events", "orders"]);
    }
}

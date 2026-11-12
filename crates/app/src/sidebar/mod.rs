//! The sidebar (240 pt): connection switcher and schema tree (connections › schemas › tables ›
//! columns) with row estimates, a filter, type-ahead, the keyboard and a context menu (open table,
//! copy name, new query). Owned by the `shell` stream.

pub mod commands;
pub mod connections;
pub mod tree;

use std::collections::BTreeSet;
use std::sync::Arc;

use ashlar::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, pos2, vec2};
use ashlar::ui::automation::{self, Details, Role};
use ashlar::ui::search::SearchField;
use ashlar::ui::tokens;
use ashlar::ui::virtual_list::{RowHeights, VirtualList};
use quarry_core::{AgentAccess, ColumnInfo, ConnColor, ConnectionId, ConnectionSpec, Namespace, SchemaTree, Table, TableKind, TableRef, Target};

use crate::theme;
use connections::{Connections, Live, Saved, Status};
use tree::{Row, RowKind};

/// What the person asked for in the sidebar this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    NewConnection,
    Connect(ConnectionId),
    Disconnect(ConnectionId),
    Edit(ConnectionId),
    OpenTable(ConnectionId, TableRef),
    /// A new editor tab on the connection, optionally with SQL.
    NewQuery(ConnectionId, Option<String>),
    Copy(String),
    /// The connection became the current one (selection).
    Select(ConnectionId),
}

/// The sidebar's own state (expansion, filter, selection, the open context menu).
#[derive(Default)]
pub struct SidebarState {
    pub expanded: BTreeSet<String>,
    pub filter: String,
    pub selected: Option<String>,
    rows: Vec<Row>,
    built: Option<(u64, u64, String)>,
    expanded_gen: u64,
    menu: Option<(usize, Pos2)>,
}

impl SidebarState {
    /// Open or close a node by its key (`tree::conn_key`, …).
    pub fn set_expanded(&mut self, key: &str, open: bool) {
        let changed = if open { self.expanded.insert(key.to_string()) } else { self.expanded.remove(key) };
        if changed {
            self.expanded_gen += 1;
        }
    }

    /// Open the context menu of the row with `key` at `at` (scenes).
    pub fn open_menu(&mut self, conns: &Connections, conns_rev: u64, key: &str, at: Pos2) {
        self.refresh(conns, conns_rev);
        if let Some(i) = self.rows.iter().position(|r| r.key == key) {
            self.selected = Some(key.to_string());
            self.menu = Some((i, at));
        }
    }

    fn refresh(&mut self, conns: &Connections, conns_rev: u64) {
        let want = (conns_rev, self.expanded_gen, self.filter.clone());
        if self.built.as_ref() != Some(&want) {
            self.rows = tree::rows(conns, &self.expanded, &self.filter);
            self.built = Some(want);
        }
    }

    /// Rows as last built (tests).
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }
}

/// Draw the sidebar into `ui` (the left panel's ui).
pub fn show(ui: &mut egui::Ui, state: &mut SidebarState, conns: &Connections, conns_rev: u64) -> Vec<Action> {
    let t = tokens::current(ui.ctx());
    let mut actions = Vec::new();
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("CONNECTIONS").size(11.0).color(t.text.secondary).family(theme::strong_family()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let plus = ui.add(egui::Button::new(egui::RichText::new("+").size(16.0)).frame(false).min_size(vec2(24.0, 22.0)));
            automation::register(ui, &plus, "quarry.sidebar.new_connection", "New Connection…", Role::Button);
            let plus = plus.on_hover_text("New connection");
            if plus.clicked() {
                actions.push(Action::NewConnection);
            }
        });
    });
    ui.add_space(4.0);
    if conns.saved.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            let r = ui.label(egui::RichText::new("No connections yet").color(t.text.secondary));
            automation::register(ui, &r, "quarry.sidebar.empty", "No connections yet", Role::Label);
            ui.add_space(8.0);
            let b = ui.add(egui::Button::new("New Connection…").min_size(vec2(150.0, 28.0)));
            automation::register(ui, &b, "quarry.sidebar.empty.new_connection", "New Connection…", Role::Button);
            if b.clicked() {
                actions.push(Action::NewConnection);
            }
        });
        return actions;
    }
    let filter = SearchField::new("quarry.sidebar.filter", &mut state.filter).label("Filter tables").hint("Filter tables").show(ui);
    if filter.changed || filter.cleared {
        state.menu = None;
    }
    ui.add_space(6.0);
    state.refresh(conns, conns_rev);
    let selected = state.selected.as_ref().and_then(|k| state.rows.iter().position(|r| &r.key == k));
    let rows = &state.rows;
    let label_of = |i: usize| rows.get(i).map_or("", |r| r.label.as_str());
    let mut toggles: Vec<(String, bool)> = Vec::new();
    let out = VirtualList::new("quarry.tree", rows.len(), RowHeights::Uniform(theme::ROW - 2.0))
        .label("Schema")
        .selected(selected)
        .row_label(&label_of)
        .type_ahead(&label_of)
        .show(ui, |ui, vrow| {
            let Some(r) = rows.get(vrow.index) else { return };
            if let Some(open) = paint_row(ui, vrow.rect, r, vrow.selected) {
                toggles.push((r.key.clone(), open));
            }
            let state_text = r.expandable.then_some(if r.expanded { "expanded" } else { "collapsed" });
            automation::register_fmt(
                ui,
                vrow.rect,
                format_args!("quarry.tree.{}", r.key),
                &r.label,
                Role::TreeItem,
                true,
                Details { value: state_text, selected: Some(vrow.selected) },
            );
        });
    if let Some(i) = out.select
        && let Some(r) = state.rows.get(i)
    {
        state.selected = Some(r.key.clone());
        if let Some(id) = conn_of(&r.kind) {
            actions.push(Action::Select(id.clone()));
        }
    }
    // keyboard: Right opens, Left closes
    if out.response.has_focus()
        && let Some(cur) = selected.and_then(|i| state.rows.get(i))
    {
        let (left, right) = ui.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight)));
        if right && cur.expandable && !cur.expanded {
            toggles.push((cur.key.clone(), true));
        } else if left && cur.expanded {
            toggles.push((cur.key.clone(), false));
        }
    }
    if let Some(i) = out.activated.or(out.double_clicked)
        && let Some(r) = state.rows.get(i)
    {
        match &r.kind {
            RowKind::Table { conn, table, .. } => actions.push(Action::OpenTable(conn.clone(), table.clone())),
            RowKind::Conn { .. } | RowKind::Schema { .. } => toggles.push((r.key.clone(), !r.expanded)),
            _ => {}
        }
    }
    if let Some(i) = out.secondary_clicked {
        let at = ui.ctx().pointer_latest_pos().unwrap_or(out.response.rect.left_top());
        state.menu = Some((i, at));
        if let Some(r) = state.rows.get(i) {
            state.selected = Some(r.key.clone());
        }
    }
    for (key, open) in toggles {
        state.set_expanded(&key, open);
        // opening a connection that is not connected connects it
        if open
            && let Some(id) = state.rows.iter().find(|r| r.key == key).and_then(|r| match &r.kind {
                RowKind::Conn { id, status: Status::Closed | Status::Failed(_), .. } => Some(id.clone()),
                _ => None,
            })
        {
            actions.push(Action::Connect(id));
        }
    }
    context_menu(ui, state, &mut actions);
    actions
}

fn conn_of(kind: &RowKind) -> Option<&ConnectionId> {
    match kind {
        RowKind::Conn { id, .. } => Some(id),
        RowKind::Schema { conn, .. } | RowKind::Table { conn, .. } | RowKind::Column { conn, .. } => Some(conn),
        RowKind::Loading | RowKind::Note { .. } => None,
    }
}

/// Paint one row; returns `Some(open)` when its disclosure was clicked.
fn paint_row(ui: &mut egui::Ui, rect: Rect, r: &Row, selected: bool) -> Option<bool> {
    let t = tokens::current(ui.ctx());
    let p = ui.painter().clone();
    let indent = 14.0;
    let x0 = rect.left() + 4.0 + f32::from(r.depth) * indent;
    let cy = rect.center().y;
    let mut toggled = None;
    if r.expandable {
        let arrow = Rect::from_center_size(pos2(x0 + 6.0, cy), vec2(16.0, rect.height()));
        let resp = ui.interact(arrow, ui.id().with(("disclosure", &r.key)), Sense::click());
        if resp.clicked() {
            toggled = Some(!r.expanded);
        }
        let c = if resp.hovered() { t.text.primary } else { t.text.secondary };
        chevron(&p, arrow.center(), r.expanded, c);
    }
    let icon = Rect::from_center_size(pos2(x0 + 20.0, cy), vec2(12.0, 12.0));
    let muted = t.text.secondary;
    let mut label_color = if selected { t.selection_text } else { t.text.primary };
    match &r.kind {
        RowKind::Conn { color, status, .. } => {
            let dot = theme::conn_color(*color).unwrap_or(muted);
            cylinder(&p, icon, dot);
            if let Status::Connected = status {
                p.circle_filled(icon.right_bottom() + vec2(1.0, 0.0), 2.5, theme::OK);
            }
        }
        RowKind::Schema { .. } => folder(&p, icon, muted),
        RowKind::Table { kind, .. } => match kind {
            TableKind::View | TableKind::MaterializedView => view_icon(&p, icon, theme::INFO),
            _ => grid_icon(&p, icon, theme::ACCENT),
        },
        RowKind::Column { primary_key, .. } => {
            if *primary_key {
                key_icon(&p, icon, theme::WARN);
            } else {
                p.circle_filled(icon.center(), 2.0, muted);
            }
        }
        RowKind::Loading => {
            let frac = match r.key.chars().last() {
                Some('0') => 0.7,
                Some('1') => 0.45,
                _ => 0.58,
            };
            let w = (rect.right() - icon.left() - 24.0).max(20.0) * frac;
            p.rect_filled(Rect::from_center_size(icon.center(), vec2(12.0, 10.0)), 3.0, t.surface.hover);
            p.rect_filled(Rect::from_min_size(pos2(icon.right() + 6.0, cy - 4.0), vec2(w, 8.0)), 4.0, t.surface.hover);
            return toggled;
        }
        RowKind::Note { error } => label_color = if *error { theme::ERROR } else { muted },
    }
    let label_x = if matches!(r.kind, RowKind::Note { .. }) { x0 + 6.0 } else { icon.right() + 6.0 };
    let detail_w = if r.detail.is_empty() {
        0.0
    } else {
        let g = p.layout_no_wrap(r.detail.clone(), FontId::monospace(11.0), muted);
        let w = g.size().x;
        p.galley(pos2(rect.right() - 8.0 - w, cy - g.size().y / 2.0), g, muted);
        w + 12.0
    };
    let max_w = (rect.right() - 8.0 - detail_w - label_x).max(8.0);
    let font = if matches!(r.kind, RowKind::Conn { .. }) {
        FontId::new(theme::UI_SIZE, theme::strong_family())
    } else {
        FontId::proportional(theme::UI_SIZE)
    };
    let mut job = egui::text::LayoutJob::simple_singleline(r.label.clone(), font, label_color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    let g = p.layout_job(job);
    p.galley(pos2(label_x, cy - g.size().y / 2.0), g, label_color);
    toggled
}

fn chevron(p: &egui::Painter, c: Pos2, open: bool, color: Color32) {
    let s = 3.5;
    let pts = if open {
        vec![c + vec2(-s, -s * 0.5), c + vec2(0.0, s * 0.6), c + vec2(s, -s * 0.5)]
    } else {
        vec![c + vec2(-s * 0.5, -s), c + vec2(s * 0.6, 0.0), c + vec2(-s * 0.5, s)]
    };
    p.add(egui::Shape::line(pts, Stroke::new(1.4, color)));
}

fn cylinder(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.3, c);
    let body = Rect::from_min_max(r.left_top() + vec2(1.0, 1.0), r.right_bottom() - vec2(1.0, 0.0));
    p.rect_stroke(body, 3.0, s, egui::StrokeKind::Inside);
    p.line_segment([pos2(body.left(), body.top() + 3.5), pos2(body.right(), body.top() + 3.5)], s);
}

fn folder(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.2, c);
    let body = Rect::from_min_max(r.left_top() + vec2(0.0, 3.0), r.right_bottom() - vec2(0.0, 1.0));
    p.rect_stroke(body, 2.0, s, egui::StrokeKind::Inside);
    p.line_segment([r.left_top() + vec2(1.0, 3.0), r.left_top() + vec2(5.0, 1.0)], s);
}

fn grid_icon(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.2, c);
    let b = r.shrink(0.5);
    p.rect_stroke(b, 2.0, s, egui::StrokeKind::Inside);
    p.line_segment([pos2(b.left(), b.top() + 4.0), pos2(b.right(), b.top() + 4.0)], s);
    p.line_segment([pos2(b.center().x, b.top() + 4.0), pos2(b.center().x, b.bottom())], s);
}

fn view_icon(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.2, c);
    p.rect_stroke(r.shrink(0.5), 2.0, s, egui::StrokeKind::Inside);
    p.circle_stroke(r.center(), 2.5, s);
}

fn key_icon(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.3, c);
    let ring = pos2(r.left() + 3.5, r.center().y);
    p.circle_stroke(ring, 2.8, s);
    p.line_segment([ring + vec2(2.8, 0.0), pos2(r.right(), r.center().y)], s);
    p.line_segment([pos2(r.right() - 2.0, r.center().y), pos2(r.right() - 2.0, r.center().y + 3.0)], s);
}

fn context_menu(ui: &mut egui::Ui, state: &mut SidebarState, actions: &mut Vec<Action>) {
    let Some((i, at)) = state.menu else { return };
    let Some(r) = state.rows.get(i).cloned() else {
        state.menu = None;
        return;
    };
    let mut items: Vec<(&str, &str, Action)> = Vec::new();
    match &r.kind {
        RowKind::Table { conn, table, .. } => {
            items.push(("quarry.tree.menu.open_table", "Open Table", Action::OpenTable(conn.clone(), table.clone())));
            items.push(("quarry.tree.menu.copy_name", "Copy Name", Action::Copy(table.to_string())));
            items.push((
                "quarry.tree.menu.new_query",
                "New Query",
                Action::NewQuery(conn.clone(), Some(format!("SELECT * FROM {} LIMIT 100;", quote(table)))),
            ));
        }
        RowKind::Column { conn, table, .. } => {
            items.push(("quarry.tree.menu.copy_name", "Copy Name", Action::Copy(r.label.clone())));
            items.push(("quarry.tree.menu.open_table", "Open Table", Action::OpenTable(conn.clone(), table.clone())));
        }
        RowKind::Conn { id, status, .. } => {
            items.push(("quarry.tree.menu.new_query", "New Query", Action::NewQuery(id.clone(), None)));
            if *status == Status::Connected {
                items.push(("quarry.tree.menu.disconnect", "Disconnect", Action::Disconnect(id.clone())));
            } else {
                items.push(("quarry.tree.menu.connect", "Connect", Action::Connect(id.clone())));
            }
            items.push(("quarry.tree.menu.edit", "Edit Connection…", Action::Edit(id.clone())));
            items.push(("quarry.tree.menu.copy_name", "Copy Name", Action::Copy(r.label.clone())));
        }
        RowKind::Schema { conn, name } => {
            items.push(("quarry.tree.menu.copy_name", "Copy Name", Action::Copy(name.clone())));
            items.push(("quarry.tree.menu.new_query", "New Query", Action::NewQuery(conn.clone(), None)));
        }
        RowKind::Loading | RowKind::Note { .. } => {}
    }
    if items.is_empty() {
        state.menu = None;
        return;
    }
    let area = egui::Area::new(egui::Id::new("quarry.tree.menu")).order(egui::Order::Foreground).fixed_pos(at).pivot(Align2::LEFT_TOP).show(
        ui.ctx(),
        |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                ui.set_min_width(170.0);
                for (id, label, action) in items {
                    let b = ui.add(egui::Button::new(label).frame(false).min_size(vec2(160.0, 24.0)));
                    automation::register(ui, &b, id, label, Role::MenuItem);
                    if b.clicked() {
                        actions.push(action);
                        state.menu = None;
                    }
                }
            })
        },
    );
    automation::register(ui, &area.response, "quarry.tree.menu", "Context menu", Role::Menu);
    let pressed_elsewhere = ui.input(|i| i.pointer.primary_pressed()) && !area.response.contains_pointer();
    if pressed_elsewhere || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.menu = None;
    }
}

/// A table name for SQL: quoted when it is not a plain lower-case identifier.
pub fn quote(t: &TableRef) -> String {
    let q = |s: &str| {
        if !s.is_empty()
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !s.starts_with(|c: char| c.is_ascii_digit())
        {
            s.to_string()
        } else {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
    };
    match &t.schema {
        Some(s) => format!("{}.{}", q(s), q(&t.name)),
        None => q(&t.name),
    }
}

/// The demo connections and schema the scenes show (mirrors `cargo xtask fixtures`). `status`:
/// `connected`, `connecting` or `failed`.
pub fn scene_connections(status: &str) -> Connections {
    let spec = ConnectionSpec {
        id: ConnectionId::new("fixture"),
        name: "quarry-10M.sqlite".into(),
        target: Target::Sqlite { path: "fixtures/quarry-10M.sqlite".into() },
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec!["password".into(), "token".into()],
        human_read_only: false,
    };
    let staging = ConnectionSpec {
        id: ConnectionId::new("staging"),
        name: "staging".into(),
        target: Target::Postgres {
            host: "db.staging.example.com".into(),
            port: 5432,
            db: "app".into(),
            user: "quarry".into(),
            tls: Default::default(),
            ssh: None,
        },
        color: ConnColor::Staging,
        agent_access: AgentAccess::ReadOnlyRole { role: "quarry_agent".into() },
        sensitive_patterns: vec![],
        human_read_only: false,
    };
    let col =
        |n: &str, ty: &str, pk: bool, nullable: bool| ColumnInfo { name: n.into(), type_name: ty.into(), nullable, default: None, primary_key: pk };
    let table = |n: &str, rows: u64, cols: Vec<ColumnInfo>| Table {
        name: n.into(),
        kind: TableKind::Table,
        row_estimate: Some(rows),
        columns: cols,
        indexes: vec![],
        foreign_keys: vec![],
    };
    let schema = SchemaTree {
        namespaces: vec![Namespace {
            name: "main".into(),
            tables: vec![
                table(
                    "customers",
                    1_000_000,
                    vec![
                        col("id", "INTEGER", true, false),
                        col("name", "TEXT", false, false),
                        col("email", "TEXT", false, false),
                        col("country", "TEXT", false, false),
                        col("created_at", "TEXT", false, false),
                        col("password_hash", "TEXT", false, false),
                    ],
                ),
                table(
                    "events",
                    10_000_000,
                    vec![
                        col("id", "INTEGER", true, false),
                        col("customer_id", "INTEGER", false, false),
                        col("order_id", "INTEGER", false, true),
                        col("kind", "TEXT", false, false),
                        col("at", "TEXT", false, false),
                        col("payload", "TEXT", false, false),
                    ],
                ),
                table(
                    "orders",
                    10_000_000,
                    vec![
                        col("id", "INTEGER", true, false),
                        col("customer_id", "INTEGER", false, false),
                        col("status", "TEXT", false, false),
                        col("total_cents", "INTEGER", false, false),
                        col("currency", "TEXT", false, false),
                        col("created_at", "TEXT", false, false),
                        col("note", "TEXT", false, true),
                    ],
                ),
            ],
        }],
    };
    let mut c = Connections::default();
    let id = spec.id.clone();
    c.saved.push(Saved { spec, approve_similar: false });
    c.saved.push(Saved { spec: staging, approve_similar: false });
    let live = c.live_mut(&id);
    *live = Live::default();
    live.status = match status {
        "connecting" => Status::Connecting,
        "failed" => Status::Failed("cannot connect: unable to open database file".into()),
        _ => Status::Connected,
    };
    if live.status == Status::Connected {
        live.schema = Some(Arc::new(schema));
        live.schema_rev = 1;
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote(&TableRef::new(None, "orders")), "orders");
        assert_eq!(quote(&TableRef::new(Some("public"), "Orders")), "public.\"Orders\"");
        assert_eq!(quote(&TableRef::new(None, "a\"b")), "\"a\"\"b\"");
        assert_eq!(quote(&TableRef::new(None, "1x")), "\"1x\"");
    }

    #[test]
    fn scene_fixture_has_three_tables() {
        let c = scene_connections("connected");
        let id = ConnectionId::new("fixture");
        assert_eq!(c.status(&id), Status::Connected);
        assert_eq!(c.live.get(&id).and_then(|l| l.schema.as_ref()).map(|s| s.table_count()), Some(3));
        assert!(matches!(scene_connections("failed").status(&id), Status::Failed(_)));
    }
}

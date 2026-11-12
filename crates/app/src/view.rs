//! The main window (DESIGN.md): toolbar 44, sidebar 240, tabs with the SQL editor over the result
//! grid, the agent panel (right, 360), status bar 24, and the dialogs. Owned by the `shell`
//! stream; the grid and agent regions are drawn by their streams' modules.
//!
//! State that is not the document lives in [`Shell`], held by the view closure: saved and live
//! connections, the tree, per-tab editor state and runs, the open dialog. Commands cannot start
//! jobs in ashlar 0.2, so connection commands queue a [`Request`] in view state and the view
//! does the work on its next frame. Running SQL never goes through a command: it needs a
//! [`HumanTicket`] (⌘↵ / ⇧⌘↵ or the Run buttons).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ashlar::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, Stroke, vec2};
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Details, Role};
use ashlar::ui::tabs::TabStrip;
use ashlar::ui::toasts::Toast;
use ashlar::ui::tokens;
use quarry_core::{Column, ConnectionId, CursorId, EngineKind, Page, SchemaTree, Tab, TabId, TabKind, Total, Workspace};
use quarry_engine::{CancelToken, SchemaFilter};
use serde::{Deserialize, Serialize};

use crate::dialogs::{self, Open, connection as conn_dialog};
use crate::editor::{self, Outcome, Rows, RunError, RunState, TabState};
use crate::human::{HumanTicket, RunScope};
use crate::sidebar::connections::{Connections, Human, Status, read_secret};
use crate::sidebar::{self, SidebarState, tree};
use crate::{agent, grid, ids, theme};

/// View-state key of the queued [`Request`]s.
pub const REQUESTS: &str = "shell.requests";
/// At most this many requests wait for the next frame.
const MAX_REQUESTS: usize = 64;
/// Schema loads stop after this many tables (the tree says so).
const MAX_TABLES: usize = 50_000;

/// Work a command asks the window to do on its next frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Request {
    NewConnection,
    EditConnection { connection: ConnectionId },
    Connect { connection: ConnectionId },
    Disconnect { connection: ConnectionId },
    Test { connection: ConnectionId },
    TablePicker { connection: Option<ConnectionId> },
    Cancel { tab: Option<TabId> },
    History { search: Option<String> },
}

/// Queue `r` for the window (from a command body).
pub fn request(s: &mut Session<Workspace>, r: Request) {
    let mut q: Vec<Request> = s.view(REQUESTS).unwrap_or_default();
    if q.len() < MAX_REQUESTS {
        q.push(r);
    }
    s.set_view(REQUESTS, &q);
}

/// Finished connection jobs.
enum Event {
    Connected {
        id: ConnectionId,
        human: Human,
        cancel: CancelToken,
        schema: Result<SchemaTree, String>,
    },
    ConnectFailed {
        id: ConnectionId,
        message: String,
    },
    /// A table tab's rows (or why they could not be read).
    TableLoaded {
        tab: TabId,
        key: String,
        connection: ConnectionId,
        table: quarry_core::TableRef,
        sql: String,
        result: Result<editor::Rows, String>,
    },
    /// `dialog`: the test came from the dialog's draft.
    Tested {
        name: String,
        dialog: bool,
        result: Result<String, String>,
    },
}

/// Everything the window keeps besides the document.
#[derive(Default)]
pub struct Shell {
    pub conns: Connections,
    conns_rev: u64,
    loaded: bool,
    pub sidebar: SidebarState,
    pub tabs: HashMap<TabId, TabState>,
    events: Arc<Mutex<Vec<Event>>>,
    runs: editor::Inbox,
    dialog: Option<Open>,
    /// The connection new tabs and the status bar use when the active tab has none.
    pub current: Option<ConnectionId>,
    generation: Option<u64>,
    screen: String,
}

pub fn view() -> impl FnMut(&mut egui::Ui, &mut Cx<'_, Workspace>) {
    let mut shell = Shell::default();
    move |ui, cx| shell.frame(ui, cx)
}

impl Shell {
    fn frame(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
        theme::frame_start(ui.ctx());
        if !self.loaded {
            self.conns = Connections::load(cx.settings);
            self.current = self.conns.saved.first().map(|s| s.spec.id.clone());
            self.loaded = true;
            self.conns_rev += 1;
        }
        self.sync_view_state(cx);
        self.drain(cx);
        self.shortcuts(ui, cx);
        self.toolbar(ui, cx);
        self.status_bar(ui, cx);
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin { left: 6, right: 6, top: 0, bottom: 6 });
        egui::Panel::left("quarry.sidebar").resizable(true).default_size(theme::SIDEBAR).min_size(theme::SIDEBAR).max_size(480.0).frame(frame).show(
            ui,
            |ui| {
                let r = ui.interact(ui.max_rect(), egui::Id::new("quarry.sidebar.region"), egui::Sense::hover());
                automation::register(ui, &r, "quarry.sidebar", "Sidebar", Role::Group);
                let actions = sidebar::show(ui, &mut self.sidebar, &self.conns, self.conns_rev);
                for a in actions {
                    self.sidebar_action(ui.ctx(), cx, a);
                }
            },
        );
        if cx.view_or("agent_panel", false) {
            let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(10));
            egui::Panel::right("quarry.agent").exact_size(theme::AGENT_PANEL).frame(frame).show(ui, |ui| agent::show(ui, cx));
        }
        if !self.screen.is_empty() {
            let r = ui.interact(
                egui::Rect::from_min_size(ui.max_rect().right_bottom() - vec2(1.0, 1.0), vec2(1.0, 1.0)),
                egui::Id::new("quarry.screen"),
                egui::Sense::hover(),
            );
            automation::register(ui, &r, &format!("quarry.screen.{}", self.screen), &self.screen, Role::Label);
        }
        egui::Frame::new().fill(theme::BG).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            self.center(ui, cx);
        });
        self.dialogs(ui.ctx(), cx);
    }

    // ------------------------------------------------------------------ view state, jobs

    fn sync_view_state(&mut self, cx: &mut Cx<'_, Workspace>) {
        let generation = cx.session().view_generation();
        if self.generation == Some(generation) {
            return;
        }
        let screen: String = cx.view_or("screen", String::new());
        if screen != self.screen {
            self.apply_scene(cx, &screen);
        }
        let queued: Vec<Request> = cx.view(REQUESTS).unwrap_or_default();
        if !queued.is_empty() {
            cx.set_view(REQUESTS, &Vec::<Request>::new());
            for r in queued {
                self.handle(cx, r);
            }
        }
        self.generation = Some(cx.session().view_generation());
    }

    /// A scene: demo connections and states (`crate::scenes`).
    fn apply_scene(&mut self, cx: &mut Cx<'_, Workspace>, screen: &str) {
        *self = Shell { loaded: true, conns_rev: self.conns_rev + 1, screen: screen.to_string(), ..Shell::default() };
        if let Some(status) = cx.view::<String>("scene.connections") {
            self.conns = sidebar::scene_connections(&status);
            self.current = Some(ConnectionId::new("fixture"));
        }
        for key in cx.view::<Vec<String>>("scene.expand").unwrap_or_default() {
            self.sidebar.set_expanded(&key, true);
        }
        if let Some(key) = cx.view::<String>("scene.select") {
            self.sidebar.selected = Some(key);
        }
        if let Some(key) = cx.view::<String>("scene.menu") {
            self.sidebar.open_menu(&self.conns, self.conns_rev, &key, egui::pos2(150.0, 262.0));
        }
        let fixture = ConnectionId::new("fixture");
        let state: String = cx.view_or("query_state", String::new());
        if let Some(tab) = cx.doc().active {
            let st = self.tabs.entry(tab).or_default();
            st.run = match state.as_str() {
                "running" => Some(RunState::Running { job: None, connection: fixture.clone() }),
                "results" => Some(RunState::Done(Outcome {
                    connection: fixture.clone(),
                    sql: String::new(),
                    elapsed_ms: 42,
                    statements: 1,
                    result: Ok(Rows {
                        columns: Arc::from(Vec::<Column>::new()),
                        total: Total::AtLeast(10_000_000),
                        first: Page { first_row: 0, rows: vec![Vec::new(); 1_000] },
                        cursor: CursorId(0),
                    }),
                })),
                "error" => Some(RunState::Done(Outcome {
                    connection: fixture.clone(),
                    sql: String::new(),
                    elapsed_ms: 3,
                    statements: 1,
                    result: Err(RunError { message: "no such table: orderz".into(), position: Some(15), statement_start: 0 }),
                })),
                _ => None,
            };
        }
        match cx.view::<String>("scene.dialog").as_deref() {
            Some("history") => {
                let now = crate::services::now().0;
                let entries = [
                    ("SELECT status, count(*) FROM orders GROUP BY status;", 5, Some(11), 184),
                    ("SELECT * FROM customers WHERE country = 'DE' LIMIT 100;", 3_600, Some(100), 12),
                    ("SELECT id, customer_id, total_cents, status FROM orders ORDER BY id;", 7_200, Some(1_000), 42),
                    ("SELECT * FROM orderz;", 90_000, None, 3),
                ]
                .into_iter()
                .map(|(sql, ago, rows, ms)| editor::history::Entry {
                    sql: sql.into(),
                    at: now.saturating_sub(ago * 1000),
                    ms,
                    rows,
                    error: rows.is_none().then(|| "no such table: orderz".into()),
                })
                .collect();
                self.dialog =
                    Some(Open::History(dialogs::HistoryDialog { connection: Some(fixture), entries, search: String::new(), selected: Some(0) }));
            }
            Some("open_table") => {
                let schema = self.conns.live.get(&fixture).and_then(|l| l.schema.clone());
                self.dialog = Some(Open::TablePicker(dialogs::Picker::new(Some(fixture), schema.as_deref())));
            }
            _ => {}
        }
        if cx.view_or("dialog", String::new()) == "connection" {
            let draft = conn_dialog::Draft {
                name: "staging".into(),
                host: "db.staging.example.com".into(),
                db: "app".into(),
                user: "quarry".into(),
                color: 2,
                ..Default::default()
            };
            self.dialog = Some(Open::Connection(Box::new(draft)));
        }
    }

    fn handle(&mut self, cx: &mut Cx<'_, Workspace>, r: Request) {
        match r {
            Request::NewConnection => self.dialog = Some(Open::Connection(Box::default())),
            Request::EditConnection { connection } => match self.conns.get(&connection) {
                Some(s) => self.dialog = Some(Open::Connection(Box::new(conn_dialog::Draft::from_saved(s)))),
                None => unknown(cx, &connection),
            },
            Request::Connect { connection } => self.connect(cx, &connection),
            Request::Disconnect { connection } => {
                crate::services::services().connections().detach_human(&connection);
                self.conns.live.remove(&connection);
                self.conns_rev += 1;
            }
            Request::Test { connection } => match self.conns.spec(&connection).cloned() {
                Some(spec) => self.test(cx, spec, None, false),
                None => unknown(cx, &connection),
            },
            Request::TablePicker { connection } => {
                let conn = connection.or_else(|| self.active_connection(cx));
                let schema = conn.as_ref().and_then(|c| self.conns.live.get(c)).and_then(|l| l.schema.clone());
                self.dialog = Some(Open::TablePicker(dialogs::Picker::new(conn, schema.as_deref())));
            }
            Request::Cancel { tab } => self.cancel(cx, tab),
            Request::History { search } => {
                let conn = self.active_connection(cx);
                let entries = conn.as_ref().map(|c| editor::history::load(cx.settings, c)).unwrap_or_default();
                self.dialog =
                    Some(Open::History(dialogs::HistoryDialog { connection: conn, entries, search: search.unwrap_or_default(), selected: None }));
            }
        }
    }

    fn connect(&mut self, cx: &mut Cx<'_, Workspace>, id: &ConnectionId) {
        let Some(spec) = self.conns.spec(id).cloned() else {
            unknown(cx, id);
            return;
        };
        if matches!(self.conns.status(id), Status::Connecting | Status::Connected) {
            return;
        }
        let live = self.conns.live_mut(id);
        live.status = Status::Connecting;
        live.schema = None;
        live.schema_error = None;
        self.conns_rev += 1;
        self.sidebar.set_expanded(&tree::conn_key(id), true);
        self.current = Some(id.clone());
        let events = Arc::clone(&self.events);
        let name = spec.name.clone();
        cx.jobs.job("connect", format!("Connecting to {name}")).spawn(spec, move |spec, _| {
            let secret = match spec.engine() {
                EngineKind::Postgres => read_secret(&spec.id),
                EngineKind::Sqlite => None,
            };
            let id = spec.id.clone();
            let ev = match quarry_engine::engine(spec.engine()).connect_human(&spec, secret.as_ref()) {
                Ok(mut h) => {
                    // agent tools reach only connections the person opened (agent stream)
                    let agent_secret = match spec.engine() {
                        EngineKind::Postgres => read_secret(&spec.id),
                        EngineKind::Sqlite => None,
                    };
                    crate::services::services().connections().register(spec.clone(), agent_secret);
                    let cancel = h.cancel_token();
                    let schema = h.schema(&SchemaFilter { max_tables: MAX_TABLES, ..Default::default() }).map_err(|e| e.to_string());
                    let human: Human = Arc::new(Mutex::new(h));
                    // approved writes and commits run on this same connection, not one of their own
                    crate::services::services().connections().attach_human(&id, Arc::clone(&human));
                    Event::Connected { id, human, cancel, schema }
                }
                Err(e) => Event::ConnectFailed { id, message: e.to_string() },
            };
            events.lock().unwrap_or_else(PoisonError::into_inner).push(ev);
            Ok(apply(|_: &mut Session<Workspace>| Ok(Value::Null)))
        });
    }

    fn test(&mut self, cx: &mut Cx<'_, Workspace>, spec: quarry_core::ConnectionSpec, password: Option<String>, dialog: bool) {
        let events = Arc::clone(&self.events);
        cx.jobs.job("connect", format!("Testing {}", spec.name)).spawn((spec, password), move |(spec, password), _| {
            let secret = password.map(quarry_core::Secret::new).or_else(|| match spec.engine() {
                EngineKind::Postgres => read_secret(&spec.id),
                EngineKind::Sqlite => None,
            });
            let result = quarry_engine::engine(spec.engine())
                .connect_human(&spec, secret.as_ref())
                .and_then(|mut h| h.schema(&SchemaFilter { max_tables: 1, ..Default::default() }).map(|_| "connection works".to_string()))
                .map_err(|e| e.to_string());
            events.lock().unwrap_or_else(PoisonError::into_inner).push(Event::Tested { name: spec.name.clone(), dialog, result });
            Ok(apply(|_: &mut Session<Workspace>| Ok(Value::Null)))
        });
    }

    fn cancel(&mut self, cx: &mut Cx<'_, Workspace>, tab: Option<TabId>) {
        let Some(tab) = tab.or(cx.doc().active) else { return };
        if let Some(RunState::Running { job, connection }) = self.tabs.get(&tab).and_then(|s| s.run.clone()) {
            if let Some(token) = self.conns.live.get(&connection).and_then(|l| l.cancel.clone()) {
                token.cancel();
            }
            if let Some(job) = job {
                cx.jobs.cancel(job);
            }
        }
    }

    fn drain(&mut self, cx: &mut Cx<'_, Workspace>) {
        let events: Vec<Event> = std::mem::take(&mut *self.events.lock().unwrap_or_else(PoisonError::into_inner));
        for ev in events {
            match ev {
                Event::Connected { id, human, cancel, schema } => {
                    let live = self.conns.live_mut(&id);
                    live.status = Status::Connected;
                    live.human = Some(human);
                    live.cancel = Some(cancel);
                    live.schema_rev += 1;
                    match schema {
                        Ok(s) => live.schema = Some(Arc::new(s)),
                        Err(e) => live.schema_error = Some(e),
                    }
                    self.conns_rev += 1;
                }
                Event::ConnectFailed { id, message } => {
                    self.conns.live_mut(&id).status = Status::Failed(message);
                    self.conns_rev += 1;
                }
                Event::TableLoaded { tab, key, connection, table, sql, result } => {
                    // a newer load (another filter) replaced this one
                    if self.tabs.get(&tab).and_then(|t| t.table_key.as_deref()) != Some(key.as_str()) {
                        continue;
                    }
                    match (result, self.conns.human(&connection), self.conns.spec(&connection).cloned()) {
                        (Ok(rows), Some(human), Some(spec)) => {
                            let schema = self.conns.live.get(&connection).and_then(|l| l.schema.clone());
                            let target = crate::table::edit_target(&spec, schema.as_deref(), &table, &rows.columns);
                            let r = (rows.columns.clone(), rows.total, rows.cursor, rows.first.clone());
                            grid::open_rows(cx.session_mut(), tab, human, r, &sql, spec.engine());
                            grid::with_grid(cx.session(), tab, |g, _| match target {
                                Ok(t) => g.target = Some(t),
                                Err(note) => g.read_only_note = Some(note),
                            });
                        }
                        (Ok(_), _, _) => grid::attach(cx.session_mut(), tab, grid::GridState::error("the connection closed", None)),
                        (Err(e), _, _) => grid::attach(cx.session_mut(), tab, grid::GridState::error(e, None)),
                    }
                }
                Event::Tested { name, dialog, result } => {
                    if let (true, Some(Open::Connection(d))) = (dialog, self.dialog.as_mut()) {
                        d.test = Some(match result {
                            Ok(m) => conn_dialog::TestState::Ok(m),
                            Err(e) => conn_dialog::TestState::Failed(e),
                        });
                    } else {
                        cx.toasts.push(match result {
                            Ok(_) => Toast::success(format!("{name}: connection works")),
                            Err(e) => Toast::error(format!("{name}: {e}")),
                        });
                    }
                }
            }
        }
        let runs: Vec<(TabId, Outcome)> = std::mem::take(&mut *self.runs.lock().unwrap_or_else(PoisonError::into_inner));
        for (tab, outcome) in runs {
            let entry = editor::history::Entry {
                sql: outcome.sql.clone(),
                at: crate::services::now().0,
                ms: outcome.elapsed_ms,
                rows: outcome.result.as_ref().ok().map(|r| r.first.rows.len() as u64),
                error: outcome.result.as_ref().err().map(|e| e.message.clone()),
            };
            if !entry.sql.is_empty() {
                editor::history::record(cx.settings, &outcome.connection, entry);
            }
            let state = if outcome.result.is_ok() { "results" } else { "error" };
            // the grid streams the rest of the cursor (grid stream)
            match (&outcome.result, self.conns.human(&outcome.connection)) {
                (Ok(rows), Some(human)) => {
                    let dialect = self.conns.spec(&outcome.connection).map_or(EngineKind::Sqlite, |s| s.engine());
                    let r = (rows.columns.clone(), rows.total, rows.cursor, rows.first.clone());
                    // the rows are the last statement's (a re-sort re-runs that one)
                    let last = outcome.sql.rsplit(";\n").next().unwrap_or(&outcome.sql);
                    grid::open_rows(cx.session_mut(), tab, human, r, last, dialect);
                }
                _ => grid::detach(cx.session_mut(), tab),
            }
            self.tabs.entry(tab).or_default().run = Some(RunState::Done(outcome));
            if cx.doc().active == Some(tab) {
                cx.set_view("query_state", &state);
            }
        }
    }

    /// Load a table tab's rows when its grid is missing or its table / filter changed: a read on
    /// the person's connection (`crate::table`), in a job.
    fn load_table(
        &mut self,
        cx: &mut Cx<'_, Workspace>,
        tab: TabId,
        conn: Option<&ConnectionId>,
        table: &quarry_core::TableRef,
        filter: Option<&str>,
    ) {
        let Some(conn) = conn else { return };
        let key = format!("{conn}|{table}|{}", filter.unwrap_or(""));
        let has_grid = grid::with_grid(cx.session(), tab, |_, _| ()).is_some();
        let st = self.tabs.entry(tab).or_default();
        match &st.table_key {
            // a grid someone else attached (a scene): keep it
            None if has_grid => {
                st.table_key = Some(key);
                return;
            }
            Some(k) if *k == key && has_grid => return,
            _ => {}
        }
        let Some(spec) = self.conns.spec(conn).cloned() else { return };
        let Some(human) = self.conns.human(conn) else {
            if self.screen.is_empty() && self.conns.status(conn) == Status::Closed {
                self.connect(cx, conn);
            }
            return;
        };
        st.table_key = Some(key.clone());
        let sql = match crate::table::select_sql(&spec, table, filter) {
            Ok(sql) => sql,
            Err(e) => {
                grid::attach(cx.session_mut(), tab, grid::GridState::error(e, None));
                return;
            }
        };
        grid::attach(cx.session_mut(), tab, grid::GridState::running());
        let events = Arc::clone(&self.events);
        let (connection, table) = (conn.clone(), table.clone());
        cx.jobs.job("query", format!("Reading {table}")).spawn((human, sql), move |(human, sql), _| {
            let result = crate::table::load(&human, &sql);
            events.lock().unwrap_or_else(PoisonError::into_inner).push(Event::TableLoaded { tab, key, connection, table, sql, result });
            Ok(apply(|_: &mut Session<Workspace>| Ok(Value::Null)))
        });
    }

    fn active_connection(&self, cx: &Cx<'_, Workspace>) -> Option<ConnectionId> {
        let doc = cx.doc();
        doc.active.and_then(|id| doc.tab(id)).and_then(|t| t.connection.clone()).or_else(|| self.current.clone())
    }

    // ------------------------------------------------------------------ running (human-only)

    fn shortcuts(&mut self, ui: &egui::Ui, cx: &mut Cx<'_, Workspace>) {
        if self.dialog.is_some() {
            return;
        }
        let ctx = ui.ctx().clone();
        // ⇧⌘↵ first: ⌘↵ would also match it logically
        let all = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter);
        if let Some(t) = HumanTicket::from_shortcut(&ctx, &all) {
            self.run(cx, t, RunScope::All);
        }
        // ⌘⌥↵ is the agent card's Approve (crate::agent::APPROVE_SHORTCUT): ⌘↵ would match it
        // logically and swallow it
        let alt_enter =
            ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if modifiers.alt)));
        let one = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
        if !alt_enter && let Some(t) = HumanTicket::from_shortcut(&ctx, &one) {
            self.run(cx, t, RunScope::Statement);
        }
    }

    fn run(&mut self, cx: &mut Cx<'_, Workspace>, ticket: HumanTicket, scope: RunScope) {
        let doc = cx.snapshot();
        let Some(tab) = doc.active.and_then(|id| doc.tab(id)) else { return };
        let TabKind::Editor { sql } = &tab.kind else {
            cx.toasts.push(Toast::info("Open an editor tab to run SQL"));
            return;
        };
        let Some(conn) = tab.connection.clone().or_else(|| self.current.clone()) else {
            cx.toasts.push(Toast::info("Choose a connection first"));
            return;
        };
        if matches!(self.tabs.get(&tab.id).and_then(|s| s.run.as_ref()), Some(RunState::Running { job: Some(_), .. })) {
            cx.toasts.push(Toast::info("A query is already running in this tab (Cmd+. cancels it)"));
            return;
        }
        let Some(human) = self.conns.human(&conn) else {
            cx.toasts.push(Toast::info("Connecting… run again when the connection is open"));
            self.connect(cx, &conn);
            return;
        };
        let st = self.tabs.entry(tab.id).or_default();
        let statements = editor::statements_for(sql, st.cursor, scope);
        if statements.is_empty() {
            cx.toasts.push(Toast::info("Nothing to run"));
            return;
        }
        if let Some(job) = editor::start_run(cx, ticket, tab.id, conn.clone(), human, statements, &self.runs) {
            self.tabs.entry(tab.id).or_default().run = Some(RunState::Running { job: Some(job), connection: conn.clone() });
            cx.set_view("query_state", &"running");
            if tab.connection.is_none() {
                let id = tab.id;
                cx.edit_merged("Set Connection", "quarry.tab.connection", move |w| {
                    if let Some(t) = w.tab_mut(id) {
                        t.connection = Some(conn);
                    }
                });
            }
        }
    }

    // ------------------------------------------------------------------ regions

    fn toolbar(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(10, 0)).stroke(Stroke::NONE);
        egui::Panel::top("quarry.toolbar").exact_size(theme::TOOLBAR).frame(frame).show_separator_line(true).show(ui, |ui| {
            let r = ui.interact(ui.max_rect(), egui::Id::new("quarry.toolbar.region"), egui::Sense::hover());
            automation::register(ui, &r, "quarry.toolbar", "Toolbar", Role::Group);
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let conn = self.active_connection(cx);
                self.connection_chip(ui, conn.as_ref());
                ui.add_space(8.0);
                let doc = cx.snapshot();
                let tab = doc.active.and_then(|id| doc.tab(id));
                let editor_tab = tab.is_some_and(|t| matches!(t.kind, TabKind::Editor { .. }));
                let running = tab.and_then(|t| self.tabs.get(&t.id)).is_some_and(|s| matches!(s.run, Some(RunState::Running { .. })));
                let mac = theme::platform(!self.screen.is_empty()) == ashlar::command::shortcut::Platform::Mac;
                let run = ui
                    .add_enabled(
                        editor_tab,
                        egui::Button::new(egui::RichText::new("▶  Run").color(theme::ON_ACCENT).family(theme::strong_family()))
                            .fill(theme::ACCENT)
                            .min_size(vec2(72.0, 28.0)),
                    )
                    .on_hover_text(if mac { "Run statement (⌘↵)" } else { "Run statement (Ctrl+Enter)" });
                automation::register(ui, &run, "quarry.toolbar.run", "Run", Role::Button);
                let run_all = ui.add_enabled(editor_tab, egui::Button::new("Run All").min_size(vec2(0.0, 28.0))).on_hover_text(if mac {
                    "Run all (⇧⌘↵)"
                } else {
                    "Run all (Ctrl+Shift+Enter)"
                });
                automation::register(ui, &run_all, "quarry.toolbar.run_all", "Run All", Role::Button);
                let ctx = ui.ctx().clone();
                if let Some(ticket) = HumanTicket::from_click(&ctx, &run) {
                    self.run(cx, ticket, RunScope::Statement);
                }
                if let Some(ticket) = HumanTicket::from_click(&ctx, &run_all) {
                    self.run(cx, ticket, RunScope::All);
                }
                let mut button = |ui: &mut egui::Ui, id: &str, label: &str, aid: &str, enabled: bool, hint: &str| {
                    let r = ui.add_enabled(enabled, egui::Button::new(label).min_size(vec2(0.0, 28.0))).on_hover_text(hint);
                    automation::register(ui, &r, aid, label, Role::Button);
                    if r.clicked() {
                        cx.dispatch(id, json!({}));
                    }
                };
                button(ui, ids::QUERY_CANCEL, "Cancel", "quarry.toolbar.cancel", running, if mac { "Cancel (⌘.)" } else { "Cancel (Ctrl+.)" });
                button(
                    ui,
                    ids::QUERY_EXPLAIN,
                    "Explain",
                    "quarry.toolbar.explain",
                    editor_tab,
                    if mac { "Explain (⌘E)" } else { "Explain (Ctrl+E)" },
                );
                button(ui, ids::QUERY_FORMAT, "Format", "quarry.toolbar.format", editor_tab, "Format SQL");
                button(ui, ids::HISTORY_SHOW, "History", "quarry.toolbar.history", true, "Query history");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open = cx.view_or("agent_panel", false);
                    let a = ui.add(egui::Button::selectable(open, "Agent").min_size(vec2(0.0, 28.0))).on_hover_text(if mac {
                        "Agent panel (⌘L)"
                    } else {
                        "Agent panel (Ctrl+L)"
                    });
                    automation::register_with(ui, &a, "quarry.toolbar.agent", "Agent", Role::Toggle, Details { value: None, selected: Some(open) });
                    if a.clicked() {
                        cx.dispatch(ids::AGENT_TOGGLE_PANEL, json!({}));
                    }
                    if let Some(spec) = conn.as_ref().and_then(|c| self.conns.spec(c))
                        && spec.human_read_only
                    {
                        badge(ui, "quarry.toolbar.read_only", "READ-ONLY", theme::WARN);
                    }
                });
            });
        });
    }

    fn connection_chip(&self, ui: &mut egui::Ui, conn: Option<&ConnectionId>) {
        let t = tokens::current(ui.ctx());
        let (label, color, status) = match conn.and_then(|c| self.conns.spec(c)) {
            Some(spec) => (spec.name.clone(), theme::conn_color(spec.color).unwrap_or(t.text.secondary), self.conns.status(&spec.id)),
            None => ("No connection".to_string(), t.text.disabled, Status::Closed),
        };
        let r = egui::Frame::new()
            .fill(theme::RAISED)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                    ui.label(theme::strong(&label, theme::UI_SIZE).color(theme::TEXT));
                    let s = match status {
                        Status::Connected => "connected",
                        Status::Connecting => "connecting…",
                        Status::Failed(_) => "disconnected",
                        Status::Closed => "",
                    };
                    if !s.is_empty() {
                        ui.label(egui::RichText::new(s).size(11.5).color(t.text.secondary));
                    }
                });
            })
            .response;
        automation::register(ui, &r, "quarry.toolbar.connection", &label, Role::Label);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
        let t = tokens::current(ui.ctx());
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(12, 0));
        egui::Panel::bottom("quarry.status").exact_size(theme::STATUS_BAR).frame(frame).show(ui, |ui| {
            let doc = cx.snapshot();
            let tab = doc.active.and_then(|id| doc.tab(id));
            let run = tab.and_then(|t| self.tabs.get(&t.id)).and_then(|s| s.run.as_ref());
            let conn = self.active_connection(cx);
            let spec = conn.as_ref().and_then(|c| self.conns.spec(c));
            let mut parts: Vec<String> = Vec::new();
            match run {
                Some(RunState::Running { .. }) => parts.push("running…".into()),
                Some(RunState::Done(o)) => {
                    match &o.result {
                        Ok(r) => {
                            let shown = r.first.rows.len() as u64;
                            let of = match r.total {
                                Total::Known(n) if n == shown => String::new(),
                                Total::Known(n) => format!(" of {}", tree::thousands(n)),
                                Total::AtLeast(n) => format!(" of ≥ {}", tree::thousands(n)),
                            };
                            parts.push(format!("rows {}{of}", tree::thousands(shown)));
                        }
                        Err(_) => parts.push("error".into()),
                    }
                    parts.push(format!("{} ms", o.elapsed_ms));
                }
                None => parts.push("rows —".into()),
            }
            let agent_on = spec.is_some_and(|s| s.agent_access.allows_agent());
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let small = |s: &str| egui::RichText::new(s).size(11.5).color(t.text.secondary);
                let text = parts.join(" · ");
                let r = ui.label(small(&text));
                automation::register_with(ui, &r, "quarry.status.result", &text, Role::Status, Details { value: Some(&text), selected: None });
                if let Some(spec) = spec {
                    let engine = match spec.engine() {
                        EngineKind::Postgres => "pg",
                        EngineKind::Sqlite => "sqlite",
                    };
                    ui.label(small(" · "));
                    let (dot, _) = ui.allocate_exact_size(vec2(12.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 3.0, theme::conn_color(spec.color).unwrap_or(t.text.secondary));
                    let c = format!("{engine}@{}", spec.name);
                    let r = ui.label(small(&c).color(theme::TEXT));
                    automation::register(ui, &r, "quarry.status.connection", &c, Role::Status);
                }
                ui.label(small(" · "));
                let agent = if agent_on { "agent: read-only" } else { "agent: off" };
                let r = ui.label(small(agent));
                automation::register(ui, &r, "quarry.status.agent", agent, Role::Status);
                // lit while the agent works (a tool call in the last seconds, an approved write
                // running); scenes set `agent_active`
                let active = if self.screen.is_empty() { agent::active_for(crate::services::now()) } else { None };
                if let Some(ms) = active {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(ms));
                }
                if agent_on && (active.is_some() || cx.view_or("agent_active", false)) {
                    ui.add_space(6.0);
                    badge(ui, "quarry.status.read_only", "READ-ONLY", theme::INFO);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let r = ui.label(small("built with ashlar"));
                    automation::register(ui, &r, "quarry.status.ashlar", "built with ashlar", Role::Label);
                });
            });
            let r = ui.interact(ui.max_rect(), egui::Id::new("quarry.status.region"), egui::Sense::hover());
            automation::register(ui, &r, "quarry.status", "Status", Role::Status);
        });
    }

    fn center(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
        let doc = cx.snapshot();
        let Some(tab) = doc.active.and_then(|id| doc.tab(id)) else {
            self.empty_state(ui, cx);
            return;
        };
        self.tab_strip(ui, cx, &doc.tabs, tab.id);
        if let TabKind::Table { table, filter } = &tab.kind {
            self.load_table(cx, tab.id, tab.connection.as_ref(), table, filter.as_deref());
        }
        let t = tokens::current(ui.ctx());
        let st = self.tabs.entry(tab.id).or_default();
        let editor_frame = egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin { left: 0, right: 8, top: 8, bottom: 4 });
        let h = ui.available_height();
        egui::Panel::top(egui::Id::new(("quarry.editor.split", tab.id.0)))
            .resizable(true)
            .default_size((h * 0.42).max(120.0))
            .min_size(80.0)
            .max_size((h - 80.0).max(81.0))
            .frame(editor_frame)
            .show(ui, |ui| editor::show(ui, cx, tab, st));
        let table_tab = matches!(tab.kind, TabKind::Table { .. });
        let (state, error) = match &st.run {
            _ if table_tab => ("results".to_string(), None),
            Some(RunState::Running { .. }) => ("running".to_string(), None),
            Some(RunState::Done(o)) => match &o.result {
                Ok(_) => ("results".to_string(), None),
                Err(e) => ("error".to_string(), Some(e.clone())),
            },
            None => (cx.view_or("query_state", String::new()), None),
        };
        let grid_frame = egui::Frame::new().fill(t.surface.app).inner_margin(egui::Margin::same(8));
        grid_frame.show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            if state == "running" {
                editor::running_strip(ui);
                ui.add_space(6.0);
            }
            if let (Some(e), TabKind::Editor { sql }) = (&error, &tab.kind) {
                editor::error_banner(ui, sql, e);
                ui.add_space(8.0);
            }
            grid::show_tab(ui, cx, tab.id, &state);
            for chart in doc.charts.iter().filter(|c| c.tab == Some(tab.id)) {
                quarry_viz::chart(ui, chart);
            }
        });
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, tabs: &[Tab], active: TabId) {
        let titles: Vec<String> = tabs.iter().map(|t| t.title.clone()).collect();
        let mut selected = tabs.iter().position(|t| t.id == active).unwrap_or(0);
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin { left: 6, right: 6, top: 4, bottom: 0 });
        egui::Panel::top("quarry.tabs.bar").exact_size(theme::TAB_STRIP).frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 36.0;
                let out = ui
                    .allocate_ui(vec2(w.max(60.0), theme::TAB_STRIP - 4.0), |ui| {
                        TabStrip::new("quarry.tabs", &titles, &mut selected).closable(true).label("Tabs").show(ui)
                    })
                    .inner;
                if let Some(i) = out.selected
                    && let Some(t) = tabs.get(i)
                {
                    cx.dispatch(ids::TAB_SELECT, json!({"tab": t.id}));
                }
                if let Some(i) = out.closed
                    && let Some(t) = tabs.get(i)
                {
                    self.tabs.remove(&t.id);
                    cx.dispatch(ids::TAB_CLOSE, json!({"tab": t.id}));
                }
                let plus = ui
                    .add(egui::Button::new(egui::RichText::new("+").size(16.0)).frame(false).min_size(vec2(28.0, 26.0)))
                    .on_hover_text("New editor tab");
                automation::register(ui, &plus, "quarry.tabs.new", "New Editor Tab", Role::Button);
                if plus.clicked() {
                    self.new_editor_tab(cx, self.active_connection(cx), String::new());
                }
            });
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>) {
        let t = tokens::current(ui.ctx());
        let current = self.current.clone().and_then(|c| self.conns.spec(&c).map(|s| (c, s.name.clone())));
        let status = current.as_ref().map(|(c, _)| self.conns.status(c)).unwrap_or_default();
        let connected = self.conns.live.values().any(|l| l.status == Status::Connected);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.3).max(40.0));
            let name = current.as_ref().map(|(_, n)| n.as_str()).unwrap_or("");
            let (title, body) = match &status {
                Status::Connecting => {
                    (format!("Connecting to {name}…"), "Loading the schema. The agent's read-only connection opens separately.".to_string())
                }
                Status::Failed(e) => (format!("Can't reach {name}"), e.clone()),
                _ if connected => (
                    "Open a table or start a query".to_string(),
                    "Double-click a table in the sidebar, or press Cmd+T for a new editor tab.".to_string(),
                ),
                _ => ("Connect to a database".to_string(), "PostgreSQL or SQLite. The agent gets its own read-only connection.".to_string()),
            };
            if status == Status::Connecting {
                ashlar::ui::controls::Spinner::new("quarry.empty.spinner", "Connecting").size(22.0).show(ui);
                ui.add_space(10.0);
            }
            let r = ui.label(theme::strong(&title, 20.0).color(t.text.primary));
            automation::register(ui, &r, "quarry.empty", &title, Role::Label);
            ui.add_space(6.0);
            let color = if matches!(status, Status::Failed(_)) { theme::ERROR } else { t.text.secondary };
            ui.label(egui::RichText::new(body).color(color));
            ui.add_space(16.0);
            let accent =
                |label: &str| egui::Button::new(egui::RichText::new(label).color(theme::ON_ACCENT)).fill(theme::ACCENT).min_size(vec2(160.0, 32.0));
            match (&status, &current) {
                (Status::Connecting, _) => {}
                (Status::Failed(_), Some((id, _))) => {
                    let b = ui.add(accent("Reconnect"));
                    automation::register(ui, &b, "quarry.empty.reconnect", "Reconnect", Role::Button);
                    if b.clicked() {
                        let id = id.clone();
                        self.connect(cx, &id);
                    }
                    ui.add_space(6.0);
                    let e = ui.add(egui::Button::new("Edit Connection…").frame(false));
                    automation::register(ui, &e, "quarry.empty.edit", "Edit Connection…", Role::Button);
                    if e.clicked() {
                        let id = id.clone();
                        self.handle(cx, Request::EditConnection { connection: id });
                    }
                }
                _ if connected => {
                    let b = ui.add(egui::Button::new("New Editor Tab").min_size(vec2(160.0, 32.0)));
                    automation::register(ui, &b, "quarry.empty.new_tab", "New Editor Tab", Role::Button);
                    if b.clicked() {
                        self.new_editor_tab(cx, self.current.clone(), String::new());
                    }
                }
                _ => {
                    let b = ui.add(accent("New Connection…"));
                    automation::register(ui, &b, "quarry.empty.new_connection", "New Connection…", Role::Button);
                    if b.clicked() {
                        cx.dispatch(ids::CONN_NEW, json!({}));
                    }
                }
            }
        });
    }

    fn new_editor_tab(&mut self, cx: &mut Cx<'_, Workspace>, connection: Option<ConnectionId>, sql: String) {
        let id = cx.edit("New Editor Tab", |w| {
            let id = TabId(w.next_id());
            let title = format!("Query {}", w.tabs.iter().filter(|t| matches!(t.kind, TabKind::Editor { .. })).count() + 1);
            w.tabs.push(Tab { id, title, connection, kind: TabKind::Editor { sql } });
            w.active = Some(id);
            id
        });
        self.tabs.entry(id).or_default().focus();
    }

    fn sidebar_action(&mut self, ctx: &egui::Context, cx: &mut Cx<'_, Workspace>, a: sidebar::Action) {
        match a {
            sidebar::Action::NewConnection => self.dialog = Some(Open::Connection(Box::default())),
            sidebar::Action::Connect(id) => self.connect(cx, &id),
            sidebar::Action::Disconnect(id) => {
                crate::services::services().connections().detach_human(&id);
                self.conns.live.remove(&id);
                self.conns_rev += 1;
            }
            sidebar::Action::Edit(id) => self.handle(cx, Request::EditConnection { connection: id }),
            sidebar::Action::OpenTable(conn, table) => {
                crate::sidebar::commands::open_table_tab(cx.session_mut(), conn, table);
            }
            sidebar::Action::NewQuery(conn, sql) => self.new_editor_tab(cx, Some(conn), sql.unwrap_or_default()),
            sidebar::Action::Copy(text) => {
                ctx.copy_text(text.clone());
                cx.toasts.push(Toast::info(format!("Copied {text}")));
            }
            sidebar::Action::Select(id) => self.current = Some(id),
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context, cx: &mut Cx<'_, Workspace>) {
        let Some(open) = self.dialog.as_mut() else { return };
        match open {
            Open::Connection(d) => {
                let conns = &self.conns;
                match conn_dialog::show(ctx, d, |name| conns.new_id(name)) {
                    Some(conn_dialog::Outcome::Cancel) => self.dialog = None,
                    Some(conn_dialog::Outcome::Save(ticket, saved, password)) => {
                        let id = saved.spec.id.clone();
                        match self.conns.save(ticket, cx.settings, saved, password.as_deref()) {
                            Ok(()) => {
                                self.dialog = None;
                                self.conns.live.remove(&id);
                                self.conns_rev += 1;
                                cx.toasts.push(Toast::success("Connection saved"));
                                self.connect(cx, &id);
                            }
                            Err(e) => {
                                if let Some(Open::Connection(d)) = self.dialog.as_mut() {
                                    d.error = Some(format!("Could not save: {e}"));
                                }
                            }
                        }
                    }
                    Some(conn_dialog::Outcome::Test(spec, password)) => self.test(cx, spec, password, true),
                    None => {}
                }
            }
            Open::History(h) => match dialogs::show_history(ctx, h, crate::services::now().0) {
                Some(dialogs::HistoryOutcome::Open(conn, sql)) => {
                    self.dialog = None;
                    self.new_editor_tab(cx, conn, sql);
                }
                Some(dialogs::HistoryOutcome::Close) => self.dialog = None,
                None => {}
            },
            Open::TablePicker(p) => match dialogs::show_picker(ctx, p) {
                Some(dialogs::PickerOutcome::Open(conn, table)) => {
                    self.dialog = None;
                    crate::sidebar::commands::open_table_tab(cx.session_mut(), conn, table);
                }
                Some(dialogs::PickerOutcome::Close) => self.dialog = None,
                None => {}
            },
        }
    }
}

fn unknown(cx: &mut Cx<'_, Workspace>, id: &ConnectionId) {
    cx.toasts.push(Toast::error(format!("No connection named {id}")));
}

fn badge(ui: &mut egui::Ui, id: &str, text: &str, color: Color32) {
    let r = egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.7)))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(theme::strong(text, 10.5).color(color));
        })
        .response;
    automation::register(ui, &r, id, text, Role::Label);
}

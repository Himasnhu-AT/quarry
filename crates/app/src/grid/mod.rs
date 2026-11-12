//! The result grid (`grid` stream): a virtualized table over a streaming cursor, with sorting,
//! column hide / reorder / freeze, NULL, binary, JSON and long-text rendering, a cell popover,
//! multi-cell selection, copy as TSV / CSV / JSON / INSERT, and pending table edits.
//!
//! - [`source`]: where rows come from ([`source::EngineSource`] on the person's connection,
//!   [`source::MemSource`] in memory).
//! - [`model`]: one grid's state ([`GridState`]): page cache, fetches in flight, sort, selection.
//! - [`draw`]: drawing on ashlar's `VirtualTable`, returning [`draw::Action`]s.
//! - [`edits`]: pending edits → statements and the review diff (committing is human-only).
//!
//! Grids are kept per tab in a process-wide registry (an ashlar 0.2 `Session` has no slot for
//! app state, and job applies reach only the session): whoever opens a cursor for a tab calls
//! [`attach`]; the view calls [`show_tab`].

pub mod cache;
pub mod cells;
pub mod columns;
pub mod commands;
pub mod copy;
pub mod draw;
pub mod edits;
pub mod model;
pub mod scenes;
pub mod source;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use ashlar::egui;
use ashlar::prelude::*;
use ashlar::ui::automation::{self, Role};
use quarry_core::{PendingEdits, TabId, Workspace};

pub use draw::{Action, DrawState};
pub use model::{EditTarget, GridState, Spawner, Status};

use crate::human::{self, HumanAction};

/// A tab's grid and its drawing state.
#[derive(Default)]
pub struct Slot {
    pub grid: Option<GridState>,
    pub draw: DrawState,
}

type Registry = Mutex<HashMap<TabId, Arc<Mutex<Slot>>>>;

fn registry() -> MutexGuard<'static, HashMap<TabId, Arc<Mutex<Slot>>>> {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Registry::default).lock().unwrap_or_else(PoisonError::into_inner)
}

fn slot(tab: TabId) -> Option<Arc<Mutex<Slot>>> {
    registry().get(&tab).cloned()
}

fn lock(s: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Show `grid` in `tab` (replacing what was there: a new statement's cursor, a running state,
/// an error).
pub fn attach(tab: TabId, grid: GridState) {
    let s = registry().entry(tab).or_default().clone();
    let mut s = lock(&s);
    s.grid = Some(grid);
    s.draw = DrawState::default();
}

/// Forget `tab`'s grid (the tab closed): its source and cursor are dropped.
pub fn detach(tab: TabId) {
    registry().remove(&tab);
}

/// Run `f` on `tab`'s grid.
pub fn with_grid<T>(tab: TabId, f: impl FnOnce(&mut GridState, &mut DrawState) -> T) -> Option<T> {
    let s = slot(tab)?;
    let mut s = lock(&s);
    let Slot { grid, draw } = &mut *s;
    grid.as_mut().map(|g| f(g, draw))
}

/// Fetch jobs on ashlar's job queue.
pub struct JobSpawner<'a>(pub &'a mut Jobs<Session<Workspace>>);

impl Spawner for JobSpawner<'_> {
    fn spawn(&mut self, label: String, work: Box<dyn FnOnce() + Send>) -> Option<u64> {
        let id = self.0.job("grid.fetch", label).spawn((), move |(), _progress| {
            work();
            Ok(apply(|_: &mut Session<Workspace>| Ok(Value::Null)))
        });
        Some(id.0)
    }
    fn cancel(&mut self, job: u64) {
        self.0.cancel(ashlar::jobs::JobId(job));
    }
}

/// The automation id prefix of the grid.
pub const ID: &str = "quarry.grid";

/// A tab with no grid: one line (the shell draws the running strip and the error banner).
pub fn show(ui: &mut egui::Ui, state: &str) {
    let text = match state {
        "running" => "Running… (Cancel ⌘.)",
        "error" => "No results: the statement failed.",
        _ => "No results yet. Run a statement with Cmd+Enter.",
    };
    let r = ui.weak(text);
    automation::register(ui, &r, ID, "Results", Role::Table);
}

/// Show the rows of a finished run in `tab`: `conn` holds the cursor `editor::start_run` opened
/// for `sql`, and `first` is the page it fetched. Replaces the tab's grid (closing its cursor).
pub fn open_rows(
    tab: TabId,
    conn: source::SharedConnection,
    rows: (std::sync::Arc<[quarry_core::Column]>, quarry_core::Total, quarry_core::CursorId, quarry_core::Page),
    sql: &str,
    dialect: quarry_core::EngineKind,
) {
    let (columns, total, cursor, first) = rows;
    let total = total.at_least(first.end_row());
    let src = source::CursorSource::new(conn, cursor, sql, Some(first));
    attach(tab, GridState::open(columns, total, Box::new(src)).dialect(dialect));
}

/// Draw `tab`'s grid (`state`: the shell's run state, `running` / `error` / `results` / ``),
/// fetch what it needs and apply what the person did.
pub fn show_tab(ui: &mut egui::Ui, cx: &mut Cx<'_, Workspace>, tab: TabId, state: &str) {
    // a new run is on its way: the old rows are stale
    if state == "running" {
        draw::skeleton(ui, ID, &GridState::running());
        return;
    }
    let Some(s) = slot(tab).filter(|_| state != "error") else {
        show(ui, state);
        return;
    };
    let mut s = lock(&s);
    let Slot { grid, draw: ds } = &mut *s;
    let Some(g) = grid.as_mut() else {
        show(ui, "");
        return;
    };
    if let Some(order) = g.requested_sort.take() {
        g.sort_by(order, &mut JobSpawner(cx.jobs));
    }
    let doc = cx.snapshot();
    let edits = doc.edits(tab);
    let (actions, visible) = draw::draw(ui, ID, g, ds, edits);
    if let Some(text) = g.clipboard.take() {
        ui.ctx().copy_text(text);
    }
    g.pump(visible, &mut JobSpawner(cx.jobs));
    for a in actions {
        apply_action(cx, tab, g, ds, a);
    }
}

/// The pending edits of `tab` in `w`, created for `g`'s table when missing.
fn edits_mut<'w>(w: &'w mut Workspace, tab: TabId, g: &GridState) -> Option<&'w mut PendingEdits> {
    let t = g.target.as_ref()?;
    if !w.edits.iter().any(|e| e.tab == tab) {
        let key = t.key.iter().filter_map(|&c| g.columns.get(c).map(|c| c.name.clone())).collect();
        w.edits.push(PendingEdits::new(tab, t.table.clone(), key));
    }
    w.edits.iter_mut().find(|e| e.tab == tab)
}

/// Drop empty edit sets.
fn tidy(w: &mut Workspace) {
    w.edits.retain(|e| !e.is_empty());
}

fn apply_action(cx: &mut Cx<'_, Workspace>, tab: TabId, g: &mut GridState, ds: &mut DrawState, a: Action) {
    match a {
        Action::Sort(order) => g.sort_by(order, &mut JobSpawner(cx.jobs)),
        Action::SetCell { row, col, value } => set_cell(cx.session_mut(), tab, g, row, col, value),
        Action::SetNewCell { n, col, value } => {
            let Some(name) = g.columns.get(col).map(|c| c.name.clone()) else { return };
            cx.edit("Edit New Row", |w| {
                if let Some(row) = edits_mut(w, tab, g).and_then(|e| e.insert_mut(n)) {
                    match row.iter_mut().find(|(k, _)| *k == name) {
                        Some((_, v)) => *v = value,
                        None => row.push((name, value)),
                    }
                }
            });
        }
        Action::DeleteRows(rows) => delete_rows(cx.session_mut(), tab, g, &rows),
        Action::AddRow => add_row(cx.session_mut(), tab, g),
        Action::Discard => {
            discard(cx.session_mut(), tab);
            ds.review = false;
        }
        Action::ToggleReview => ds.review = !ds.review,
        Action::Commit(ticket) => {
            if let Err(e) = human::perform(cx.session_mut(), ticket, HumanAction::CommitEdits { tab }) {
                g.notice = Some(e.to_string());
            }
        }
    }
}

/// Set shown row `row`, source column `col` to `value` as a pending edit (one undo step).
pub fn set_cell(s: &mut Session<Workspace>, tab: TabId, g: &mut GridState, row: u64, col: usize, value: quarry_core::Value) {
    let (Some(key), Some(name)) = (g.key_of(row), g.columns.get(col).map(|c| c.name.clone())) else { return };
    let old = g.cell(row, col).cloned().unwrap_or(quarry_core::Value::Null);
    s.edit("Edit Cell", |w| {
        if let Some(e) = edits_mut(w, tab, g) {
            e.set(key, &name, old, value);
        }
        tidy(w);
    });
}

/// Add a new row (all columns take their defaults until edited).
pub fn add_row(s: &mut Session<Workspace>, tab: TabId, g: &mut GridState) {
    if g.target.is_none() {
        g.notice = Some("Rows can be added to a table tab with a primary key".into());
        return;
    }
    s.edit("Add Row", |w| {
        if let Some(e) = edits_mut(w, tab, g) {
            e.insert(Vec::new());
        }
    });
}

/// Drop `tab`'s pending edits (one undo step: ⌘Z brings them back).
pub fn discard(s: &mut Session<Workspace>, tab: TabId) {
    if s.doc().edits(tab).is_some() {
        s.edit("Discard Pending Changes", |w| w.edits.retain(|e| e.tab != tab));
    }
}

/// Mark window rows `rows` of `g` for deletion (one undo step).
pub fn delete_rows(s: &mut Session<Workspace>, tab: TabId, g: &mut GridState, rows: &[u64]) {
    let mut picked = Vec::new();
    for &r in rows {
        let shown = g.window.saturating_add(r);
        let Some(key) = g.key_of(shown) else { continue };
        let before = g.row(shown).map(|vals| g.columns.iter().zip(vals).map(|(c, v)| (c.name.clone(), v.clone())).collect()).unwrap_or_default();
        picked.push((key, before));
    }
    if picked.is_empty() {
        g.notice = Some("Nothing to delete (rows not loaded, or the grid is not editable)".into());
        return;
    }
    s.edit("Delete Rows", |w| {
        if let Some(e) = edits_mut(w, tab, g) {
            for (k, b) in picked {
                e.delete(k, b);
            }
        }
    });
}

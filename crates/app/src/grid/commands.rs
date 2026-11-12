//! The grid's commands (`grid.*`, `edit.*` except the human-only `edit.commit`). They act on the
//! active tab's grid. Every surface can call them (ashlar 0.2 has no per-command surface filter),
//! so none returns cell contents: `grid.copy` puts text on the clipboard and reports only counts,
//! and the `edit.*` ones change only the pending set, which never reaches the database without a
//! person's Commit click.

use ashlar::prelude::*;
use quarry_core::{Tab, TabId, TabKind, Workspace};

use super::copy::Format;
use super::source::Order;
use super::{GridState, draw, with_grid};
use crate::services::services;

type S = Session<Workspace>;
type R = Result<Value, CommandError>;

fn fail(msg: impl Into<String>) -> CommandError {
    CommandError::failed("", msg.into())
}

fn no_grid() -> CommandError {
    fail("no_grid: the tab shows no results")
}

fn active(s: &S) -> Result<TabId, CommandError> {
    s.doc().active.ok_or_else(|| fail("no_tab: no tab is open"))
}

fn on_grid<T>(s: &mut S, tab: TabId, f: impl FnOnce(&mut GridState, &mut super::DrawState) -> T) -> Result<T, CommandError> {
    let w = super::window(s);
    super::with_in(w, tab, f).ok_or_else(no_grid)
}

/// `grid.copy {format}`: copy the selected cells (whole rows when no cells are picked).
pub fn copy(s: &mut S, p: &Value) -> R {
    let format = Format::parse(p.get("format").and_then(Value::as_str).unwrap_or("tsv")).unwrap_or(Format::Tsv);
    let tab = active(s)?;
    let (rows, columns) = on_grid(s, tab, |g, _| g.copy(format))?;
    Ok(json!({"format": format.name(), "rows": rows, "columns": columns}))
}

/// `grid.sort {tab, column, descending}`: sort by a column (re-runs the statement unless the
/// result is small and fully loaded). Applied on the next frame (it may start a job).
pub fn sort(s: &mut S, p: &Value) -> R {
    let tab = TabId(p.get("tab").and_then(Value::as_u64).unwrap_or(0));
    let name = p.get("column").and_then(Value::as_str).unwrap_or("");
    let descending = p.get("descending").and_then(Value::as_bool).unwrap_or(false);
    let col =
        on_grid(s, tab, |g, _| g.columns.iter().position(|c| c.name == name))?.ok_or_else(|| fail(format!("unknown_column: no column `{name}`")))?;
    on_grid(s, tab, |g, _| g.requested_sort = Some(Some(Order { column: col, descending })))?;
    Ok(json!({"tab": tab, "column": name, "descending": descending}))
}

/// View-state key: the table tab whose filter field takes focus on the next frame (⌘F).
pub const FILTER_FOCUS: &str = "grid.filter_focus";

/// `grid.filter {tab, where}` (⌘F): filter a table tab. Without `where`, focus the tab's filter
/// field; an empty `where` clears the filter; otherwise the clause must pass
/// `quarry_guard::check_filter` (the same check as the agent's `view.filter`), and its canonical
/// rendering becomes the tab's filter, which re-reads the table with that WHERE.
pub fn filter(s: &mut S, p: &Value) -> R {
    let tab = match p.get("tab").and_then(Value::as_u64) {
        Some(t) => TabId(t),
        None => active(s)?,
    };
    let doc = s.snapshot();
    let t = doc.tab(tab).ok_or_else(|| fail(format!("not_found: no tab {}", tab.0)))?;
    let (TabKind::Table { .. }, Some(conn)) = (&t.kind, &t.connection) else {
        return Err(fail("not_table: filters apply to table tabs (open one with table.open)"));
    };
    let Some(clause) = p.get("where").and_then(Value::as_str) else {
        s.set_view(FILTER_FOCUS, &tab.0);
        return Ok(json!({"tab": tab, "focus": true}));
    };
    let applied = if clause.trim().is_empty() {
        None
    } else {
        let e = services().connections().get(conn.as_str()).ok_or_else(|| fail(format!("unknown_connection: `{conn}` is not open")))?;
        Some(quarry_guard::check_filter(clause, e.spec().engine(), &e.policy()).map_err(|r| fail(r.to_string()))?)
    };
    let set = applied.clone();
    s.edit("Filter Table", |w| {
        if let Some(Tab { kind: TabKind::Table { filter, .. }, .. }) = w.tab_mut(tab) {
            *filter = set;
        }
    });
    Ok(json!({"tab": tab, "where": applied}))
}

/// `edit.cell` (F2): edit the cell at the cursor.
pub fn edit_cell(s: &mut S, _: &Value) -> R {
    let tab = active(s)?;
    on_grid(s, tab, |g, _| {
        let row = g.selection.cursor().map(|r| g.window.saturating_add(r as u64));
        let shown = g.layout.shown();
        let col = g.cells.span.and_then(|(_, b)| shown.get(b).copied()).or_else(|| shown.first().copied());
        match (row, col) {
            (Some(r), Some(c)) => {
                draw::start_edit(g, r, c);
                Ok(json!({"editing": g.editing.is_some()}))
            }
            _ => Err(fail("no_cell: select a cell first")),
        }
    })?
}

/// `edit.add_row`.
pub fn add_row(s: &mut S, _: &Value) -> R {
    let tab = active(s)?;
    let w = super::window(s);
    super::with_in(w, tab, |g, _| super::add_row(s, tab, g)).ok_or_else(no_grid)?;
    Ok(json!({"pending": s.doc().edits(tab).map_or(0, |e| e.changes.len())}))
}

/// `edit.delete_rows`: mark the selected rows for deletion.
pub fn delete_rows(s: &mut S, _: &Value) -> R {
    let tab = active(s)?;
    let w = super::window(s);
    super::with_in(w, tab, |g, _| {
        let rows: Vec<u64> = g.selection.iter().map(|r| r as u64).collect();
        super::delete_rows(s, tab, g, &rows);
    })
    .ok_or_else(no_grid)?;
    Ok(json!({"pending": s.doc().edits(tab).map_or(0, |e| e.changes.len())}))
}

/// `edit.discard`: drop the active tab's pending edits (undoable).
pub fn discard(s: &mut S, _: &Value) -> R {
    let tab = active(s)?;
    super::discard(s, tab);
    let _ = with_grid(s, tab, |_, d| d.review = false);
    Ok(json!({"pending": 0}))
}

/// `edit.review` (⌘⌥S): show the pending changes as a diff with their SQL.
pub fn review(s: &mut S, _: &Value) -> R {
    let tab = active(s)?;
    let n = s.doc().edits(tab).map_or(0, |e| e.changes.len());
    on_grid(s, tab, |_, d| d.review = n > 0)?;
    Ok(json!({"pending": n, "open": n > 0}))
}

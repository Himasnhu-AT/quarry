//! A SQLite cursor: a prepared statement stepped forward as pages are fetched. Between fetches
//! the statement keeps its position: the `Rows` stepping it is forgotten instead of dropped
//! (dropping resets the statement; creating `Rows` does not), which leaks nothing.
//! Pages behind the
//! current position come from a small cache or, failing that, from re-running the statement and
//! skipping (stepping without reading values). The statement is finalized as soon as it reaches
//! its end, so a finished result holds no read lock.

use std::collections::VecDeque;
use std::sync::Arc;

use quarry_core::{Column, Page, Total, Value};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row, Statement};

use crate::cells;

/// Pages kept per cursor (the grid keeps the visible pages and their neighbours itself).
const CACHED_PAGES: usize = 4;

/// How a column's text is shown, from its declared type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decl {
    Plain,
    Bool,
    Json,
    Time,
}

impl Decl {
    pub(crate) fn of(decl: &str) -> Self {
        let d = decl.to_ascii_uppercase();
        if d.contains("JSON") {
            Decl::Json
        } else if d.contains("BOOL") {
            Decl::Bool
        } else if d.contains("DATE") || d.contains("TIME") {
            Decl::Time
        } else {
            Decl::Plain
        }
    }
}

fn text_cell(b: &[u8], d: Decl) -> Value {
    let cap = quarry_core::limits::CELL_CAP_BYTES;
    if b.len() > cap {
        let head = String::from_utf8_lossy(b.get(..cap.saturating_add(4)).unwrap_or(b));
        return Value::Clipped { preview: cells::clip_str(&head, cap).into(), len: b.len() as u64 };
    }
    let s = String::from_utf8_lossy(b);
    match d {
        Decl::Json => cells::json(&s),
        Decl::Time => cells::time(&s),
        Decl::Plain | Decl::Bool => cells::text(&s),
    }
}

pub(crate) fn cell(row: &Row<'_>, i: usize, d: Decl) -> Value {
    match row.get_ref(i) {
        Ok(ValueRef::Null) | Err(_) => Value::Null,
        Ok(ValueRef::Integer(n)) if d == Decl::Bool && (n == 0 || n == 1) => Value::Bool(n == 1),
        Ok(ValueRef::Integer(n)) => Value::Int(n),
        Ok(ValueRef::Real(f)) => Value::Float(f),
        Ok(ValueRef::Text(b)) => text_cell(b, d),
        Ok(ValueRef::Blob(b)) => cells::bytes(b),
    }
}

/// The result columns of a prepared statement.
pub(crate) fn describe(stmt: &Statement<'_>) -> (Vec<Column>, Vec<Decl>) {
    stmt.columns()
        .map(|c| {
            let decl = c.decl_type().unwrap_or("");
            (Column::new(c.name(), decl), Decl::of(decl))
        })
        .unzip()
}

pub(crate) struct SqlCursor<'c> {
    sql: String,
    stmt: Option<Statement<'c>>,
    /// Rows stepped past so far.
    pos: u64,
    done: bool,
    cap: Option<u64>,
    pub(crate) columns: Arc<[Column]>,
    decl: Vec<Decl>,
    cache: VecDeque<Page>,
    pub(crate) timeout_ms: Option<u32>,
}

impl<'c> SqlCursor<'c> {
    /// Prepare `sql` and read its first page (a statement without rows runs to completion here).
    pub(crate) fn open(conn: &'c Connection, sql: &str, cap: Option<u64>, first_page: u32, timeout_ms: Option<u32>) -> rusqlite::Result<Self> {
        let stmt = conn.prepare(sql)?;
        let (columns, decl) = describe(&stmt);
        let mut c = Self {
            sql: sql.to_string(),
            stmt: Some(stmt),
            pos: 0,
            done: false,
            cap,
            columns: Arc::from(columns),
            decl,
            cache: VecDeque::new(),
            timeout_ms,
        };
        if c.columns.is_empty() {
            // runs the statement; it has no rows
            c.step(u64::MAX, false)?;
            return Ok(c);
        }
        let rows = c.step(u64::from(first_page), true)?;
        c.remember(Page { first_row: 0, rows });
        Ok(c)
    }

    pub(crate) fn total(&self) -> Total {
        if self.done { Total::Known(self.pos) } else { Total::AtLeast(self.pos) }
    }

    fn remember(&mut self, page: Page) {
        if page.rows.is_empty() {
            return;
        }
        if self.cache.len() >= CACHED_PAGES {
            self.cache.pop_front();
        }
        self.cache.push_back(page);
    }

    /// Step up to `n` rows from the current position (within the cap), keeping them if `keep`.
    fn step(&mut self, n: u64, keep: bool) -> rusqlite::Result<Vec<Vec<Value>>> {
        let mut want = n;
        if let Some(cap) = self.cap {
            want = want.min(cap.saturating_sub(self.pos));
        }
        let mut out = Vec::new();
        if want == 0 {
            if self.cap.is_some_and(|c| self.pos >= c) {
                self.finish();
            }
            return Ok(out);
        }
        let Some(stmt) = self.stmt.as_mut() else {
            return Ok(out);
        };
        let decl = &self.decl;
        let mut got = 0u64;
        let mut ended = false;
        let mut rows = stmt.raw_query();
        while got < want {
            match rows.next()? {
                Some(row) => {
                    if keep {
                        out.push(decl.iter().enumerate().map(|(i, d)| cell(row, i, *d)).collect());
                    }
                    got += 1;
                }
                None => {
                    ended = true;
                    break;
                }
            }
        }
        if ended {
            drop(rows);
        } else {
            // keep the statement where it is for the next page
            std::mem::forget(rows);
        }
        self.pos = self.pos.saturating_add(got);
        if ended || self.cap.is_some_and(|c| self.pos >= c) {
            self.finish();
        }
        Ok(out)
    }

    /// The end is reached: finalize the statement (releases its read lock).
    fn finish(&mut self) {
        self.done = true;
        self.stmt = None;
    }

    fn cached(&self, first: u64, rows: u64) -> Option<Page> {
        let want_end = first.saturating_add(rows);
        let p = self.cache.iter().find(|p| p.first_row <= first && (p.end_row() >= want_end || (self.done && p.end_row() == self.pos)))?;
        let from = usize::try_from(first - p.first_row).ok()?;
        let to = usize::try_from(want_end.min(p.end_row()) - p.first_row).ok()?;
        Some(Page { first_row: first, rows: p.rows.get(from..to)?.to_vec() })
    }

    /// Rows `first .. first + rows`.
    pub(crate) fn fetch(&mut self, conn: &'c Connection, first: u64, rows: u32) -> rusqlite::Result<Page> {
        let rows = u64::from(rows);
        if let Some(p) = self.cached(first, rows) {
            return Ok(p);
        }
        if self.done && first >= self.pos {
            return Ok(Page { first_row: first, rows: Vec::new() });
        }
        if first < self.pos {
            self.stmt = Some(conn.prepare(&self.sql)?);
            self.pos = 0;
            self.done = false;
        }
        if first > self.pos {
            self.step(first - self.pos, false)?;
        }
        let page = Page { first_row: first, rows: if self.pos == first { self.step(rows, true)? } else { Vec::new() } };
        self.remember(page.clone());
        Ok(page)
    }
}

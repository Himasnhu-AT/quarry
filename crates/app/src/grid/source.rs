//! Where a grid's rows come from: an open cursor on the person's connection ([`EngineSource`]),
//! or rows in memory ([`MemSource`]: scenes, tests, small client-side results).
//!
//! A source is used from ashlar jobs only (never on the UI thread), except an
//! [`PageSource::instant`] one, which answers from memory.

use std::cmp::Ordering;
use std::sync::Arc;

use quarry_core::{Column, Page, Total, Value};
use quarry_engine::{CancelToken, Cursor, HumanConnection, QueryOptions};

/// Sort the result by one column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    /// Index into the source's columns.
    pub column: usize,
    pub descending: bool,
}

/// What a grid reads rows from.
pub trait PageSource: Send {
    /// Rows `first_row .. first_row + rows` (fewer at the end).
    fn fetch(&mut self, first_row: u64, rows: u32) -> Result<Page, String>;
    /// Run again, ordered by `order` (`None`: the original order); the grid then fetches from row 0.
    /// Returns the new total.
    fn requery(&mut self, order: Option<Order>) -> Result<Total, String>;
    /// Answers from memory at once: fetched on the UI thread, no job.
    fn instant(&self) -> bool {
        false
    }
    /// Cancels the statement in flight, from any thread.
    fn cancel_token(&self) -> Option<CancelToken> {
        None
    }
}

/// `sql` ordered by the `n`th output column (1-based ordinals need no quoting and work for
/// duplicate or unnamed columns) on PostgreSQL and SQLite.
pub fn ordered_sql(sql: &str, order: Order) -> String {
    let base = sql.trim().trim_end_matches(';').trim_end();
    let dir = if order.descending { "DESC" } else { "ASC" };
    format!("SELECT * FROM (\n{base}\n) AS quarry_sorted ORDER BY {} {dir}", order.column.saturating_add(1))
}

/// A cursor on the person's connection. Re-sorting re-runs the statement with an `ORDER BY`.
pub struct EngineSource {
    conn: Box<dyn HumanConnection>,
    cursor: Cursor,
    sql: String,
}

impl EngineSource {
    /// `cursor` was opened by running `sql` on `conn`.
    pub fn new(conn: Box<dyn HumanConnection>, cursor: Cursor, sql: impl Into<String>) -> Self {
        Self { conn, cursor, sql: sql.into() }
    }
    pub fn columns(&self) -> Arc<[Column]> {
        self.cursor.columns.clone()
    }
    pub fn total(&self) -> Total {
        self.cursor.total
    }
}

impl PageSource for EngineSource {
    fn fetch(&mut self, first_row: u64, rows: u32) -> Result<Page, String> {
        self.conn.fetch(self.cursor.id, first_row, rows).map_err(|e| e.to_string())
    }
    fn requery(&mut self, order: Option<Order>) -> Result<Total, String> {
        let sql = order.map_or_else(|| self.sql.clone(), |o| ordered_sql(&self.sql, o));
        let cursor = self.conn.query(&sql, &QueryOptions::default()).map_err(|e| e.to_string())?;
        self.conn.close(self.cursor.id);
        self.cursor = cursor;
        Ok(self.cursor.total)
    }
    fn cancel_token(&self) -> Option<CancelToken> {
        Some(self.conn.cancel_token())
    }
}

impl Drop for EngineSource {
    fn drop(&mut self) {
        self.conn.close(self.cursor.id);
    }
}

/// A connection shared with the window (the shell's `sidebar::connections::Human`).
pub type SharedConnection = std::sync::Arc<std::sync::Mutex<Box<dyn HumanConnection>>>;

/// A cursor the shell opened on a shared connection (`editor::start_run`), with the first page it
/// already fetched. Fetches lock the connection inside fetch jobs, never on the UI thread.
pub struct CursorSource {
    conn: SharedConnection,
    cursor: quarry_core::CursorId,
    sql: String,
    first: Option<Page>,
    cancel: CancelToken,
}

impl CursorSource {
    pub fn new(conn: SharedConnection, cursor: quarry_core::CursorId, sql: impl Into<String>, first: Option<Page>) -> Self {
        let cancel = lock_conn(&conn).cancel_token();
        Self { conn, cursor, sql: sql.into(), first, cancel }
    }
}

fn lock_conn(c: &SharedConnection) -> std::sync::MutexGuard<'_, Box<dyn HumanConnection>> {
    c.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PageSource for CursorSource {
    fn fetch(&mut self, first_row: u64, rows: u32) -> Result<Page, String> {
        // the page the run already fetched is handed over once
        if let Some(p) = self.first.take_if(|p| p.first_row == first_row) {
            return Ok(p);
        }
        lock_conn(&self.conn).fetch(self.cursor, first_row, rows).map_err(|e| e.to_string())
    }
    fn requery(&mut self, order: Option<Order>) -> Result<Total, String> {
        let sql = order.map_or_else(|| self.sql.clone(), |o| ordered_sql(&self.sql, o));
        let mut c = lock_conn(&self.conn);
        let cursor = c.query(&sql, &QueryOptions::default()).map_err(|e| e.to_string())?;
        c.close(self.cursor);
        self.cursor = cursor.id;
        self.first = None;
        Ok(cursor.total)
    }
    fn cancel_token(&self) -> Option<CancelToken> {
        Some(self.cancel.clone())
    }
}

impl Drop for CursorSource {
    fn drop(&mut self) {
        lock_conn(&self.conn).close(self.cursor);
    }
}

/// Rows generated on demand: row `i` → its values (a 10M-row result without 10M rows in memory).
pub type RowFn = Arc<dyn Fn(u64) -> Vec<Value> + Send + Sync>;

enum Rows {
    Vec(Arc<Vec<Vec<Value>>>),
    Gen { rows: u64, row: RowFn },
}

/// Rows in memory, or generated: scenes, tests, benches.
pub struct MemSource {
    rows: Rows,
    /// Display order → stored row (only for stored rows; generated ones do not sort).
    order: Option<Vec<u32>>,
    /// Report `AtLeast` totals, as a server-side cursor does before it reaches the end.
    streaming: bool,
    instant: bool,
    /// Fail every fetch with this message.
    fail: Option<String>,
}

impl MemSource {
    pub fn new(rows: Vec<Vec<Value>>) -> Self {
        Self { rows: Rows::Vec(Arc::new(rows)), order: None, streaming: false, instant: true, fail: None }
    }
    /// `rows` rows made by `row(i)`.
    pub fn generated(rows: u64, row: RowFn) -> Self {
        Self { rows: Rows::Gen { rows, row }, order: None, streaming: false, instant: true, fail: None }
    }
    /// Totals are `AtLeast` until a fetch reaches the end.
    pub fn streaming(mut self, on: bool) -> Self {
        self.streaming = on;
        self
    }
    /// Fetched by jobs, like a database (default: on the UI thread).
    pub fn deferred(mut self) -> Self {
        self.instant = false;
        self
    }
    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.fail = Some(message.into());
        self
    }
    pub fn len(&self) -> u64 {
        match &self.rows {
            Rows::Vec(v) => v.len() as u64,
            Rows::Gen { rows, .. } => *rows,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The total a fresh cursor reports.
    pub fn total(&self) -> Total {
        if self.streaming { Total::AtLeast(0) } else { Total::Known(self.len()) }
    }
    fn row(&self, i: u64) -> Option<Vec<Value>> {
        match &self.rows {
            Rows::Vec(v) => {
                let at = match &self.order {
                    Some(o) => u64::from(*o.get(usize::try_from(i).ok()?)?),
                    None => i,
                };
                v.get(usize::try_from(at).ok()?).cloned()
            }
            Rows::Gen { rows, row } => (i < *rows).then(|| row(i)),
        }
    }
}

impl PageSource for MemSource {
    fn fetch(&mut self, first_row: u64, rows: u32) -> Result<Page, String> {
        if let Some(m) = &self.fail {
            return Err(m.clone());
        }
        let end = first_row.saturating_add(u64::from(rows)).min(self.len());
        let rows = (first_row..end).filter_map(|i| self.row(i)).collect();
        Ok(Page { first_row, rows })
    }
    fn requery(&mut self, order: Option<Order>) -> Result<Total, String> {
        if let Some(m) = &self.fail {
            return Err(m.clone());
        }
        self.order = match (&self.rows, order) {
            (Rows::Vec(v), Some(o)) => {
                let mut idx: Vec<u32> = (0..u32::try_from(v.len()).map_err(|_| "too many rows to sort in memory".to_string())?).collect();
                idx.sort_by(|a, b| {
                    let get = |i: &u32| v.get(*i as usize).and_then(|r| r.get(o.column));
                    let c = compare(get(a), get(b));
                    if o.descending { c.reverse() } else { c }
                });
                Some(idx)
            }
            _ => None,
        };
        Ok(self.total())
    }
    fn instant(&self) -> bool {
        self.instant
    }
}

/// The order of two cells: NULLs last (first when reversed for a descending sort, as in
/// PostgreSQL), numbers by value, everything else by its text.
pub fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    fn num(v: &Value) -> Option<f64> {
        match v {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) if !f.is_nan() => Some(*f),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }
    let null = |v: Option<&Value>| v.is_none_or(Value::is_null);
    match (null(a), null(b)) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    let (Some(a), Some(b)) = (a, b) else { return Ordering::Equal };
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        return x.cmp(y);
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => super::cells::plain(a).cmp(&super::cells::plain(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_sql_wraps_and_strips_semicolons() {
        assert_eq!(
            ordered_sql("SELECT a, b FROM t;  ", Order { column: 1, descending: true }),
            "SELECT * FROM (\nSELECT a, b FROM t\n) AS quarry_sorted ORDER BY 2 DESC"
        );
    }

    #[test]
    fn mem_source_sorts_nulls_last_and_pages() {
        let rows = vec![vec![Value::Int(3)], vec![Value::Null], vec![Value::Int(1)], vec![Value::Int(2)]];
        let mut s = MemSource::new(rows);
        assert_eq!(s.requery(Some(Order { column: 0, descending: false })).unwrap(), Total::Known(4));
        let p = s.fetch(0, 10).unwrap();
        assert_eq!(p.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)], vec![Value::Null]]);
        s.requery(Some(Order { column: 0, descending: true })).unwrap();
        // descending puts NULLs first, as PostgreSQL does
        assert_eq!(s.fetch(0, 3).unwrap().rows, vec![vec![Value::Null], vec![Value::Int(3)], vec![Value::Int(2)]]);
        assert!(s.fetch(10, 5).unwrap().rows.is_empty());
        s.requery(None).unwrap();
        assert_eq!(s.fetch(0, 1).unwrap().rows, vec![vec![Value::Int(3)]]);
    }

    #[test]
    fn generated_rows_cover_ten_million() {
        let mut s = MemSource::generated(10_000_000, Arc::new(|i| vec![Value::Int(i as i64)]));
        let p = s.fetch(9_999_999, 1000).unwrap();
        assert_eq!(p.rows, vec![vec![Value::Int(9_999_999)]]);
        assert_eq!(s.total(), Total::Known(10_000_000));
    }
}

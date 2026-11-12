//! SQLite through rusqlite with the bundled library.
//!
//! Each connection runs on its own worker thread (rusqlite statements are not `Send`); the
//! [`HumanConnection`] / [`AgentConnection`] handles send it requests and wait for the answer.
//! Cancel and timeouts go through SQLite's progress handler, which stops only the statement
//! stepping now (`sqlite3_interrupt` would also fail the other open cursors' next steps).
//!
//! The agent connection (SECURITY.md layer A) is opened `SQLITE_OPEN_READ_ONLY`, runs
//! `PRAGMA query_only = 1`, defensive mode, `trusted_schema = 0`, no attached databases
//! (`SQLITE_LIMIT_ATTACHED = 0`), extension loading off (SQLite's default; never enabled), and the
//! authorizer in [`auth`] last, so the engine's own setup is the only thing that ran without it.

mod auth;
mod cursor;
mod schema;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};

use quarry_core::limits::{AGENT_TIMEOUT_MS_DEFAULT, AGENT_TIMEOUT_MS_MAX, PAGE_ROWS};
use quarry_core::{ConnectionSpec, CursorId, EngineKind, Estimate, Page, PlanNode, SchemaTree, Secret, Target};
use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, ErrorCode, OpenFlags};

use crate::{
    AgentConnection, Browse, CancelToken, Cursor, Engine, EngineError, HumanConnection, QueryOptions, ReadStatement, SchemaFilter, TxOutcome,
};
use cursor::SqlCursor;

/// Open cursors per connection; more are refused until some are closed.
const MAX_CURSORS: usize = 64;
/// How long a statement waits for another connection's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Rows of an `EXPLAIN QUERY PLAN` kept.
const PLAN_ROWS_MAX: usize = 10_000;

/// The SQLite engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteEngine;

/// The bundled SQLite library version (`3.x.y`).
pub fn library_version() -> &'static str {
    rusqlite::version()
}

fn path_of(spec: &ConnectionSpec) -> Result<&Path, EngineError> {
    match &spec.target {
        Target::Sqlite { path } => Ok(path),
        Target::Postgres { .. } => Err(EngineError::Connect("not a SQLite connection".into())),
    }
}

impl Engine for SqliteEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Sqlite
    }
    fn connect_human(&self, spec: &ConnectionSpec, _secret: Option<&Secret>) -> Result<Box<dyn HumanConnection>, EngineError> {
        let mode = if spec.human_read_only { Mode::HumanReadOnly } else { Mode::Human };
        Ok(Box::new(SqliteConn::open(path_of(spec)?, mode)?))
    }
    fn connect_agent(&self, spec: &ConnectionSpec, _secret: Option<&Secret>) -> Result<Box<dyn AgentConnection>, EngineError> {
        if !spec.agent_access.allows_agent() {
            return Err(EngineError::Connect("agent access is off for this connection".into()));
        }
        Ok(Box::new(SqliteConn::open(path_of(spec)?, Mode::Agent)?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Human,
    HumanReadOnly,
    Agent,
}

/// What the progress handler checks: cancel and the deadline of the request running now.
struct Ctl {
    base: Instant,
    /// Milliseconds after `base`; 0: none.
    deadline_ms: AtomicU64,
    cancel: AtomicBool,
    timed_out: AtomicBool,
    busy: AtomicBool,
}

impl Ctl {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn begin(&self, timeout_ms: Option<u32>) {
        self.cancel.store(false, Ordering::SeqCst);
        self.timed_out.store(false, Ordering::SeqCst);
        let d = timeout_ms.map_or(0, |t| self.now_ms().saturating_add(u64::from(t)).max(1));
        self.deadline_ms.store(d, Ordering::SeqCst);
        self.busy.store(true, Ordering::SeqCst);
    }
    fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
        self.deadline_ms.store(0, Ordering::SeqCst);
    }
    fn should_stop(&self) -> bool {
        if self.cancel.load(Ordering::SeqCst) {
            return true;
        }
        let d = self.deadline_ms.load(Ordering::SeqCst);
        if d != 0 && self.now_ms() > d {
            self.timed_out.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
    fn error(&self, e: rusqlite::Error) -> EngineError {
        match &e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::OperationInterrupted => {
                if self.timed_out.load(Ordering::SeqCst) {
                    EngineError::Timeout
                } else {
                    EngineError::Cancelled
                }
            }
            rusqlite::Error::SqliteFailure(f, msg) if f.code == ErrorCode::AuthorizationForStatementDenied => EngineError::Database {
                message: format!("{}: the connection is read-only", msg.as_deref().unwrap_or("not authorized")),
                position: None,
            },
            _ => EngineError::Database { message: e.to_string(), position: None },
        }
    }
}

type Reply<T> = SyncSender<Result<T, EngineError>>;

enum Req {
    Query { sql: String, opts: QueryOptions, reply: Reply<Cursor> },
    Fetch { id: CursorId, first: u64, rows: u32, reply: Reply<Page> },
    Close { id: CursorId },
    Schema { filter: SchemaFilter, reply: Reply<SchemaTree> },
    Explain { sql: String, analyze: bool, reply: Reply<PlanNode> },
    Exec { statements: Vec<String>, check: Option<Estimate>, reply: Reply<TxOutcome> },
}

/// A connection handle: requests go to the worker thread that owns the SQLite connection.
pub(crate) struct SqliteConn {
    tx: Sender<Req>,
    ctl: Arc<Ctl>,
    mode: Mode,
}

fn open_connection(path: &Path, mode: Mode, ctl: &Arc<Ctl>) -> rusqlite::Result<Connection> {
    let flags = match mode {
        Mode::Human => OpenFlags::SQLITE_OPEN_READ_WRITE,
        Mode::HumanReadOnly | Mode::Agent => OpenFlags::SQLITE_OPEN_READ_ONLY,
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let c = Arc::clone(ctl);
    conn.progress_handler(1_000, Some(move || c.should_stop()))?;
    if mode != Mode::Human {
        conn.execute_batch("PRAGMA query_only = 1")?;
        let on: i64 = conn.query_row("PRAGMA query_only", [], |r| r.get(0))?;
        if on != 1 {
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    if mode == Mode::Agent {
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
        conn.authorizer(Some(auth::authorize))?;
    }
    Ok(conn)
}

impl SqliteConn {
    fn open(path: &Path, mode: Mode) -> Result<Self, EngineError> {
        let ctl = Arc::new(Ctl {
            base: Instant::now(),
            deadline_ms: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            busy: AtomicBool::new(false),
        });
        let conn = open_connection(path, mode, &ctl).map_err(|e| EngineError::Connect(e.to_string()))?;
        let (tx, rx) = mpsc::channel();
        let wctl = Arc::clone(&ctl);
        std::thread::Builder::new()
            .name("quarry-sqlite".into())
            .spawn(move || Worker { conn: &conn, ctl: wctl, mode, cursors: HashMap::new(), next: 1 }.run(rx))
            .map_err(|e| EngineError::Connect(format!("cannot start the connection thread: {e}")))?;
        Ok(Self { tx, ctl, mode })
    }

    fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Req) -> Result<T, EngineError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx.send(make(tx)).map_err(|_| EngineError::Closed)?;
        rx.recv().map_err(|_| EngineError::Closed)?
    }

    fn agent_timeout(opts: &QueryOptions) -> QueryOptions {
        QueryOptions { timeout_ms: Some(opts.timeout_ms.unwrap_or(AGENT_TIMEOUT_MS_DEFAULT).clamp(1, AGENT_TIMEOUT_MS_MAX)), ..*opts }
    }
}

impl Drop for SqliteConn {
    fn drop(&mut self) {
        // stop whatever runs; the worker ends when the channel closes
        if self.ctl.busy.load(Ordering::SeqCst) {
            self.ctl.cancel.store(true, Ordering::SeqCst);
        }
    }
}

impl Browse for SqliteConn {
    fn schema(&mut self, filter: &SchemaFilter) -> Result<SchemaTree, EngineError> {
        self.call(|reply| Req::Schema { filter: filter.clone(), reply })
    }
    fn fetch(&mut self, cursor: CursorId, first_row: u64, rows: u32) -> Result<Page, EngineError> {
        self.call(|reply| Req::Fetch { id: cursor, first: first_row, rows, reply })
    }
    fn close(&mut self, cursor: CursorId) {
        let _ = self.tx.send(Req::Close { id: cursor });
    }
    fn cancel_token(&self) -> CancelToken {
        let ctl = Arc::clone(&self.ctl);
        CancelToken::new(move || {
            if ctl.busy.load(Ordering::SeqCst) {
                ctl.cancel.store(true, Ordering::SeqCst);
            }
        })
    }
}

impl HumanConnection for SqliteConn {
    fn query(&mut self, sql: &str, opts: &QueryOptions) -> Result<Cursor, EngineError> {
        self.call(|reply| Req::Query { sql: sql.to_string(), opts: *opts, reply })
    }
    fn explain(&mut self, sql: &str, analyze: bool) -> Result<PlanNode, EngineError> {
        self.call(|reply| Req::Explain { sql: sql.to_string(), analyze, reply })
    }
    fn execute_in_tx(&mut self, statements: &[String], check: Option<Estimate>) -> Result<TxOutcome, EngineError> {
        if self.mode != Mode::Human {
            return Err(EngineError::Database { message: "the connection is read-only".into(), position: None });
        }
        self.call(|reply| Req::Exec { statements: statements.to_vec(), check, reply })
    }
}

fn check_engine(stmt: &ReadStatement) -> Result<(), EngineError> {
    if stmt.engine() == EngineKind::Sqlite {
        Ok(())
    } else {
        Err(EngineError::Database { message: "the statement was classified for another engine".into(), position: None })
    }
}

impl AgentConnection for SqliteConn {
    fn query(&mut self, stmt: &ReadStatement, opts: &QueryOptions) -> Result<Cursor, EngineError> {
        check_engine(stmt)?;
        let opts = Self::agent_timeout(opts);
        self.call(|reply| Req::Query { sql: stmt.sql().to_string(), opts, reply })
    }
    fn explain(&mut self, stmt: &ReadStatement, analyze: bool) -> Result<PlanNode, EngineError> {
        check_engine(stmt)?;
        let sql = if matches!(stmt.kind(), quarry_guard::ReadKind::Explain { .. }) { crate::strip_explain(stmt.sql()) } else { stmt.sql() };
        self.call(|reply| Req::Explain { sql: sql.to_string(), analyze, reply })
    }
}

struct Worker<'c> {
    conn: &'c Connection,
    ctl: Arc<Ctl>,
    mode: Mode,
    cursors: HashMap<CursorId, SqlCursor<'c>>,
    next: u64,
}

impl<'c> Worker<'c> {
    fn run(mut self, rx: Receiver<Req>) {
        while let Ok(req) = rx.recv() {
            match req {
                Req::Query { sql, opts, reply } => {
                    let r = self.query(&sql, &opts);
                    let _ = reply.send(r);
                }
                Req::Fetch { id, first, rows, reply } => {
                    let r = self.fetch(id, first, rows);
                    let _ = reply.send(r);
                }
                Req::Close { id } => {
                    self.cursors.remove(&id);
                }
                Req::Schema { filter, reply } => {
                    let r = self.guarded(self.default_timeout(), |w| schema::load(w.conn, &filter));
                    let _ = reply.send(r);
                }
                Req::Explain { sql, analyze, reply } => {
                    let r = self.explain(&sql, analyze);
                    let _ = reply.send(r);
                }
                Req::Exec { statements, check, reply } => {
                    let r = self.exec(&statements, check);
                    let _ = reply.send(r);
                }
            }
        }
        // cursors (and their statements) go before the connection
        self.cursors.clear();
    }

    fn default_timeout(&self) -> Option<u32> {
        (self.mode == Mode::Agent).then_some(AGENT_TIMEOUT_MS_DEFAULT)
    }

    fn guarded<T>(&mut self, timeout_ms: Option<u32>, f: impl FnOnce(&mut Self) -> rusqlite::Result<T>) -> Result<T, EngineError> {
        self.ctl.begin(timeout_ms);
        let r = f(self);
        self.ctl.end();
        r.map_err(|e| self.ctl.error(e))
    }

    fn query(&mut self, sql: &str, opts: &QueryOptions) -> Result<Cursor, EngineError> {
        if self.cursors.len() >= MAX_CURSORS {
            return Err(EngineError::Database { message: format!("{MAX_CURSORS} results are open: close some"), position: None });
        }
        let first = opts.row_cap.map_or(PAGE_ROWS, |c| u32::try_from(c).unwrap_or(u32::MAX).min(PAGE_ROWS));
        let c = self.guarded(opts.timeout_ms, |w| SqlCursor::open(w.conn, sql, opts.row_cap, first, opts.timeout_ms))?;
        let id = CursorId(self.next);
        self.next = self.next.saturating_add(1);
        let cursor = Cursor { id, columns: Arc::clone(&c.columns), total: c.total() };
        self.cursors.insert(id, c);
        Ok(cursor)
    }

    fn fetch(&mut self, id: CursorId, first: u64, rows: u32) -> Result<Page, EngineError> {
        let Some(mut c) = self.cursors.remove(&id) else {
            return Err(EngineError::Closed);
        };
        let r = self.guarded(c.timeout_ms, |w| c.fetch(w.conn, first, rows));
        match r {
            Ok(p) => {
                self.cursors.insert(id, c);
                Ok(p)
            }
            // an interrupted or failed statement is gone; so is its cursor
            Err(e) => Err(e),
        }
    }

    fn explain(&mut self, sql: &str, analyze: bool) -> Result<PlanNode, EngineError> {
        let agent = self.mode == Mode::Agent;
        let timeout = self.default_timeout();
        self.guarded(timeout, |w| {
            let mut root = plan(w.conn, sql)?;
            if analyze {
                // the agent connection cannot write (and may not open a savepoint); the person's
                // EXPLAIN ANALYZE never commits what it ran
                if !agent {
                    w.conn.execute_batch("SAVEPOINT quarry_explain")?;
                }
                let started = Instant::now();
                let run = (|| -> rusqlite::Result<u64> {
                    let mut s = w.conn.prepare(sql)?;
                    let mut rows = s.raw_query();
                    let mut n = 0u64;
                    while rows.next()?.is_some() {
                        n += 1;
                    }
                    Ok(n)
                })();
                if !agent {
                    let _ = w.conn.execute_batch("ROLLBACK TO quarry_explain; RELEASE quarry_explain");
                }
                let n = run?;
                root.actual_rows = Some(n as f64);
                root.actual_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
            Ok(root)
        })
    }

    fn exec(&mut self, statements: &[String], check: Option<Estimate>) -> Result<TxOutcome, EngineError> {
        self.guarded(None, |w| {
            let conn = w.conn;
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let run = (|| -> rusqlite::Result<TxOutcome> {
                let mut out = TxOutcome::default();
                for sql in statements {
                    let before: i64 = conn.query_row("SELECT total_changes()", [], |r| r.get(0))?;
                    let mut s = conn.prepare(sql)?;
                    let mut rows = s.raw_query();
                    while rows.next()?.is_some() {}
                    drop(rows);
                    drop(s);
                    let after: i64 = conn.query_row("SELECT total_changes()", [], |r| r.get(0))?;
                    let n = u64::try_from(after.saturating_sub(before)).unwrap_or(0);
                    out.per_statement.push(n);
                    out.rows_affected = out.rows_affected.saturating_add(n);
                }
                Ok(out)
            })();
            match run {
                Ok(out) => {
                    if let Some(est) = check.filter(|e| !e.accepts(out.rows_affected)) {
                        conn.execute_batch("ROLLBACK")?;
                        return Ok(Err(EngineError::OverEstimate { rows_affected: out.rows_affected, expected: est.rows }));
                    }
                    conn.execute_batch("COMMIT")?;
                    Ok(Ok(out))
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })?
    }
}

/// `EXPLAIN QUERY PLAN` as a tree under a `QUERY PLAN` root.
fn plan(conn: &Connection, sql: &str) -> rusqlite::Result<PlanNode> {
    let mut s = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let mut rows = s.raw_query();
    let mut flat: Vec<(i64, i64, PlanNode)> = Vec::new();
    while let Some(r) = rows.next()? {
        if flat.len() >= PLAN_ROWS_MAX {
            break;
        }
        let id: i64 = r.get(0)?;
        let parent: i64 = r.get(1)?;
        let detail: String = r.get(3)?;
        let mut words = detail.split_whitespace();
        let relation = match words.next() {
            Some("SCAN" | "SEARCH") => words.next().map(str::to_string),
            _ => None,
        };
        flat.push((id, parent, PlanNode { op: detail, relation, ..PlanNode::default() }));
    }
    Ok(build_tree(flat))
}

fn build_tree(flat: Vec<(i64, i64, PlanNode)>) -> PlanNode {
    // children listed after their parent; attach in reverse so each node is complete when moved
    let mut nodes: Vec<Option<PlanNode>> = Vec::with_capacity(flat.len());
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut parents = Vec::with_capacity(flat.len());
    for (i, (id, parent, node)) in flat.into_iter().enumerate() {
        index.insert(id, i);
        parents.push(parent);
        nodes.push(Some(node));
    }
    let mut root = PlanNode { op: "QUERY PLAN".into(), ..PlanNode::default() };
    for i in (0..nodes.len()).rev() {
        let Some(node) = nodes.get_mut(i).and_then(Option::take) else { continue };
        let parent = parents.get(i).and_then(|p| index.get(p)).copied().filter(|&p| p < i);
        match parent.and_then(|p| nodes.get_mut(p)).and_then(Option::as_mut) {
            Some(p) => p.children.insert(0, node),
            None => root.children.insert(0, node),
        }
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_rows_become_a_tree() {
        let n = |op: &str| PlanNode { op: op.into(), ..PlanNode::default() };
        let t = build_tree(vec![(2, 0, n("a")), (3, 2, n("b")), (5, 2, n("c")), (7, 0, n("d"))]);
        assert_eq!(t.children.len(), 2);
        assert_eq!(t.children[0].op, "a");
        assert_eq!(t.children[0].children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(t.children[1].op, "d");
    }
}

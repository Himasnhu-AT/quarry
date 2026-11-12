//! Database engines (contract 5, see `docs/contracts.md`). Rank 1.
//!
//! An [`Engine`] opens two kinds of connection to one database, and the type system keeps them
//! apart:
//!
//! - [`HumanConnection`]: the person's own connection. Runs any SQL the person typed, and is the
//!   only thing with [`HumanConnection::execute_in_tx`] (pending edits, approved proposals).
//! - [`AgentConnection`]: the agent's separate read-only connection (SECURITY.md layer A). Its
//!   `query` and `explain` take a [`ReadStatement`], which only the guard's classifier builds, and
//!   it has no write method at all.
//!
//! Both browse the same way ([`Browse`]): schema, page fetches from server-side cursors, cancel.
//!
//! The API is blocking: every call runs inside an ashlar job, never on the UI thread. The
//! PostgreSQL backend drives tokio-postgres on its own current-thread runtime; SQLite is sync.
//! A [`CancelToken`] is cloned to another thread and cancels the statement in flight.
//!
//! The agent path reads only what the guard allowed:
//!
//! ```
//! fn read(agent: &mut dyn quarry_engine::AgentConnection, stmt: &quarry_engine::ReadStatement) {
//!     let _ = agent.query(stmt, &quarry_engine::QueryOptions::default());
//! }
//! ```
//!
//! and cannot write, by construction:
//!
//! ```compile_fail
//! fn smuggle(agent: &mut dyn quarry_engine::AgentConnection) {
//!     let _ = agent.execute_in_tx(&["DELETE FROM t".to_string()], None);
//! }
//! ```
//!
//! and takes no raw SQL:
//!
//! ```compile_fail
//! fn smuggle(agent: &mut dyn quarry_engine::AgentConnection) {
//!     let _ = agent.query("DELETE FROM t", &quarry_engine::QueryOptions::default());
//! }
//! ```

use std::fmt;
use std::sync::Arc;

use quarry_core::{Column, ConnectionSpec, CursorId, EngineKind, Estimate, Page, PlanNode, Refusal, RefusalCode, SchemaTree, Secret, Total};
pub use quarry_guard::ReadStatement;

mod cells;
pub mod pg;
pub mod sqlite;

/// Why an engine call failed. Messages never contain a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("cannot connect: {0}")]
    Connect(String),
    /// The database refused or failed; `position` is the 1-based character offset in the SQL when
    /// the server reports one (the editor's inline error banner).
    #[error("{message}")]
    Database { message: String, position: Option<u32> },
    #[error("cancelled")]
    Cancelled,
    #[error("timed out")]
    Timeout,
    /// The connection is closed or the cursor is gone.
    #[error("closed")]
    Closed,
    /// `execute_in_tx` rolled back: the affected rows differ from the estimate beyond its tolerance.
    #[error("rolled back: {rows_affected} rows affected, expected about {expected}")]
    OverEstimate { rows_affected: u64, expected: u64 },
    #[error("not built yet")]
    NotYet,
}

impl From<EngineError> for Refusal {
    fn from(e: EngineError) -> Self {
        let code = match e {
            EngineError::Timeout => RefusalCode::Timeout,
            EngineError::Closed => RefusalCode::UnknownConnection,
            EngineError::NotYet => RefusalCode::NotYet,
            _ => RefusalCode::Database,
        };
        Refusal::new(code, e.to_string())
    }
}

/// Limits for one query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// Statement timeout; `None`: the connection's default (agent: 15 s).
    pub timeout_ms: Option<u32>,
    /// Stop after this many rows (the total becomes `Known`); `None`: stream everything.
    pub row_cap: Option<u64>,
}

/// What `schema` should load.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaFilter {
    /// Only this namespace.
    pub schema: Option<String>,
    /// Only this table (with columns, indexes and keys).
    pub table: Option<String>,
    /// At most this many tables (0: no cap); the result says when it was cut.
    pub max_tables: usize,
}

/// An open server-side cursor (`DECLARE … CURSOR` / a stepped SQLite statement): its columns and
/// what is known of its row count. Pages are fetched with [`Browse::fetch`] in any order.
#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    pub id: CursorId,
    pub columns: Arc<[Column]>,
    pub total: Total,
}

/// Cancels the statement running on one connection, from any thread. Cloneable; cancelling when
/// nothing runs does nothing.
#[derive(Clone)]
pub struct CancelToken(Arc<dyn Fn() + Send + Sync>);

impl CancelToken {
    pub fn new(cancel: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(cancel))
    }
    /// A token that does nothing (stubs, tests).
    pub fn noop() -> Self {
        Self::new(|| {})
    }
    pub fn cancel(&self) {
        (self.0)()
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CancelToken")
    }
}

/// What an `execute_in_tx` committed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxOutcome {
    /// Rows affected, per statement.
    pub per_statement: Vec<u64>,
    pub rows_affected: u64,
}

/// A database engine: opens the two kinds of connection.
pub trait Engine: Send + Sync {
    fn kind(&self) -> EngineKind;

    /// The person's connection (`secret`: the password or passphrase from the keychain).
    fn connect_human(&self, spec: &ConnectionSpec, secret: Option<&Secret>) -> Result<Box<dyn HumanConnection>, EngineError>;

    /// The agent's read-only connection, with every layer-A guard on before it is returned:
    /// PostgreSQL `default_transaction_read_only = on`, each statement in `BEGIN READ ONLY …
    /// ROLLBACK`, `statement_timeout` and `lock_timeout`, the agent role when configured; SQLite
    /// `SQLITE_OPEN_READ_ONLY`, `PRAGMA query_only = 1`, the authorizer, no extensions, no ATTACH.
    /// Refused when the spec's agent access is off.
    fn connect_agent(&self, spec: &ConnectionSpec, secret: Option<&Secret>) -> Result<Box<dyn AgentConnection>, EngineError>;
}

/// What both connections do: schema, pages from cursors, cancel.
pub trait Browse: Send {
    fn schema(&mut self, filter: &SchemaFilter) -> Result<SchemaTree, EngineError>;
    /// Rows `first_row .. first_row + rows` of an open cursor (fewer at the end). Cells are capped
    /// (`limits::CELL_CAP_BYTES`, `limits::BYTES_PREVIEW`).
    fn fetch(&mut self, cursor: CursorId, first_row: u64, rows: u32) -> Result<Page, EngineError>;
    /// Close a cursor; unknown ids are ignored.
    fn close(&mut self, cursor: CursorId);
    fn cancel_token(&self) -> CancelToken;
}

/// The person's connection.
pub trait HumanConnection: Browse {
    /// Run one statement typed by the person and open a cursor on its rows (a statement without
    /// rows gives a cursor with no columns and `Total::Known(0)`).
    fn query(&mut self, sql: &str, opts: &QueryOptions) -> Result<Cursor, EngineError>;
    /// The plan; `analyze` executes the statement.
    fn explain(&mut self, sql: &str, analyze: bool) -> Result<PlanNode, EngineError>;
    /// Run `statements` in one transaction and commit, or roll back on the first error. With
    /// `check`, roll back (error [`EngineError::OverEstimate`]) when the total affected rows are
    /// outside the estimate's tolerance. Only pending edits the person committed and approved
    /// proposals reach this.
    fn execute_in_tx(&mut self, statements: &[String], check: Option<Estimate>) -> Result<TxOutcome, EngineError>;
}

/// The agent's read-only connection. No write method, no raw SQL.
pub trait AgentConnection: Browse {
    fn query(&mut self, stmt: &ReadStatement, opts: &QueryOptions) -> Result<Cursor, EngineError>;
    /// `analyze` only when the classifier allowed it (`ReadKind::Explain { analyze: true }` or a
    /// plain read the agent asked to analyze).
    fn explain(&mut self, stmt: &ReadStatement, analyze: bool) -> Result<PlanNode, EngineError>;
}

/// The statement inside an `EXPLAIN …` the classifier allowed, for `explain` (which adds its own
/// `EXPLAIN`): skips `EXPLAIN`, SQLite's `QUERY PLAN`, PostgreSQL's `(…)` options and
/// `ANALYZE` / `VERBOSE`. Anything else is returned as it is.
pub(crate) fn strip_explain(sql: &str) -> &str {
    fn word<'a>(s: &'a str, w: &str) -> Option<&'a str> {
        let s = s.trim_start();
        let head = s.get(..w.len())?;
        let rest = s.get(w.len()..)?;
        (head.eq_ignore_ascii_case(w) && rest.starts_with(|c: char| c.is_whitespace() || c == '(')).then_some(rest)
    }
    let Some(mut s) = word(sql, "explain") else { return sql };
    if let Some(r) = word(s, "query").and_then(|r| word(r, "plan")) {
        return r.trim_start();
    }
    let t = s.trim_start();
    if let Some(r) = t.strip_prefix('(') {
        match r.split_once(')') {
            Some((_, rest)) => s = rest,
            None => return sql,
        }
    }
    while let Some(r) = word(s, "analyze").or_else(|| word(s, "analyse")).or_else(|| word(s, "verbose")) {
        s = r;
    }
    s.trim_start()
}

/// The engine for `kind`.
pub fn engine(kind: EngineKind) -> Box<dyn Engine> {
    match kind {
        EngineKind::Postgres => Box::new(pg::PgEngine),
        EngineKind::Sqlite => Box::new(sqlite::SqliteEngine),
    }
}

#[cfg(test)]
mod tests {
    use super::strip_explain;

    #[test]
    fn explain_prefixes_are_stripped() {
        assert_eq!(strip_explain("EXPLAIN QUERY PLAN SELECT 1"), "SELECT 1");
        assert_eq!(strip_explain("explain select 1"), "select 1");
        assert_eq!(strip_explain("EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1"), "SELECT 1");
        assert_eq!(strip_explain("EXPLAIN ANALYZE VERBOSE SELECT 1"), "SELECT 1");
        assert_eq!(strip_explain("SELECT explained FROM t"), "SELECT explained FROM t");
        assert_eq!(strip_explain("EXPLAIN (oops SELECT 1"), "EXPLAIN (oops SELECT 1");
    }
}

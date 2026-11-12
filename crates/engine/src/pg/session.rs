//! One PostgreSQL session and its cursors, driven on the engine's runtime.
//!
//! **Cursors.** A `SELECT`-like statement (`SELECT`, `WITH`, `VALUES`, `TABLE`) becomes a
//! server-side `DECLARE … SCROLL CURSOR`, read with `MOVE ABSOLUTE` + `FETCH FORWARD`, so pages
//! come in any order without the client reading what it skips. Other statements with rows
//! (`SHOW`, `EXPLAIN`, `… RETURNING`) are read whole, up to [`MATERIALIZE_ROWS`]. Cursors live in
//! one transaction per session (the "cursor transaction"), opened with the first cursor and
//! ended when the last one closes: the person's commits, the agent's rolls back.
//!
//! **Steps.** While the cursor transaction is open, each new statement runs inside
//! `SAVEPOINT quarry_step`, so a typo or a cancelled first page does not abort the other open
//! cursors. A failed page fetch does abort the transaction; then every cursor of the session is
//! closed (their next fetch says [`EngineError::Closed`]).
//!
//! **Writes by the person** (statements without rows, `execute_in_tx`) end the cursor
//! transaction first (commit; open cursors close) and run in autocommit or their own
//! transaction, so an approved write is committed when it says so.
//!
//! **The agent** (SECURITY.md layer A): the session has `default_transaction_read_only = on`
//! (checked after connecting), `statement_timeout` 30 s, `lock_timeout` 5 s; every agent
//! statement runs inside `BEGIN READ ONLY`, which is rolled back when its last cursor closes, so
//! even a session setting changed by a statement is undone. A sentinel cursor proves the
//! transaction is still ours after each agent statement: a statement that ended it (`COMMIT`)
//! fails, the session's guards are set again, and nothing else runs outside the read-only
//! transaction.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use quarry_core::limits::PAGE_ROWS;
use quarry_core::{Column, CursorId, Estimate, Page, PlanNode, SchemaTree, Total, Value};
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::{Client, NoTls, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{explain, schema, types};
use crate::{Cursor, EngineError, QueryOptions, SchemaFilter, TxOutcome};

/// Rows kept from a statement that cannot be a server-side cursor.
pub(crate) const MATERIALIZE_ROWS: u64 = 100_000;
/// Open cursors per session.
const MAX_CURSORS: usize = 64;
const SENTINEL: &str = "quarry_tx";
/// The agent session's guards, set after connecting and again after a statement ended the
/// read-only transaction.
pub(crate) const AGENT_GUARDS: &str = "SET default_transaction_read_only = on; SET statement_timeout = 30000; \
     SET lock_timeout = 5000; SET idle_in_transaction_session_timeout = 600000";

/// Sends PostgreSQL's out-of-band cancel request for the session.
#[derive(Clone)]
pub(crate) struct Canceller {
    pub(crate) token: tokio_postgres::CancelToken,
    pub(crate) tls: Option<MakeRustlsConnect>,
}

impl Canceller {
    pub(crate) async fn cancel(&self) {
        let r = match &self.tls {
            Some(t) => self.token.cancel_query(t.clone()).await,
            None => self.token.cancel_query(NoTls).await,
        };
        if let Err(e) = r {
            log::debug!("cancel request failed: {e}");
        }
    }
}

pub(crate) fn map_err(e: tokio_postgres::Error) -> EngineError {
    if let Some(db) = e.as_db_error() {
        if *db.code() == SqlState::QUERY_CANCELED {
            return if db.message().contains("timeout") { EngineError::Timeout } else { EngineError::Cancelled };
        }
        let mut message = db.message().to_string();
        if let Some(d) = db.detail() {
            message.push('\n');
            message.push_str(d);
        }
        if let Some(h) = db.hint() {
            message.push_str("\nhint: ");
            message.push_str(h);
        }
        let position = match db.position() {
            Some(ErrorPosition::Original(p)) => Some(*p),
            _ => None,
        };
        return EngineError::Database { message, position };
    }
    if e.is_closed() { EngineError::Closed } else { EngineError::Database { message: e.to_string(), position: None } }
}

/// Run `fut` within `ms` (if set); past it, ask the server to cancel and report a timeout.
async fn timed<T>(canceller: &Canceller, ms: Option<u32>, fut: impl Future<Output = Result<T, tokio_postgres::Error>>) -> Result<T, EngineError> {
    match ms {
        None => fut.await.map_err(map_err),
        Some(ms) => match tokio::time::timeout(Duration::from_millis(u64::from(ms)), fut).await {
            Ok(r) => r.map_err(map_err),
            Err(_) => {
                canceller.cancel().await;
                Err(EngineError::Timeout)
            }
        },
    }
}

/// Whether a statement can be the query of a `DECLARE CURSOR` (skips comments and parentheses).
pub(crate) fn select_like(sql: &str) -> bool {
    let mut s = sql;
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r);
        } else {
            break;
        }
    }
    let word: String = s.chars().take_while(char::is_ascii_alphabetic).collect::<String>().to_ascii_lowercase();
    matches!(word.as_str(), "select" | "with" | "values" | "table")
}

enum Source {
    Declared { name: String, pos: u64, fetch: Option<(u64, Statement)> },
    Materialized(Vec<Vec<Value>>),
}

struct PgCursor {
    source: Source,
    cap: Option<u64>,
    /// Rows known to exist.
    seen: u64,
    done: bool,
    timeout_ms: Option<u32>,
}

impl PgCursor {
    fn total(&self) -> Total {
        if self.done { Total::Known(self.seen) } else { Total::AtLeast(self.seen) }
    }
}

pub(crate) struct Session {
    pub(crate) client: Client,
    pub(crate) canceller: Canceller,
    agent: bool,
    in_tx: bool,
    /// A cleanup failed: the session state is unknown, every call fails `Closed`.
    broken: bool,
    cursors: HashMap<CursorId, PgCursor>,
    next: u64,
}

fn rows_of(rows: &[tokio_postgres::Row]) -> Vec<Vec<Value>> {
    rows.iter().map(|r| (0..r.len()).map(|i| types::cell(r, i)).collect()).collect()
}

fn columns_of(stmt: &Statement) -> Arc<[Column]> {
    stmt.columns().iter().map(|c| Column::new(c.name(), c.type_().name())).collect::<Vec<_>>().into()
}

impl Session {
    pub(crate) fn new(client: Client, canceller: Canceller, agent: bool) -> Self {
        Self { client, canceller, agent, in_tx: false, broken: false, cursors: HashMap::new(), next: 1 }
    }

    /// Cursors that live in the cursor transaction go with it; results read whole stay.
    fn drop_declared(&mut self) {
        self.cursors.retain(|_, c| matches!(c.source, Source::Materialized(_)));
    }

    fn has_declared(&self) -> bool {
        self.cursors.values().any(|c| matches!(c.source, Source::Declared { .. }))
    }

    fn check(&self) -> Result<(), EngineError> {
        if self.broken || self.client.is_closed() { Err(EngineError::Closed) } else { Ok(()) }
    }

    async fn sql(&self, sql: &str) -> Result<(), EngineError> {
        self.client.batch_execute(sql).await.map_err(map_err)
    }

    async fn begin(&mut self) -> Result<(), EngineError> {
        if self.in_tx {
            return Ok(());
        }
        if self.agent {
            self.sql(&format!("BEGIN READ ONLY; DECLARE {SENTINEL} NO SCROLL CURSOR FOR SELECT 1")).await?;
        } else {
            self.sql("BEGIN").await?;
        }
        self.in_tx = true;
        Ok(())
    }

    /// End the cursor transaction (agent: roll back; person: commit). Its cursors close; results
    /// read whole stay.
    pub(crate) async fn end_tx(&mut self) {
        self.drop_declared();
        if !self.in_tx {
            return;
        }
        self.in_tx = false;
        let end = if self.agent { "ROLLBACK" } else { "COMMIT" };
        if let Err(e) = self.client.batch_execute(end).await {
            log::warn!("{end} of the cursor transaction failed: {e}");
            if self.client.batch_execute("ROLLBACK").await.is_err() {
                self.broken = true;
            }
        }
    }

    /// After an error that aborts the transaction: roll it back and forget its cursors.
    async fn abandon(&mut self) {
        self.drop_declared();
        if self.in_tx {
            self.in_tx = false;
            if self.client.batch_execute("ROLLBACK").await.is_err() {
                self.broken = true;
            }
        }
    }

    async fn savepoint(&self) -> Result<(), EngineError> {
        self.sql("SAVEPOINT quarry_step").await
    }

    /// Finish a step: keep its work, or undo it and keep the transaction usable.
    async fn end_step(&mut self, ok: bool) {
        let sql = if ok { "RELEASE SAVEPOINT quarry_step" } else { "ROLLBACK TO SAVEPOINT quarry_step; RELEASE SAVEPOINT quarry_step" };
        if self.client.batch_execute(sql).await.is_err() {
            self.abandon().await;
        }
    }

    /// Agent: the read-only transaction is still the one we began.
    async fn still_ours(&mut self) -> Result<(), EngineError> {
        let ok = match self.client.query_one("SELECT count(*) FROM pg_cursors WHERE name = $1", &[&SENTINEL]).await {
            Ok(r) => r.try_get::<_, i64>(0).unwrap_or(0) == 1,
            Err(_) => false,
        };
        if ok {
            return Ok(());
        }
        // the statement ended our transaction: reset and set the guards again
        self.drop_declared();
        self.in_tx = false;
        let _ = self.client.batch_execute("ROLLBACK").await;
        if self.client.batch_execute(AGENT_GUARDS).await.is_err() {
            self.broken = true;
        }
        Err(EngineError::Database { message: "the statement ended the read-only transaction".into(), position: None })
    }

    fn register(&mut self, c: PgCursor, columns: Arc<[Column]>) -> Cursor {
        let id = CursorId(self.next);
        self.next = self.next.saturating_add(1);
        let total = c.total();
        self.cursors.insert(id, c);
        Cursor { id, columns, total }
    }

    fn first_page(opts: &QueryOptions) -> u64 {
        opts.row_cap.map_or(u64::from(PAGE_ROWS), |c| c.min(u64::from(PAGE_ROWS)))
    }

    async fn materialize(&self, stmt: &Statement, cap: Option<u64>, timeout: Option<u32>) -> Result<PgCursor, EngineError> {
        let limit = cap.unwrap_or(MATERIALIZE_ROWS).min(MATERIALIZE_ROWS);
        let read = async {
            let stream = self.client.query_raw(stmt, std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>()).await?;
            let mut stream = std::pin::pin!(stream);
            let mut rows = Vec::new();
            let mut more = false;
            while let Some(r) = stream.next().await {
                let r = r?;
                if rows.len() as u64 >= limit {
                    more = true;
                    break;
                }
                rows.push((0..r.len()).map(|i| types::cell(&r, i)).collect::<Vec<_>>());
            }
            Ok::<_, tokio_postgres::Error>((rows, more))
        };
        let (rows, more) = timed(&self.canceller, timeout, read).await?;
        let n = rows.len() as u64;
        // more rows than kept and no cap asked for: say "at least"
        let done = !more || cap.is_some();
        Ok(PgCursor { source: Source::Materialized(rows), cap, seen: n, done, timeout_ms: timeout })
    }

    async fn declare(&mut self, sql: &str, opts: &QueryOptions, timeout: Option<u32>) -> Result<PgCursor, EngineError> {
        let name = format!("quarry_c{}", self.next);
        // `sql` was parsed as exactly one statement by `prepare` (extended protocol), and so is this
        timed(&self.canceller, timeout, self.client.execute(&format!("DECLARE {name} SCROLL CURSOR FOR {sql}"), &[])).await?;
        let mut c = PgCursor { source: Source::Declared { name, pos: 0, fetch: None }, cap: opts.row_cap, seen: 0, done: false, timeout_ms: timeout };
        self.fetch_cursor(&mut c, 0, Self::first_page(opts)).await?;
        Ok(c)
    }

    /// Rows `first .. first + n` of a declared cursor (within its cap).
    async fn fetch_cursor(&self, c: &mut PgCursor, first: u64, n: u64) -> Result<Vec<Vec<Value>>, EngineError> {
        let want = c.cap.map_or(n, |cap| n.min(cap.saturating_sub(first)));
        let Source::Declared { name, pos, fetch } = &mut c.source else {
            return Ok(Vec::new());
        };
        if want == 0 {
            if c.cap.is_some_and(|cap| first >= cap) {
                c.done = true;
            }
            return Ok(Vec::new());
        }
        if *pos != first {
            timed(&self.canceller, c.timeout_ms, self.client.batch_execute(&format!("MOVE ABSOLUTE {first} IN {name}"))).await?;
        }
        let stmt = match fetch {
            Some((k, s)) if *k == want => s.clone(),
            _ => {
                let s = self.client.prepare(&format!("FETCH FORWARD {want} FROM {name}")).await.map_err(map_err)?;
                *fetch = Some((want, s.clone()));
                s
            }
        };
        let rows = timed(&self.canceller, c.timeout_ms, self.client.query(&stmt, &[])).await?;
        let got = rows.len() as u64;
        *pos = first.saturating_add(got);
        let end = first.saturating_add(got);
        c.seen = c.seen.max(end);
        if got < want || c.cap.is_some_and(|cap| end >= cap) {
            c.done = true;
        }
        Ok(rows_of(&rows))
    }

    /// Run one statement and open a cursor on its rows.
    pub(crate) async fn query(&mut self, sql: &str, opts: QueryOptions) -> Result<Cursor, EngineError> {
        self.check()?;
        if self.cursors.len() >= MAX_CURSORS {
            return Err(EngineError::Database { message: format!("{MAX_CURSORS} results are open: close some"), position: None });
        }
        let timeout = opts.timeout_ms;
        let in_tx = self.agent || select_like(sql) || self.in_tx;
        if in_tx {
            self.begin().await?;
            self.savepoint().await?;
        }
        let r = self.query_step(sql, &opts, timeout, in_tx).await;
        if in_tx && self.in_tx {
            let ok = r.is_ok();
            if self.agent && ok {
                self.still_ours().await?;
            }
            match &r {
                Ok(Step::Done(_)) | Err(_) => self.end_step(ok).await,
                Ok(Step::NoRowsHuman(_)) => {}
            }
        }
        let r = match r {
            Ok(Step::NoRowsHuman(stmt)) => {
                // a person's write: end the cursor transaction (commit), then run it on its own
                if self.in_tx {
                    self.end_step(true).await;
                    self.end_tx().await;
                }
                timed(&self.canceller, timeout, self.client.execute(&stmt, &[])).await?;
                Ok(self
                    .register(PgCursor { source: Source::Materialized(Vec::new()), cap: None, seen: 0, done: true, timeout_ms: None }, Arc::from([])))
            }
            Ok(Step::Done(c)) => Ok(c),
            Err(e) => Err(e),
        };
        if self.in_tx && !self.has_declared() {
            self.end_tx().await;
        }
        r
    }

    async fn query_step(&mut self, sql: &str, opts: &QueryOptions, timeout: Option<u32>, in_tx: bool) -> Result<Step, EngineError> {
        let stmt = timed(&self.canceller, timeout, self.client.prepare(sql)).await?;
        let columns = columns_of(&stmt);
        if columns.is_empty() {
            if !self.agent {
                return Ok(Step::NoRowsHuman(stmt));
            }
            timed(&self.canceller, timeout, self.client.execute(&stmt, &[])).await?;
            let c = PgCursor { source: Source::Materialized(Vec::new()), cap: None, seen: 0, done: true, timeout_ms: None };
            return Ok(Step::Done(self.register(c, columns)));
        }
        let c = if in_tx && select_like(sql) {
            match self.declare(sql, opts, timeout).await {
                Ok(c) => c,
                // not every SELECT can be a cursor (data-modifying WITH): read it whole instead
                Err(EngineError::Database { .. }) => {
                    self.sql("ROLLBACK TO SAVEPOINT quarry_step").await?;
                    self.materialize(&stmt, opts.row_cap, timeout).await?
                }
                Err(e) => return Err(e),
            }
        } else {
            self.materialize(&stmt, opts.row_cap, timeout).await?
        };
        Ok(Step::Done(self.register(c, columns)))
    }

    pub(crate) async fn fetch(&mut self, id: CursorId, first: u64, rows: u32) -> Result<Page, EngineError> {
        self.check()?;
        let Some(mut c) = self.cursors.remove(&id) else { return Err(EngineError::Closed) };
        let n = u64::from(rows);
        if let Source::Materialized(all) = &c.source {
            let from = usize::try_from(first).unwrap_or(usize::MAX).min(all.len());
            let to = usize::try_from(first.saturating_add(n)).unwrap_or(usize::MAX).min(all.len());
            let page = Page { first_row: first, rows: all.get(from..to).map(<[_]>::to_vec).unwrap_or_default() };
            self.cursors.insert(id, c);
            return Ok(page);
        }
        if c.done && first >= c.seen {
            self.cursors.insert(id, c);
            return Ok(Page { first_row: first, rows: Vec::new() });
        }
        match self.fetch_cursor(&mut c, first, n).await {
            Ok(rows) => {
                self.cursors.insert(id, c);
                Ok(Page { first_row: first, rows })
            }
            Err(e) => {
                // the transaction is aborted: every cursor of the session goes
                self.abandon().await;
                Err(e)
            }
        }
    }

    pub(crate) async fn close(&mut self, id: CursorId) {
        if let Some(PgCursor { source: Source::Declared { name, .. }, .. }) = self.cursors.remove(&id)
            && self.in_tx
            && self.has_declared()
            && self.client.batch_execute(&format!("CLOSE {name}")).await.is_err()
        {
            self.abandon().await;
        }
        if !self.has_declared() {
            self.end_tx().await;
        }
    }

    pub(crate) async fn schema(&mut self, filter: &SchemaFilter, timeout: Option<u32>) -> Result<SchemaTree, EngineError> {
        self.check()?;
        if !self.in_tx {
            return timed(&self.canceller, timeout, schema::load(&self.client, filter)).await;
        }
        self.savepoint().await?;
        let r = timed(&self.canceller, timeout, schema::load(&self.client, filter)).await;
        self.end_step(r.is_ok()).await;
        r
    }

    /// `EXPLAIN (FORMAT JSON)`, always rolled back (an analyzed write never commits).
    pub(crate) async fn explain(&mut self, sql: &str, analyze: bool, timeout: Option<u32>) -> Result<PlanNode, EngineError> {
        self.check()?;
        let text = format!("EXPLAIN ({}FORMAT JSON) {sql}", if analyze { "ANALYZE, " } else { "" });
        let nested = self.in_tx;
        if nested {
            self.savepoint().await?;
        } else {
            self.sql(if self.agent { "BEGIN READ ONLY" } else { "BEGIN" }).await?;
        }
        let r = timed(&self.canceller, timeout, self.client.query(&text, &[])).await;
        let undo = if nested { "ROLLBACK TO SAVEPOINT quarry_step; RELEASE SAVEPOINT quarry_step" } else { "ROLLBACK" };
        if self.client.batch_execute(undo).await.is_err() {
            self.abandon().await;
        }
        let rows = r?;
        let json = rows.first().and_then(|r| r.try_get::<_, types::Raw<'_>>(0).ok()).map(|raw| String::from_utf8_lossy(raw.0).into_owned());
        json.as_deref().and_then(explain::parse).ok_or_else(|| EngineError::Database { message: "unreadable EXPLAIN output".into(), position: None })
    }

    /// The person's transaction: commit, or roll back on the first error or an over-estimate.
    pub(crate) async fn execute_in_tx(&mut self, statements: &[String], check: Option<Estimate>) -> Result<TxOutcome, EngineError> {
        self.check()?;
        self.end_tx().await;
        self.sql("BEGIN").await?;
        let mut out = TxOutcome::default();
        for s in statements {
            match self.client.execute(s.as_str(), &[]).await {
                Ok(n) => {
                    out.per_statement.push(n);
                    out.rows_affected = out.rows_affected.saturating_add(n);
                }
                Err(e) => {
                    if self.client.batch_execute("ROLLBACK").await.is_err() {
                        self.broken = true;
                    }
                    return Err(map_err(e));
                }
            }
        }
        if let Some(est) = check.filter(|e| !e.accepts(out.rows_affected)) {
            self.sql("ROLLBACK").await?;
            return Err(EngineError::OverEstimate { rows_affected: out.rows_affected, expected: est.rows });
        }
        self.sql("COMMIT").await?;
        Ok(out)
    }
}

enum Step {
    Done(Cursor),
    /// A person's statement without rows, prepared, waiting to run outside the cursor transaction.
    NoRowsHuman(Statement),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_like_statements() {
        for s in [
            "SELECT 1",
            "  select 1",
            "(SELECT 1) UNION (SELECT 2)",
            "WITH a AS (SELECT 1) SELECT * FROM a",
            "VALUES (1)",
            "TABLE t",
            "-- hi\nSELECT 1",
            "/* x */ SELECT 1",
        ] {
            assert!(select_like(s), "{s}");
        }
        for s in ["SHOW search_path", "EXPLAIN SELECT 1", "INSERT INTO t VALUES (1) RETURNING *", "selectx", "", "-- only a comment"] {
            assert!(!select_like(s), "{s}");
        }
    }
}

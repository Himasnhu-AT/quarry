//! The agent's read path, shared by `db.query`, `db.sample`, `viz.*` and the proposal estimate:
//! connection lookup and access check, `quarry_guard::classify` → [`ReadStatement`] →
//! `cap_rows` → the agent's read-only connection → `redact` (SECURITY.md layers A, B, D).
//!
//! It runs synchronously in the command body (ashlar 0.2 commands cannot spawn jobs): bounded by
//! the statement timeout and the row cap. In the window that blocks the UI thread for the query's
//! duration; over `quarry-cli mcp` it blocks only the MCP server.

use std::sync::Arc;
use std::time::Instant;

use quarry_core::limits::{AGENT_RESULT_BYTES_MAX, PAGE_ROWS};
use quarry_core::{Column, Page, Refusal, RefusalCode, Value};
use quarry_engine::QueryOptions;
use quarry_guard::{ReadKind, ReadStatement, Verdict};

use crate::services::{ConnEntry, services};

/// Rows an agent read returned, redacted.
#[derive(Clone, Debug, PartialEq)]
pub struct Rows {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// The row cap or the result size cap cut the rows short (or the result has exactly the cap).
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// The connection `id`, if it exists and the agent may use it.
pub fn entry(id: &str) -> Result<Arc<ConnEntry>, Refusal> {
    let e = services()
        .connections()
        .get(id)
        .ok_or_else(|| Refusal::new(RefusalCode::UnknownConnection, format!("no connection `{id}`; db.connections lists them")))?;
    if !e.spec().agent_access.allows_agent() {
        return Err(Refusal::new(RefusalCode::AgentAccessOff, format!("agent access is off for `{id}`")));
    }
    Ok(e)
}

/// Classify `sql` as an agent read on `e`: a [`ReadStatement`], or why not. A write is refused
/// `not_read_only` (pointing at `db.propose_write`); so is `EXPLAIN ANALYZE`, which executes the
/// statement (the owner has not decided to give agents that; PLAN §11).
pub fn classify_read(e: &ConnEntry, sql: &str) -> Result<ReadStatement, Refusal> {
    match quarry_guard::classify(sql, e.spec().engine(), &e.policy()) {
        Verdict::Read(r) if matches!(r.kind(), ReadKind::Explain { analyze: true }) => {
            Err(Refusal::new(RefusalCode::NotReadOnly, "EXPLAIN ANALYZE executes the statement; agents get plain EXPLAIN (db.explain)"))
        }
        Verdict::Read(r) => Ok(r),
        Verdict::Write(w) => Err(Refusal::new(
            RefusalCode::NotReadOnly,
            format!("{} can change data; db.propose_write asks the person to approve it", w.kind.as_str()),
        )),
        Verdict::Refused(r) => Err(r),
    }
}

/// Run a classified read on the agent connection: at most `limit` rows, `timeout_ms`, sensitive
/// columns redacted, the result kept under the size cap.
pub fn run(e: &ConnEntry, stmt: &ReadStatement, limit: u32, timeout_ms: u32) -> Result<Rows, Refusal> {
    let started = Instant::now();
    let (mut columns, rows) = fetch(e, stmt, limit, timeout_ms)?;
    let mut page = Page { first_row: 0, rows };
    quarry_guard::redact(&mut columns, &mut page, &e.policy());
    let mut truncated = page.rows.len() as u64 >= u64::from(limit);
    truncated |= fit(&mut page.rows);
    Ok(Rows { columns, rows: page.rows, truncated, elapsed_ms: started.elapsed().as_millis() as u64 })
}

/// [`run`] without redaction, for the window only (the undo snapshot of an approved write); its
/// rows never reach an agent result.
pub(crate) fn fetch(e: &ConnEntry, stmt: &ReadStatement, limit: u32, timeout_ms: u32) -> Result<(Vec<Column>, Vec<Vec<Value>>), Refusal> {
    let capped = quarry_guard::cap_rows(stmt, limit);
    let limit = u64::from(limit.max(1));
    e.with_agent(|c| {
        let cursor = c.query(&capped, &QueryOptions { timeout_ms: Some(timeout_ms), row_cap: Some(limit) })?;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let got = (|| {
            while (rows.len() as u64) < limit {
                let want = u32::try_from((limit - rows.len() as u64).min(u64::from(PAGE_ROWS))).unwrap_or(PAGE_ROWS);
                let page = c.fetch(cursor.id, rows.len() as u64, want)?;
                let n = page.rows.len();
                rows.extend(page.rows);
                if n < want as usize {
                    break;
                }
            }
            Ok(())
        })();
        c.close(cursor.id);
        got.map(|()| (cursor.columns.to_vec(), rows))
    })
}

/// Drop rows from the end until the agent JSON fits [`AGENT_RESULT_BYTES_MAX`]; whether any went.
fn fit(rows: &mut Vec<Vec<Value>>) -> bool {
    let budget = AGENT_RESULT_BYTES_MAX.saturating_sub(16 * 1024);
    let mut used = 0usize;
    for (i, r) in rows.iter().enumerate() {
        let n: usize = r.iter().map(|v| v.to_agent_json().to_string().len().saturating_add(1)).sum();
        used = used.saturating_add(n.saturating_add(2));
        if used > budget {
            rows.truncate(i);
            return true;
        }
    }
    false
}

/// `"name"` with quotes doubled: an identifier both engines read literally.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A table name for SQL: `"schema"."name"` or `"name"`.
pub fn quote_table(t: &quarry_core::TableRef) -> String {
    match &t.schema {
        Some(s) => format!("{}.{}", quote_ident(s), quote_ident(&t.name)),
        None => quote_ident(&t.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_table(&quarry_core::TableRef::new(Some("public"), "orders")), "\"public\".\"orders\"");
    }

    #[test]
    fn huge_results_are_cut_to_the_cap() {
        let big: Vec<Vec<Value>> = (0..100).map(|_| vec![Value::text("x".repeat(64 * 1024))]).collect();
        let mut rows = big.clone();
        assert!(fit(&mut rows));
        assert!(rows.len() < big.len() && !rows.is_empty());
        let mut small = vec![vec![Value::Int(1)]];
        assert!(!fit(&mut small));
    }
}

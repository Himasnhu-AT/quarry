//! The SQL classifier and agent policy (SECURITY.md layers B and D). Rank 0: no I/O.
//!
//! **Contract (frozen in M0, owned by the `guard` stream):**
//!
//! - [`classify`] turns agent SQL into a [`Verdict`]: [`Verdict::Read`] carries a
//!   [`ReadStatement`], the only value the engine's agent connection accepts, so SQL cannot reach
//!   the agent path without passing here. [`Verdict::Write`] is what `db.propose_write` stores.
//!   Anything else is [`Verdict::Refused`].
//! - [`check_filter`] checks a `view.filter` WHERE clause the same way.
//! - [`redact`] withholds sensitive columns from agent results.
//!
//! M0 state: the parse, single-statement and default-deny steps are real; every statement that
//! parses is refused `not_yet` until the guard stream lands the allow-list. Default deny: a stub
//! never lets anything through.

use std::sync::Arc;

use quarry_core::{Column, EngineKind, Page, Refusal, RefusalCode, TableRef, Value};
use sqlparser::dialect::{Dialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

/// Column name parts that are always sensitive (case-insensitive substrings).
pub const BUILTIN_SENSITIVE: &[&str] = &["password", "passwd", "secret", "token", "api_key", "apikey", "private_key", "ssn"];

/// What the human configured for agents on one connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// User-defined functions the human marked safe (otherwise unknown functions are denied).
    pub safe_functions: Vec<String>,
    /// Column name patterns redacted on top of [`BUILTIN_SENSITIVE`].
    pub sensitive_patterns: Vec<String>,
}

/// The kind of a read the classifier allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    /// `SELECT`, `WITH … SELECT`, `VALUES`.
    Select,
    /// `EXPLAIN` of an allowed statement; `analyze` only when the inner statement is allowed.
    Explain { analyze: bool },
    /// `SHOW` (PostgreSQL).
    Show,
    /// A read-only pragma (SQLite).
    Pragma,
}

/// One statement the classifier allowed on the agent's read-only connection. Only [`classify`]
/// builds one (and, in engine tests, [`ReadStatement::unchecked`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadStatement {
    sql: Arc<str>,
    engine: EngineKind,
    kind: ReadKind,
}

impl ReadStatement {
    pub fn sql(&self) -> &str {
        &self.sql
    }
    pub fn engine(&self) -> EngineKind {
        self.engine
    }
    pub fn kind(&self) -> ReadKind {
        self.kind
    }

    /// Skip the classifier: for engine tests that prove layer A holds on its own.
    #[cfg(feature = "unchecked")]
    pub fn unchecked(sql: &str, engine: EngineKind) -> Self {
        Self { sql: sql.into(), engine, kind: ReadKind::Select }
    }
}

/// A statement that would change something, for `db.propose_write`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteStatement {
    pub sql: Arc<str>,
    pub engine: EngineKind,
    /// Tables it writes, for the approval card.
    pub tables: Vec<TableRef>,
}

/// The classifier's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Read(ReadStatement),
    Write(WriteStatement),
    Refused(Refusal),
}

fn dialect(engine: EngineKind) -> Box<dyn Dialect> {
    match engine {
        EngineKind::Postgres => Box::new(PostgreSqlDialect {}),
        EngineKind::Sqlite => Box::new(SQLiteDialect {}),
    }
}

/// Classify one agent statement (SECURITY.md layer B).
pub fn classify(sql: &str, engine: EngineKind, _policy: &Policy) -> Verdict {
    if sql.len() > quarry_core::limits::AGENT_SQL_BYTES_MAX {
        return Verdict::Refused(Refusal::new(RefusalCode::TooLarge, "the statement is too long"));
    }
    let statements = match Parser::parse_sql(&*dialect(engine), sql) {
        Ok(s) => s,
        Err(e) => return Verdict::Refused(Refusal::new(RefusalCode::ParseError, e.to_string())),
    };
    match statements.len() {
        0 => Verdict::Refused(Refusal::new(RefusalCode::ParseError, "no statement")),
        1 => Verdict::Refused(Refusal::new(RefusalCode::NotYet, "the classifier allow-list is not built yet")),
        n => Verdict::Refused(Refusal::new(RefusalCode::MultipleStatements, format!("{n} statements: send exactly one"))),
    }
}

/// Check a `view.filter` WHERE clause: the clause as it will be applied, or why not.
pub fn check_filter(where_clause: &str, engine: EngineKind, policy: &Policy) -> Result<String, Refusal> {
    match classify(&format!("SELECT 1 WHERE {where_clause}"), engine, policy) {
        Verdict::Read(_) => Ok(where_clause.trim().to_string()),
        Verdict::Write(_) => Err(Refusal::new(RefusalCode::NotReadOnly, "the filter changes data")),
        Verdict::Refused(r) => Err(r),
    }
}

/// Whether a column name is sensitive under `policy`.
pub fn is_sensitive(column: &str, policy: &Policy) -> bool {
    let lower = column.to_lowercase();
    BUILTIN_SENSITIVE
        .iter()
        .copied()
        .chain(policy.sensitive_patterns.iter().map(String::as_str))
        .any(|p| !p.is_empty() && lower.contains(&p.to_lowercase()))
}

/// Mark sensitive columns and replace their cells with [`Value::Redacted`] (length kept), for an
/// agent result. Columns already marked `sensitive` (by the human) are redacted too.
pub fn redact(columns: &mut [Column], page: &mut Page, policy: &Policy) {
    for c in columns.iter_mut() {
        c.sensitive |= is_sensitive(&c.name, policy);
    }
    for row in &mut page.rows {
        for (cell, col) in row.iter_mut().zip(columns.iter()) {
            if col.sensitive && !cell.is_null() {
                *cell = Value::Redacted { len: byte_len(cell) };
            }
        }
    }
}

fn byte_len(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(i) => i.to_string().len() as u64,
        Value::Float(f) => f.to_string().len() as u64,
        Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => s.len() as u64,
        Value::Bytes { len, .. } | Value::Clipped { len, .. } | Value::Redacted { len } => *len,
    }
}

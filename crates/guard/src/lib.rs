//! The SQL classifier and agent policy (SECURITY.md layers B and D). Rank 0: no I/O.
//!
//! **Contract (frozen in M0, owned by the `guard` stream):**
//!
//! - [`classify`] turns agent SQL into a [`Verdict`]: [`Verdict::Read`] carries a
//!   [`ReadStatement`], the only value the engine's agent connection accepts, so SQL cannot reach
//!   the agent path without passing here. [`Verdict::Write`] is a statement that changes (or
//!   might change) something: `db.query` refuses it `not_read_only`. Anything else is
//!   [`Verdict::Refused`].
//! - [`propose_write`] is the `db.propose_write` path: one statement becomes a [`WriteStatement`]
//!   draft (kind, tables, the estimate hook) for an approval card, unless it is refused on every
//!   path (transaction control, session state, `load_extension`, …).
//! - [`check_filter`] checks a `view.filter` WHERE clause the same way.
//! - [`cap_rows`] puts the agent row cap into a read; [`redact`] withholds sensitive columns.
//!
//! Fail closed: a statement is a read only when every part of it is provably a read. The SQL a
//! [`ReadStatement`] carries is the parser's canonical rendering of the statement it classified
//! (comments dropped, one statement), never the agent's text, so what runs is what was checked.

mod functions;
mod statement;
mod walk;

use std::sync::Arc;

use quarry_core::limits::{AGENT_ROW_CAP_MAX, AGENT_SQL_BYTES_MAX};
use quarry_core::{Column, EngineKind, Page, Refusal, RefusalCode, TableRef, Value};
use sqlparser::ast::{Expr, LimitClause, Statement};
use sqlparser::dialect::{Dialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

use statement::Root;
pub use walk::MAX_DEPTH;

/// Column name parts that are always sensitive (case-insensitive substrings).
pub const BUILTIN_SENSITIVE: &[&str] = &["password", "passwd", "secret", "token", "api_key", "apikey", "private_key", "ssn"];

/// The parser's recursion limit (nested parentheses, subqueries); deeper input is a parse error.
pub const PARSER_RECURSION_LIMIT: usize = 64;

/// Most operator tokens in one statement. Operator chains (`a + b + c …`) parse into a tree as
/// deep as the chain is long; this keeps that tree shallow enough to walk and drop safely.
pub const MAX_OPERATORS: usize = 1_000;

/// What the human configured for agents on one connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// User-defined functions the human marked safe (otherwise unknown functions are denied).
    /// `name` or `schema.name`, case-insensitive. Never unlocks a deny-listed function.
    pub safe_functions: Vec<String>,
    /// Column name patterns redacted on top of [`BUILTIN_SENSITIVE`].
    pub sensitive_patterns: Vec<String>,
}

/// The kind of a read the classifier allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    /// `SELECT`, `WITH … SELECT`, `VALUES`, `TABLE t`, set operations.
    Select,
    /// `EXPLAIN` of an allowed query; `analyze` (which runs it) is allowed because the query is.
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
    /// The canonical SQL to run (not the agent's text: comments dropped, exactly one statement).
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

/// What a write is, for the approval card and the "approve similar" rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WriteKind {
    Insert,
    Update,
    Delete,
    Merge,
    Truncate,
    /// CREATE / ALTER / DROP / COMMENT / RENAME, and `SELECT … INTO` (creates a table).
    Ddl,
    /// GRANT / REVOKE / roles and users.
    Dcl,
    /// COPY (either direction, incl. `TO PROGRAM`), UNLOAD, LOAD DATA.
    Copy,
    /// CALL of a procedure.
    Call,
    /// VACUUM / ANALYZE / REFRESH MATERIALIZED VIEW and the like.
    Maintenance,
    /// LOCK, or a query with FOR UPDATE / FOR SHARE.
    Lock,
    /// A SQLite pragma that sets something.
    Pragma,
    /// A query that only reads but calls a function outside the read-only allow-list, or uses a
    /// sensitive column (a proposal of it is accepted; `db.query` refuses it).
    Select,
    /// Anything else that is not provably a read.
    Other,
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteKind::Insert => "INSERT",
            WriteKind::Update => "UPDATE",
            WriteKind::Delete => "DELETE",
            WriteKind::Merge => "MERGE",
            WriteKind::Truncate => "TRUNCATE",
            WriteKind::Ddl => "DDL",
            WriteKind::Dcl => "GRANT / REVOKE",
            WriteKind::Copy => "COPY",
            WriteKind::Call => "CALL",
            WriteKind::Maintenance => "maintenance",
            WriteKind::Lock => "a lock",
            WriteKind::Pragma => "a pragma that sets something",
            WriteKind::Select => "a query",
            WriteKind::Other => "a statement that is not a plain read",
        }
    }
}

/// A statement that would change something, for `db.propose_write`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteStatement {
    /// Exactly as the agent sent it (the card shows it unformatted). It parsed as one statement.
    pub sql: Arc<str>,
    pub engine: EngineKind,
    /// Tables it writes, for the approval card (best effort).
    pub tables: Vec<TableRef>,
    /// What it is (additive, guard stream).
    pub kind: WriteKind,
    /// An UPDATE / DELETE without WHERE, or a TRUNCATE: every row (additive).
    pub unbounded: bool,
    /// The estimate hook (additive): for a single-table UPDATE / DELETE, `SELECT count(*)` with
    /// the same WHERE, classified as a read, for the engine to run on the agent connection.
    /// `None` when there is no such count (the engine falls back to the planner's EXPLAIN).
    pub count: Option<ReadStatement>,
    /// For a single-table UPDATE, the columns its SET assigns (additive, finish stream): an undo
    /// plan refuses one that changes a primary key. `None` for other statements and for an UPDATE
    /// that may touch more than its SET says (`UPDATE OR REPLACE`, `UPDATE … FROM`).
    pub assigned: Option<Vec<String>>,
}

impl WriteStatement {
    /// Whether "Approve similar for this session" may be offered (SECURITY.md layer C): never for
    /// DDL, unbounded changes or anything but row DML. The app adds the prod-connection rule.
    pub fn approve_similar_allowed(&self) -> bool {
        matches!(self.kind, WriteKind::Insert | WriteKind::Update | WriteKind::Delete | WriteKind::Merge) && !self.unbounded
    }
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

fn refused(code: RefusalCode, msg: impl Into<String>) -> Refusal {
    Refusal::new(code, msg)
}

/// Tokenize and bound the input before building a tree from it.
fn tokens(sql: &str, d: &dyn Dialect) -> Result<Vec<Token>, Refusal> {
    if sql.len() > AGENT_SQL_BYTES_MAX {
        return Err(refused(RefusalCode::TooLarge, format!("the statement is longer than {AGENT_SQL_BYTES_MAX} bytes")));
    }
    let toks = sqlparser::tokenizer::Tokenizer::new(d, sql).tokenize().map_err(|e| refused(RefusalCode::ParseError, e.to_string()))?;
    let ops = toks.iter().filter(|t| is_operator(t)).count();
    if ops > MAX_OPERATORS {
        return Err(refused(RefusalCode::TooLarge, format!("more than {MAX_OPERATORS} operators in one statement")));
    }
    Ok(toks)
}

/// Tokens that can join two expressions (and so deepen the tree). Over-counting is harmless.
fn is_operator(t: &Token) -> bool {
    match t {
        Token::Word(w) => {
            use sqlparser::keywords::Keyword as K;
            matches!(w.keyword, K::AND | K::OR | K::NOT | K::IS | K::LIKE | K::ILIKE | K::IN | K::BETWEEN | K::COLLATE | K::SIMILAR | K::AT)
        }
        Token::Whitespace(_)
        | Token::Number(..)
        | Token::Char(_)
        | Token::SingleQuotedString(_)
        | Token::DoubleQuotedString(_)
        | Token::DollarQuotedString(_)
        | Token::EscapedStringLiteral(_)
        | Token::NationalStringLiteral(_)
        | Token::HexStringLiteral(_)
        | Token::Comma
        | Token::LParen
        | Token::RParen
        | Token::Period
        | Token::SemiColon
        | Token::LBracket
        | Token::RBracket
        | Token::Placeholder(_)
        | Token::EOF => false,
        _ => true,
    }
}

/// One parsed statement with its canonical text.
struct Parsed {
    stmt: Statement,
    canonical: String,
}

fn parse_one(sql: &str, engine: EngineKind) -> Result<Parsed, Refusal> {
    let d = dialect(engine);
    let toks = tokens(sql, &*d)?;
    let mut statements = Parser::new(&*d)
        .with_recursion_limit(PARSER_RECURSION_LIMIT)
        .with_tokens(toks)
        .parse_statements()
        .map_err(|e| refused(RefusalCode::ParseError, e.to_string()))?;
    let stmt = match statements.len() {
        0 => return Err(refused(RefusalCode::ParseError, "no statement")),
        1 => statements.pop().ok_or_else(|| refused(RefusalCode::ParseError, "no statement"))?,
        n => return Err(refused(RefusalCode::MultipleStatements, format!("{n} statements: send exactly one"))),
    };
    let canonical = stmt.to_string();
    // The canonical text must parse back to itself as one statement: whatever runs is exactly
    // what was classified, and a rendering quirk cannot turn into a second statement.
    let again = Parser::new(&*d)
        .with_recursion_limit(PARSER_RECURSION_LIMIT)
        .try_with_sql(&canonical)
        .and_then(|mut p| p.parse_statements())
        .map_err(|e| refused(RefusalCode::ParseError, format!("the statement does not survive a round trip through the parser: {e}")))?;
    match again.as_slice() {
        [s] if s.to_string() == canonical => Ok(Parsed { stmt, canonical }),
        _ => Err(refused(RefusalCode::ParseError, "the statement does not survive a round trip through the parser")),
    }
}

/// Classify one agent statement (SECURITY.md layer B).
pub fn classify(sql: &str, engine: EngineKind, policy: &Policy) -> Verdict {
    let p = match parse_one(sql, engine) {
        Ok(p) => p,
        Err(r) => return Verdict::Refused(r),
    };
    let root = statement::root(&p.stmt, engine);
    let found = walk::walk(&p.stmt, engine, policy);
    if found.too_deep {
        return Verdict::Refused(refused(RefusalCode::TooLarge, format!("nested deeper than {MAX_DEPTH} levels")));
    }
    if let Some(why) = found.never {
        return Verdict::Refused(refused(RefusalCode::NotReadOnly, why));
    }
    let kind = match root {
        Root::Never(why) => return Verdict::Refused(refused(RefusalCode::NotReadOnly, why)),
        Root::Write(_) => return Verdict::Write(write_statement(sql, &p.stmt, root, found, engine, policy)),
        Root::Read(kind) => kind,
    };
    if found.write.is_some() {
        return Verdict::Write(write_statement(sql, &p.stmt, root, found, engine, policy));
    }
    if let Some(why) = found.denied {
        return Verdict::Refused(refused(RefusalCode::DeniedFunction, why));
    }
    if let Some(why) = found.sensitive {
        return Verdict::Refused(refused(RefusalCode::Sensitive, why));
    }
    Verdict::Read(ReadStatement { sql: p.canonical.into(), engine, kind })
}

fn write_statement(sql: &str, stmt: &Statement, root: Root, found: walk::Findings, engine: EngineKind, policy: &Policy) -> WriteStatement {
    let kind = match (root, &found.write) {
        (Root::Write(k), _) => k,
        (_, Some((k, _))) => *k,
        _ => WriteKind::Select,
    };
    let mut tables = statement::targets(stmt);
    for t in found.tables {
        if !tables.contains(&t) {
            tables.push(t);
        }
    }
    let count = statement::count_sql(stmt).and_then(|c| match classify(&c, engine, policy) {
        Verdict::Read(r) => Some(r),
        _ => None,
    });
    WriteStatement { sql: sql.into(), engine, tables, kind, unbounded: statement::unbounded(stmt), count, assigned: statement::assigned(stmt) }
}

/// The `db.propose_write` path: one statement as an approval-card draft. Refused only when it
/// does not parse as exactly one statement, is too large, or is never allowed even with approval
/// (transaction control, session settings, prepared statements, cursors, ATTACH / DETACH,
/// `load_extension`). A plain read is accepted too (it changes nothing; `db.query` is the better
/// tool) with kind [`WriteKind::Select`].
pub fn propose_write(sql: &str, engine: EngineKind, policy: &Policy) -> Result<WriteStatement, Refusal> {
    let p = parse_one(sql, engine)?;
    let root = statement::root(&p.stmt, engine);
    let found = walk::walk(&p.stmt, engine, policy);
    if found.too_deep {
        return Err(refused(RefusalCode::TooLarge, format!("nested deeper than {MAX_DEPTH} levels")));
    }
    if let Root::Never(why) = root {
        return Err(refused(RefusalCode::NotReadOnly, format!("cannot be proposed: {why}")));
    }
    if let Some(why) = &found.never {
        return Err(refused(RefusalCode::NotReadOnly, format!("cannot be proposed: {why}")));
    }
    Ok(write_statement(sql, &p.stmt, root, found, engine, policy))
}

/// Check a `view.filter` WHERE clause: the clause as it will be applied (canonical, one
/// expression, comments dropped), or why not. The same rules as [`classify`] apply to it.
pub fn check_filter(where_clause: &str, engine: EngineKind, policy: &Policy) -> Result<String, Refusal> {
    let d = dialect(engine);
    let toks = tokens(where_clause, &*d)?;
    let mut parser = Parser::new(&*d).with_recursion_limit(PARSER_RECURSION_LIMIT).with_tokens(toks);
    let expr: Expr = parser.parse_expr().map_err(|e| refused(RefusalCode::ParseError, e.to_string()))?;
    if parser.peek_token().token != Token::EOF {
        return Err(refused(RefusalCode::ParseError, "a filter is one expression and nothing after it"));
    }
    if walk::walk_expr(&expr, engine, policy).too_deep {
        return Err(refused(RefusalCode::TooLarge, format!("nested deeper than {MAX_DEPTH} levels")));
    }
    let clause = expr.to_string();
    // The full rules (nested statements, functions, round trip) on the clause as applied.
    match classify(&format!("SELECT 1 WHERE {clause}"), engine, policy) {
        Verdict::Read(_) => Ok(clause),
        Verdict::Write(_) => Err(refused(RefusalCode::NotReadOnly, "the filter changes data")),
        Verdict::Refused(r) => Err(r),
    }
}

/// `stmt` limited to at most `cap` rows (clamped to `1..=AGENT_ROW_CAP_MAX`), for an agent
/// read. A literal LIMIT above the cap is lowered, a missing one added; any other row clause
/// (a computed LIMIT, FETCH FIRST, OFFSET-comma forms) is wrapped as
/// `SELECT * FROM (…) AS quarry_capped LIMIT cap`. EXPLAIN, SHOW and pragmas are returned
/// unchanged (they return a handful of rows).
pub fn cap_rows(stmt: &ReadStatement, cap: u32) -> ReadStatement {
    let cap = cap.clamp(1, AGENT_ROW_CAP_MAX);
    if stmt.kind != ReadKind::Select {
        return stmt.clone();
    }
    let Ok(mut p) = parse_one(&stmt.sql, stmt.engine) else { return wrap_cap(stmt, cap) };
    let Statement::Query(q) = &mut p.stmt else { return wrap_cap(stmt, cap) };
    if q.fetch.is_some() {
        return wrap_cap(stmt, cap);
    }
    let cap_expr = Expr::value(sqlparser::ast::Value::Number(cap.to_string(), false));
    match &mut q.limit_clause {
        None => q.limit_clause = Some(LimitClause::LimitOffset { limit: Some(cap_expr), offset: None, limit_by: Vec::new() }),
        Some(LimitClause::LimitOffset { limit, limit_by, .. }) if limit_by.is_empty() => match limit {
            None => *limit = Some(cap_expr),
            Some(Expr::Value(v)) => match &v.value {
                sqlparser::ast::Value::Number(n, _) if n.parse::<u64>().is_ok_and(|n| n <= u64::from(cap)) => {}
                sqlparser::ast::Value::Number(..) | sqlparser::ast::Value::Null => *limit = Some(cap_expr),
                _ => return wrap_cap(stmt, cap),
            },
            Some(_) => return wrap_cap(stmt, cap),
        },
        Some(_) => return wrap_cap(stmt, cap),
    }
    ReadStatement { sql: p.stmt.to_string().into(), engine: stmt.engine, kind: ReadKind::Select }
}

fn wrap_cap(stmt: &ReadStatement, cap: u32) -> ReadStatement {
    ReadStatement { sql: format!("SELECT * FROM ({}) AS quarry_capped LIMIT {cap}", stmt.sql).into(), engine: stmt.engine, kind: ReadKind::Select }
}

/// The read-only function allow-list for `engine` (SECURITY.md documents it).
pub fn allowed_functions(engine: EngineKind) -> Vec<&'static str> {
    functions::allow_list(engine)
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

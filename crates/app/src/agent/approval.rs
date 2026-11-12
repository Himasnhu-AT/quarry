//! Write approval (SECURITY.md layer C): `db.propose_write` stores a pending proposal and never
//! runs it; the person approves or denies it on its card in the window ([`approve`], [`deny`],
//! called only by `crate::human::perform` with a `HumanTicket`); the app runs an approved one on
//! the person's connection as exactly one statement in one transaction ([`execute`], in a job),
//! rolling back when the affected rows leave the estimate's tolerance. Every step is audited.
//!
//! What the card shows beyond the [`quarry_core::Proposal`] (kind, unbounded, the preview rows,
//! the undo statements) is a [`Card`], kept here by proposal id.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use ashlar::prelude::{CommandError, Value as J, json};
use quarry_core::limits::{APPROVAL_PREVIEW_ROWS, MAX_KEPT_PROPOSALS};
use quarry_core::tool::{ProposalStatusResult, ProposeResult};
use quarry_core::{
    Actor, AuditEntry, AuditKind, ConnColor, EngineKind, Estimate, Proposal, ProposalEvent, ProposalId, ProposalStatus, Refusal, RefusalCode,
    Timestamp, TransitionError, Value,
};
use quarry_engine::SchemaFilter;
use quarry_guard::{ReadStatement, Verdict, WriteKind};

use super::read;
use crate::ids;
use crate::services::{ConnEntry, now, services};

/// What `db.propose_write` tells the agent.
pub const NOTE: &str = "A person must approve this in the Quarry window. Poll db.proposal_status.";
/// Timeout of the estimate's count and the preview on the read-only connection.
pub const ESTIMATE_TIMEOUT_MS: u32 = 5_000;
/// Longest `reason` kept.
pub const MAX_REASON_CHARS: usize = 2_000;

/// The card's extras for one proposal.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub kind: WriteKind,
    /// UPDATE / DELETE without WHERE, or TRUNCATE.
    pub unbounded: bool,
    pub connection_name: String,
    pub color: ConnColor,
    /// Why there is no estimate, when there is none.
    pub estimate_note: Option<String>,
    /// The rows an UPDATE / DELETE touches, when there are at most [`APPROVAL_PREVIEW_ROWS`]
    /// (redacted like an agent result).
    pub preview: Option<Preview>,
    /// After it ran: statements that undo it (row edits of at most 20 rows with a known key).
    pub undo: Option<Vec<String>>,
    /// `SELECT * FROM <table> WHERE <same where>` (classified), for the preview and undo.
    rows_sql: Option<ReadStatement>,
}

/// Rows shown on a card, as text.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

fn cards() -> MutexGuard<'static, HashMap<ProposalId, Card>> {
    static C: OnceLock<Mutex<HashMap<ProposalId, Card>>> = OnceLock::new();
    C.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner)
}

/// The card extras of proposal `id`.
pub fn card(id: ProposalId) -> Option<Card> {
    cards().get(&id).cloned()
}

fn keep_card(id: ProposalId, card: Card) {
    let mut c = cards();
    if c.len() >= MAX_KEPT_PROPOSALS {
        let book = services().proposals();
        c.retain(|k, _| book.get(*k).is_some());
    }
    c.insert(id, card);
}

/// A card for a scene (no database behind it).
pub(crate) fn scene_card(kind: WriteKind, name: &str, color: ConnColor, preview: Option<Preview>, undo: Option<Vec<String>>) -> Card {
    Card { kind, unbounded: false, connection_name: name.into(), color, estimate_note: None, preview, undo, rows_sql: None }
}

/// One audit line about proposal `p`.
pub(crate) fn audit(kind: AuditKind, actor: Actor, p: &Proposal, rows_affected: Option<u64>, undo_sql: Option<Vec<String>>, note: Option<String>) {
    services().audit().record(AuditEntry {
        at: now(),
        actor,
        kind,
        connection: p.connection.clone(),
        proposal: Some(p.id),
        sql: p.sql.clone(),
        rows_affected,
        undo_sql,
        note,
    });
}

fn refuse(code: RefusalCode, msg: impl Into<String>) -> Refusal {
    Refusal::new(code, msg)
}

/// `db.propose_write`: draft `sql` with the guard, store it pending, estimate the rows it
/// touches on the read-only connection, and return its id. Runs nothing on the person's
/// connection.
pub fn propose(connection: &str, sql: &str, reason: &str) -> Result<ProposeResult, Refusal> {
    let e = read::entry(connection)?;
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(refuse(RefusalCode::ParseError, "say why in `reason`: the person reads it on the card"));
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(refuse(RefusalCode::TooLarge, format!("`reason` is longer than {MAX_REASON_CHARS} characters")));
    }
    let w = quarry_guard::propose_write(sql, e.spec().engine(), &e.policy())?;
    let id = services().proposals().propose(e.spec().id.clone(), &*w.sql, reason, w.tables.clone(), now())?;
    let mut card = Card {
        kind: w.kind,
        unbounded: w.unbounded,
        connection_name: e.spec().name.clone(),
        color: e.spec().color,
        estimate_note: None,
        preview: None,
        undo: None,
        rows_sql: None,
    };
    match &w.count {
        Some(count) => match count_rows(&e, count) {
            Ok(n) => {
                let est = Estimate { rows: n, exact: true, tolerance: n / 10 };
                let _ = services().proposals().set_estimate(id, est);
                card.rows_sql = rows_statement(&e, count);
                if n <= u64::from(APPROVAL_PREVIEW_ROWS)
                    && let Some(rs) = &card.rows_sql
                {
                    card.preview = read::run(&e, rs, APPROVAL_PREVIEW_ROWS, ESTIMATE_TIMEOUT_MS).ok().map(|r| preview(&r));
                }
            }
            Err(r) => card.estimate_note = Some(format!("no estimate: {r}")),
        },
        None => {
            card.estimate_note = Some(match w.kind {
                WriteKind::Insert => "no estimate: rows an INSERT adds are not counted before it runs".into(),
                WriteKind::Update | WriteKind::Delete => "no estimate: the statement joins or its WHERE is not a plain read".into(),
                _ => format!("no estimate for {}", w.kind.as_str()),
            })
        }
    }
    keep_card(id, card);
    let p = services().proposals().get(id).cloned().ok_or_else(|| refuse(RefusalCode::NotFound, "the proposal is gone"))?;
    audit(AuditKind::Proposed, Actor::Agent, &p, None, None, Some(p.reason.clone()));
    Ok(ProposeResult { proposal_id: id, status: p.status, expires_at: p.expires_at, note: NOTE.into() })
}

fn count_rows(e: &ConnEntry, count: &ReadStatement) -> Result<u64, Refusal> {
    let (_, rows) = read::fetch(e, count, 1, ESTIMATE_TIMEOUT_MS)?;
    match rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => u64::try_from(*n).map_err(|_| refuse(RefusalCode::Database, "a negative count")),
        Some(Value::Text(t) | Value::Other(t)) => t.parse().map_err(|_| refuse(RefusalCode::Database, "the count is not a number")),
        _ => Err(refuse(RefusalCode::Database, "the count returned no number")),
    }
}

/// The guard's count (`SELECT count(*) FROM <t> WHERE …`, canonical) turned into the rows it
/// counts, classified again.
fn rows_statement(e: &ConnEntry, count: &ReadStatement) -> Option<ReadStatement> {
    let rest = count.sql().strip_prefix("SELECT count(*) FROM ")?;
    match quarry_guard::classify(&format!("SELECT * FROM {rest}"), e.spec().engine(), &e.policy()) {
        Verdict::Read(r) => Some(r),
        _ => None,
    }
}

fn preview(r: &read::Rows) -> Preview {
    let text = |v: &Value| -> String {
        let s = match v {
            Value::Null => "NULL".to_string(),
            Value::Redacted { len } => format!("[redacted {len} B]"),
            Value::Bytes { len, .. } => format!("[{len} bytes]"),
            Value::Clipped { preview, len } => format!("{preview}… [{len} B]"),
            other => match other.to_agent_json() {
                J::String(s) => s,
                j => j.to_string(),
            },
        };
        s.chars().take(60).collect()
    };
    Preview { columns: r.columns.iter().map(|c| c.name.clone()).collect(), rows: r.rows.iter().map(|row| row.iter().map(text).collect()).collect() }
}

/// `db.proposal_status`.
pub fn status(id: ProposalId) -> Result<ProposalStatusResult, Refusal> {
    expire_due(now());
    let book = services().proposals();
    book.get(id).map(ProposalStatusResult::from).ok_or_else(|| refuse(RefusalCode::NotFound, format!("no proposal {}", id.0)))
}

/// Expire every pending proposal past its deadline at `now` (the panel calls this every frame it
/// draws; status and propose calls too) and audit each.
pub fn expire_due(at: Timestamp) -> Vec<ProposalId> {
    let (ids, expired): (Vec<ProposalId>, Vec<Proposal>) = {
        let mut book = services().proposals();
        let ids = book.expire_due(at);
        let ps = ids.iter().filter_map(|id| book.get(*id).cloned()).collect();
        (ids, ps)
    };
    for p in &expired {
        audit(AuditKind::Expired, Actor::System, p, None, None, None);
    }
    ids
}

fn decision_error(id: &'static str, e: TransitionError) -> CommandError {
    let code = match e {
        TransitionError::NotFound(_) => RefusalCode::NotFound,
        _ => RefusalCode::NotReadOnly,
    };
    CommandError::failed(id, Refusal::new(code, e.to_string()).to_string())
}

/// Approve proposal `id` (human only: `crate::human::perform`) and mark it running; the caller
/// then runs [`execute`] in a job. A decision after the deadline expires the proposal instead.
pub(crate) fn approve(id: ProposalId) -> Result<J, CommandError> {
    let at = now();
    let r = {
        let mut book = services().proposals();
        book.apply(id, ProposalEvent::Approve, Actor::Human, at).cloned().and_then(|p| {
            book.apply(id, ProposalEvent::Start, Actor::System, at)?;
            Ok(p)
        })
    };
    match r {
        Ok(p) => {
            audit(AuditKind::Approved, Actor::Human, &p, None, None, None);
            Ok(json!({"proposal_id": id, "status": ProposalStatus::Running}))
        }
        Err(e) => {
            if e == TransitionError::Expired
                && let Some(p) = services().proposals().get(id).cloned()
            {
                audit(AuditKind::Expired, Actor::System, &p, None, None, Some("decided after the deadline".into()));
            }
            Err(decision_error(ids::PROPOSAL_APPROVE, e))
        }
    }
}

/// Deny proposal `id` (human only: `crate::human::perform`).
pub(crate) fn deny(id: ProposalId) -> Result<J, CommandError> {
    let r = services().proposals().apply(id, ProposalEvent::Deny, Actor::Human, now()).cloned();
    match r {
        Ok(p) => {
            audit(AuditKind::Denied, Actor::Human, &p, None, None, None);
            Ok(json!({"proposal_id": id, "status": p.status}))
        }
        Err(e) => {
            if e == TransitionError::Expired
                && let Some(p) = services().proposals().get(id).cloned()
            {
                audit(AuditKind::Expired, Actor::System, &p, None, None, Some("decided after the deadline".into()));
            }
            Err(decision_error(ids::PROPOSAL_DENY, e))
        }
    }
}

/// Run an approved (running) proposal on the person's connection: exactly the approved text, as
/// one statement, in one transaction, rolled back when the affected rows leave the estimate's
/// tolerance. Blocking: call it from a job. Returns the final status.
pub fn execute(id: ProposalId) -> ProposalStatus {
    let Some(p) = services().proposals().get(id).cloned() else { return ProposalStatus::Failed };
    if p.status != ProposalStatus::Running {
        return p.status;
    }
    match run_approved(&p) {
        Ok((rows, undo)) => {
            let done = services().proposals().apply(id, ProposalEvent::Finish { rows_affected: rows }, Actor::System, now()).is_ok();
            if let Some(c) = cards().get_mut(&id) {
                c.undo.clone_from(&undo);
            }
            audit(AuditKind::Executed, Actor::System, &p, Some(rows), undo, None);
            if done { ProposalStatus::Done } else { ProposalStatus::Failed }
        }
        Err(msg) => {
            let _ = services().proposals().apply(id, ProposalEvent::Fail { error: msg.clone() }, Actor::System, now());
            audit(AuditKind::Failed, Actor::System, &p, None, None, Some(msg));
            ProposalStatus::Failed
        }
    }
}

fn run_approved(p: &Proposal) -> Result<(u64, Option<Vec<String>>), String> {
    let e: Arc<ConnEntry> = services().connections().get(p.connection.as_str()).ok_or("the connection is no longer registered")?;
    if e.spec().human_read_only {
        return Err("this connection is read-only for the person too".into());
    }
    // what runs is what was approved: the exact text, still exactly one statement the guard drafts
    let assigned = match quarry_guard::propose_write(&p.sql, e.spec().engine(), &e.policy()) {
        Ok(w) if *w.sql == *p.sql => w.assigned,
        Ok(_) => return Err("the statement changed".into()),
        Err(r) => return Err(r.to_string()),
    };
    let card = card(p.id);
    let undo = card.as_ref().and_then(|c| undo_plan(&e, p, c, assigned.as_deref()));
    let out = e.with_human(|h| h.execute_in_tx(std::slice::from_ref(&p.sql), p.estimate)).map_err(|err| err.to_string())?;
    Ok((out.rows_affected, undo))
}

/// Statements that undo an UPDATE / DELETE of at most [`APPROVAL_PREVIEW_ROWS`] rows, from a
/// snapshot taken just before it runs: re-INSERT deleted rows, UPDATE changed rows back by
/// primary key. `None` when no inverse is possible (more rows, no key, clipped or binary cells),
/// and for an UPDATE that assigns a primary-key column or may touch more than its SET list
/// (`assigned` is `None`): the rows would no longer be found by the keys in the snapshot.
fn undo_plan(e: &ConnEntry, p: &Proposal, card: &Card, assigned: Option<&[String]>) -> Option<Vec<String>> {
    let rows_sql = card.rows_sql.as_ref()?;
    if !matches!(card.kind, WriteKind::Update | WriteKind::Delete) || p.estimate.is_none_or(|est| est.rows > u64::from(APPROVAL_PREVIEW_ROWS)) {
        return None;
    }
    let [table] = p.tables.as_slice() else { return None };
    let (columns, rows) = read::fetch(e, rows_sql, APPROVAL_PREVIEW_ROWS + 1, ESTIMATE_TIMEOUT_MS).ok()?;
    if rows.len() > APPROVAL_PREVIEW_ROWS as usize || rows.is_empty() {
        return None;
    }
    let engine = e.spec().engine();
    let t = read::quote_table(table);
    let names: Vec<String> = columns.iter().map(|c| read::quote_ident(&c.name)).collect();
    let mut out = Vec::with_capacity(rows.len());
    match card.kind {
        WriteKind::Delete => {
            for row in &rows {
                let lits: Option<Vec<String>> = row.iter().map(|v| literal(v, engine)).collect();
                out.push(format!("INSERT INTO {t} ({}) VALUES ({})", names.join(", "), lits?.join(", ")));
            }
        }
        _ => {
            let filter = SchemaFilter { schema: table.schema.clone(), table: Some(table.name.clone()), max_tables: 1 };
            let tree = e.with_agent(|c| c.schema(&filter)).ok()?;
            let key: Vec<usize> = tree
                .table(table)?
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|k| columns.iter().position(|c| c.name == k.name))
                .collect::<Option<Vec<usize>>>()?;
            if key.is_empty() {
                return None;
            }
            // an UPDATE that moves a row to another key is not undone by its old key
            let assigned = assigned?;
            let changes_key = key.iter().filter_map(|i| columns.get(*i)).any(|k| assigned.iter().any(|a| a.eq_ignore_ascii_case(&k.name)));
            if changes_key {
                return None;
            }
            for row in &rows {
                let lits: Vec<String> = row.iter().map(|v| literal(v, engine)).collect::<Option<Vec<String>>>()?;
                let set: Vec<String> =
                    names.iter().zip(&lits).enumerate().filter(|(i, _)| !key.contains(i)).map(|(_, (n, l))| format!("{n} = {l}")).collect();
                let cond: Vec<String> = key.iter().filter_map(|i| Some(format!("{} = {}", names.get(*i)?, lits.get(*i)?))).collect();
                out.push(format!("UPDATE {t} SET {} WHERE {}", set.join(", "), cond.join(" AND ")));
            }
        }
    }
    Some(out)
}

/// A cell as a SQL literal both engines read back to the same value (`None`: not restorable).
pub fn literal(v: &Value, engine: EngineKind) -> Option<String> {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    Some(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match engine {
            EngineKind::Sqlite => u8::from(*b).to_string(),
            EngineKind::Postgres => b.to_string().to_uppercase(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => quote(s),
        Value::Bytes { len, preview } if *len == preview.len() as u64 => {
            let hex: String = preview.iter().map(|b| format!("{b:02x}")).collect();
            match engine {
                EngineKind::Sqlite => format!("X'{hex}'"),
                EngineKind::Postgres => format!("'\\x{hex}'::bytea"),
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_round_trip_or_refuse() {
        assert_eq!(literal(&Value::text("O'Brien"), EngineKind::Sqlite).as_deref(), Some("'O''Brien'"));
        assert_eq!(literal(&Value::Bool(true), EngineKind::Postgres).as_deref(), Some("TRUE"));
        assert_eq!(literal(&Value::Float(0.1), EngineKind::Sqlite).as_deref(), Some("0.1"));
        assert_eq!(literal(&Value::Bytes { len: 2, preview: Arc::from(&[1u8, 255][..]) }, EngineKind::Sqlite).as_deref(), Some("X'01ff'"));
        assert_eq!(literal(&Value::Bytes { len: 900, preview: Arc::from(&[1u8][..]) }, EngineKind::Sqlite), None);
        assert_eq!(literal(&Value::Redacted { len: 3 }, EngineKind::Sqlite), None);
        assert_eq!(literal(&Value::Float(f64::NAN), EngineKind::Sqlite), None);
    }
}

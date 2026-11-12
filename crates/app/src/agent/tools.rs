//! The agent tools (PLAN §3, `docs/contracts.md` §1): command bodies for `db.*`, `view.*` and
//! `viz.*`. Each returns its `quarry_core::tool` result, or fails with a `Refusal`
//! (`"code: message"`). Every call is refused `agent_paused` while the person paused the agent,
//! and `rate_limited` past [`CALLS_PER_MINUTE`]; every call lands in the agent panel's activity
//! list, every read in the audit log.
//!
//! None of these writes to a database: reads go to the agent's read-only connection as a guard
//! [`quarry_guard::ReadStatement`]; `db.propose_write` only stores a proposal.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ashlar::prelude::*;
use quarry_core::limits::{AGENT_ROW_CAP_DEFAULT, AGENT_SQL_BYTES_MAX, AGENT_TIMEOUT_MS_DEFAULT};
use quarry_core::tool::*;
use quarry_core::{
    Actor, AuditEntry, AuditKind, ChartKind, ChartSpec, ConnectionId, PanelId, ProposalId, Refusal, RefusalCode, Region, Tab, TabId, TabKind,
    TableRef, Workspace,
};
use quarry_engine::SchemaFilter;
use quarry_viz::{Agg, ChartData, PanelData, PivotData};
use serde::Serialize;

use super::activity::{self, Jump};
use super::{approval, read};
use crate::services::{now, services};

type S = Session<Workspace>;
type R = Result<Value, CommandError>;

/// Tool calls an agent may make per minute (SECURITY.md layer D).
pub const CALLS_PER_MINUTE: usize = 600;
/// Tables `db.schema` returns at most.
pub const SCHEMA_MAX_TABLES: usize = 1_000;
/// Default rows of `db.sample`.
pub const SAMPLE_DEFAULT: u32 = 20;
/// Most ranges or indexes in `view.select`, and columns.
const MAX_SELECT_PARTS: usize = 1_000;

fn refuse(code: RefusalCode, msg: impl Into<String>) -> Refusal {
    Refusal::new(code, msg)
}

fn fail(r: &Refusal) -> CommandError {
    CommandError::failed("", r.to_string())
}

fn str_of<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("")
}

fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn u32_of(p: &Value, key: &str, default: u32) -> u32 {
    p.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(default)
}

/// Paused, rate limit.
fn gate() -> Result<(), Refusal> {
    if services().agent_paused() {
        return Err(refuse(RefusalCode::AgentPaused, "the person paused the agent in the Quarry window"));
    }
    static CALLS: OnceLock<Mutex<VecDeque<Instant>>> = OnceLock::new();
    let mut calls = CALLS.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    while calls.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
        calls.pop_front();
    }
    if calls.len() >= CALLS_PER_MINUTE {
        return Err(refuse(RefusalCode::RateLimited, format!("more than {CALLS_PER_MINUTE} tool calls in a minute")));
    }
    calls.push_back(now);
    Ok(())
}

/// What a tool call produced for the activity list besides its result.
struct Done<T> {
    result: T,
    rows: Option<u64>,
    jump: Option<Jump>,
}

fn done<T>(result: T) -> Done<T> {
    Done { result, rows: None, jump: None }
}

/// Run one tool call: gate, body, activity entry (and an audit line for reads), result JSON.
fn call<T: Serialize>(tool: &'static str, summary: &str, audit_read: Option<(&str, &str)>, body: impl FnOnce() -> Result<Done<T>, Refusal>) -> R {
    let started = Instant::now();
    let out = gate().and_then(|()| body());
    let elapsed = started.elapsed().as_millis() as u64;
    let refused = out.as_ref().err().map(ToString::to_string);
    let (rows, jump) = out.as_ref().map_or((None, None), |d| (d.rows, d.jump));
    activity::record(tool, summary, rows, elapsed, refused.clone(), jump);
    if let Some((connection, sql)) = audit_read {
        services().audit().record(AuditEntry {
            at: now(),
            actor: Actor::Agent,
            kind: AuditKind::AgentRead,
            connection: ConnectionId::new(connection),
            proposal: None,
            sql: sql.chars().take(AGENT_SQL_BYTES_MAX).collect(),
            rows_affected: None,
            undo_sql: None,
            note: refused.or_else(|| Some(format!("{tool}: {} rows", rows.unwrap_or(0)))),
        });
    }
    match out {
        Ok(d) => serde_json::to_value(&d.result).map_err(|e| fail(&refuse(RefusalCode::Database, e.to_string()))),
        Err(r) => Err(fail(&r)),
    }
}

// ---------------------------------------------------------------------------------------------
// db.*
// ---------------------------------------------------------------------------------------------

pub fn db_connections(_: &mut S, _: &Value) -> R {
    call(crate::ids::DB_CONNECTIONS, "connections", None, || {
        let connections = services().connections().summaries();
        let n = connections.len() as u64;
        Ok(Done { result: ConnectionsResult { connections }, rows: Some(n), jump: None })
    })
}

pub fn db_schema(_: &mut S, p: &Value) -> R {
    let conn = str_of(p, "connection");
    let filter = SchemaFilter {
        schema: opt_str(p, "schema").map(str::to_string),
        table: opt_str(p, "table").map(str::to_string),
        max_tables: SCHEMA_MAX_TABLES,
    };
    let summary = format!("{conn} {}", filter.table.as_deref().or(filter.schema.as_deref()).unwrap_or("all tables"));
    call(crate::ids::DB_SCHEMA, &summary, None, || {
        let e = read::entry(conn)?;
        let tree = e.with_agent(|c| c.schema(&filter))?;
        let n = tree.table_count() as u64;
        Ok(Done {
            result: SchemaResult { connection: e.spec().id.clone(), truncated: tree.table_count() >= SCHEMA_MAX_TABLES, namespaces: tree.namespaces },
            rows: Some(n),
            jump: None,
        })
    })
}

fn rows_result(connection: ConnectionId, r: read::Rows) -> Done<RowsResult> {
    let page = quarry_core::Page { first_row: 0, rows: r.rows };
    let result = RowsResult::from_page(connection, r.columns, &page, r.truncated, r.elapsed_ms);
    let n = result.row_count;
    Done { result, rows: Some(n), jump: None }
}

pub fn db_query(_: &mut S, p: &Value) -> R {
    let (conn, sql) = (str_of(p, "connection"), str_of(p, "sql"));
    let limit = u32_of(p, "limit", AGENT_ROW_CAP_DEFAULT);
    let timeout = u32_of(p, "timeout_ms", AGENT_TIMEOUT_MS_DEFAULT);
    call(crate::ids::DB_QUERY, sql, Some((conn, sql)), || {
        let e = read::entry(conn)?;
        let stmt = read::classify_read(&e, sql)?;
        Ok(rows_result(e.spec().id.clone(), read::run(&e, &stmt, limit, timeout)?))
    })
}

pub fn db_sample(_: &mut S, p: &Value) -> R {
    let (conn, table) = (str_of(p, "connection"), str_of(p, "table"));
    let n = u32_of(p, "n", SAMPLE_DEFAULT);
    let summary = format!("{table} ({n} rows)");
    call(crate::ids::DB_SAMPLE, &summary, Some((conn, &summary)), || {
        let e = read::entry(conn)?;
        let t = TableRef::parse(table).ok_or_else(|| refuse(RefusalCode::ParseError, "`table` is `name` or `schema.name`"))?;
        // ORDER BY random() reads the whole table: the statement timeout bounds it
        let sql = format!("SELECT * FROM {} ORDER BY random() LIMIT {n}", read::quote_table(&t));
        let stmt = read::classify_read(&e, &sql)?;
        Ok(rows_result(e.spec().id.clone(), read::run(&e, &stmt, n, AGENT_TIMEOUT_MS_DEFAULT)?))
    })
}

pub fn db_explain(_: &mut S, p: &Value) -> R {
    let (conn, sql) = (str_of(p, "connection"), str_of(p, "sql"));
    let analyze = p.get("analyze").and_then(Value::as_bool).unwrap_or(false);
    call(crate::ids::DB_EXPLAIN, sql, Some((conn, sql)), || {
        if analyze {
            return Err(refuse(RefusalCode::NotReadOnly, "EXPLAIN ANALYZE executes the statement; agents get plain EXPLAIN for now"));
        }
        let e = read::entry(conn)?;
        let stmt = read::classify_read(&e, sql)?;
        let started = Instant::now();
        let plan = e.with_agent(|c| c.explain(&stmt, false))?;
        Ok(done(ExplainResult { connection: e.spec().id.clone(), analyzed: false, plan, elapsed_ms: started.elapsed().as_millis() as u64 }))
    })
}

pub fn db_propose_write(_: &mut S, p: &Value) -> R {
    let (conn, sql, reason) = (str_of(p, "connection"), str_of(p, "sql"), str_of(p, "reason"));
    call(crate::ids::DB_PROPOSE_WRITE, sql, None, || {
        let r = approval::propose(conn, sql, reason)?;
        let est = services().proposals().get(r.proposal_id).and_then(|p| p.estimate).map(|e| e.rows);
        Ok(Done { jump: Some(Jump::Proposal(r.proposal_id)), rows: est, result: r })
    })
}

pub fn db_proposal_status(_: &mut S, p: &Value) -> R {
    let id = ProposalId(p.get("proposal_id").and_then(Value::as_u64).unwrap_or(0));
    call(crate::ids::DB_PROPOSAL_STATUS, &format!("proposal {}", id.0), None, || {
        Ok(Done { result: approval::status(id)?, rows: None, jump: Some(Jump::Proposal(id)) })
    })
}

// ---------------------------------------------------------------------------------------------
// view.*: change what the window shows, never the database
// ---------------------------------------------------------------------------------------------

fn tab_id(p: &Value) -> TabId {
    TabId(p.get("tab").and_then(Value::as_u64).unwrap_or(0))
}

pub fn view_open_table(s: &mut S, p: &Value) -> R {
    let (conn, table) = (str_of(p, "connection"), str_of(p, "table"));
    call(crate::ids::VIEW_OPEN_TABLE, &format!("{conn} {table}"), None, || {
        let e = read::entry(conn)?;
        let t = TableRef::parse(table).ok_or_else(|| refuse(RefusalCode::ParseError, "`table` is `name` or `schema.name`"))?;
        let filter = SchemaFilter { schema: t.schema.clone(), table: Some(t.name.clone()), max_tables: 1 };
        let tree = e.with_agent(|c| c.schema(&filter))?;
        if tree.table(&t).is_none() {
            return Err(refuse(RefusalCode::NotFound, format!("no table `{t}` on `{conn}`")));
        }
        let id = s.edit("Open Table", |w| {
            let id = TabId(w.next_id());
            w.tabs.push(Tab {
                id,
                title: t.to_string(),
                connection: Some(e.spec().id.clone()),
                kind: TabKind::Table { table: t.clone(), filter: None },
            });
            w.active = Some(id);
            id
        });
        Ok(Done { result: TabResult { tab: id }, rows: None, jump: Some(Jump::Tab(id)) })
    })
}

pub fn view_run_in_editor(s: &mut S, p: &Value) -> R {
    let sql = str_of(p, "sql");
    let conn = opt_str(p, "connection");
    call(crate::ids::VIEW_RUN_IN_EDITOR, sql, None, || {
        if sql.len() > AGENT_SQL_BYTES_MAX {
            return Err(refuse(RefusalCode::TooLarge, format!("the SQL is longer than {AGENT_SQL_BYTES_MAX} bytes")));
        }
        let connection = match conn {
            Some(c) => Some(
                services()
                    .connections()
                    .get(c)
                    .ok_or_else(|| refuse(RefusalCode::UnknownConnection, format!("no connection `{c}`")))?
                    .spec()
                    .id
                    .clone(),
            ),
            None => None,
        };
        let sql = sql.to_string();
        // shown, never run: running it is the person's ⌘↵ (a human-only action)
        let id = s.edit("Agent SQL in Editor", |w| {
            let id = TabId(w.next_id());
            let n = w.tabs.iter().filter(|t| matches!(t.kind, TabKind::Editor { .. })).count() + 1;
            w.tabs.push(Tab { id, title: format!("Query {n} (agent)"), connection, kind: TabKind::Editor { sql } });
            w.active = Some(id);
            id
        });
        Ok(Done { result: TabResult { tab: id }, rows: None, jump: Some(Jump::Tab(id)) })
    })
}

pub fn view_filter(s: &mut S, p: &Value) -> R {
    let tab = tab_id(p);
    let clause = str_of(p, "where");
    call(crate::ids::VIEW_FILTER, &format!("tab {} WHERE {clause}", tab.0), None, || {
        let doc = s.snapshot();
        let t = doc.tab(tab).ok_or_else(|| refuse(RefusalCode::NotFound, format!("no tab {}", tab.0)))?;
        let (TabKind::Table { .. }, Some(conn)) = (&t.kind, &t.connection) else {
            return Err(refuse(RefusalCode::NotFound, format!("tab {} is not a table tab", tab.0)));
        };
        let e = read::entry(conn.as_str())?;
        let applied = quarry_guard::check_filter(clause, e.spec().engine(), &e.policy())?;
        let set = applied.clone();
        s.edit("Agent Filter", |w| {
            if let Some(Tab { kind: TabKind::Table { filter, .. }, .. }) = w.tab_mut(tab) {
                *filter = Some(set);
            }
            w.active = Some(tab);
        });
        Ok(Done { result: FilterResult { tab, where_clause: applied }, rows: None, jump: Some(Jump::Tab(tab)) })
    })
}

/// Row ranges `[from, to]` (inclusive) from `[0, 5]` or `[{"from": 0, "to": 9}]`.
fn row_ranges(rows: &Value) -> Result<Vec<[u64; 2]>, Refusal> {
    let bad = || refuse(RefusalCode::ParseError, "`rows` is a list of row indexes or {\"from\", \"to\"} ranges");
    let items = rows.as_array().ok_or_else(bad)?;
    if items.len() > MAX_SELECT_PARTS {
        return Err(refuse(RefusalCode::TooLarge, format!("more than {MAX_SELECT_PARTS} rows or ranges")));
    }
    items
        .iter()
        .map(|v| match v {
            Value::Number(n) => n.as_u64().map(|i| [i, i]).ok_or_else(bad),
            Value::Object(o) => {
                let from = o.get("from").and_then(Value::as_u64).ok_or_else(bad)?;
                let to = o.get("to").and_then(Value::as_u64).ok_or_else(bad)?;
                Ok([from.min(to), from.max(to)])
            }
            _ => Err(bad()),
        })
        .collect()
}

pub fn view_select(s: &mut S, p: &Value) -> R {
    let tab = tab_id(p);
    call(crate::ids::VIEW_SELECT, &format!("tab {}", tab.0), None, || {
        if s.doc().tab(tab).is_none() {
            return Err(refuse(RefusalCode::NotFound, format!("no tab {}", tab.0)));
        }
        let ranges = row_ranges(p.get("rows").unwrap_or(&Value::Null))?;
        let columns: Vec<String> = match p.get("columns") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) if a.len() <= MAX_SELECT_PARTS => a
                .iter()
                .map(|c| c.as_str().map(str::to_string))
                .collect::<Option<_>>()
                .ok_or_else(|| refuse(RefusalCode::ParseError, "`columns` is a list of names"))?,
            Some(_) => return Err(refuse(RefusalCode::ParseError, "`columns` is a list of at most 1000 names")),
        };
        let rows = ranges.iter().fold(0u64, |n, [a, b]| n.saturating_add(b - a + 1));
        s.set_view("selection", &json!({"tab": tab, "rows": ranges, "columns": columns, "by_agent": true}));
        Ok(Done { result: SelectResult { tab, rows, columns: columns.len() as u64 }, rows: Some(rows), jump: Some(Jump::Tab(tab)) })
    })
}

pub fn view_focus(s: &mut S, p: &Value) -> R {
    let name = str_of(p, "region");
    call(crate::ids::VIEW_FOCUS, name, None, || {
        let region: Region = serde_json::from_value(json!(name)).map_err(|_| refuse(RefusalCode::ParseError, "unknown region"))?;
        s.set_view("focus", &region);
        if region == Region::Agent {
            s.set_view("agent_panel", &true);
        }
        Ok(done(FocusResult { region }))
    })
}

// ---------------------------------------------------------------------------------------------
// viz.*: charts and pivots from a guarded read, pinned in the document
// ---------------------------------------------------------------------------------------------

fn pin(s: &mut S, mut spec: ChartSpec, data: PanelData) -> PanelId {
    s.edit("Agent Chart", |w| {
        let panel = PanelId(w.next_id());
        spec.panel = panel;
        spec.tab = spec.tab.or(w.active);
        w.charts.push(spec);
        quarry_viz::store::put(panel, data);
        panel
    })
}

pub fn viz_chart(s: &mut S, p: &Value) -> R {
    let (conn, sql, x) = (str_of(p, "connection"), str_of(p, "sql"), str_of(p, "x"));
    let (y, series) = (opt_str(p, "y"), opt_str(p, "series"));
    let kind_name = str_of(p, "kind");
    call(crate::ids::VIZ_CHART, sql, Some((conn, sql)), || {
        let kind = ChartKind::from_name(kind_name)
            .filter(|k| *k != ChartKind::Pivot)
            .ok_or_else(|| refuse(RefusalCode::ParseError, "unknown chart kind"))?;
        let e = read::entry(conn)?;
        let stmt = read::classify_read(&e, sql)?;
        let rows = read::run(&e, &stmt, AGENT_ROW_CAP_DEFAULT, AGENT_TIMEOUT_MS_DEFAULT)?;
        let data = ChartData::build(kind, &rows.columns, &rows.rows, x, y, series).map_err(|m| refuse(RefusalCode::NotFound, m))?;
        let title = opt_str(p, "title").map_or_else(|| format!("{} by {x}", y.unwrap_or("count")), |t| t.chars().take(200).collect());
        let row_count = rows.rows.len() as u64;
        let spec = ChartSpec {
            panel: PanelId(0),
            kind,
            connection: e.spec().id.clone(),
            sql: sql.to_string(),
            x: x.to_string(),
            y: y.map(str::to_string),
            series: series.map(str::to_string),
            title,
            by_agent: true,
            tab: None,
            agg: None,
        };
        let panel = pin(s, spec, PanelData::Chart(data));
        Ok(Done { result: ChartResult { panel, kind, row_count, truncated: rows.truncated }, rows: Some(row_count), jump: Some(Jump::Panel(panel)) })
    })
}

pub fn viz_pivot(s: &mut S, p: &Value) -> R {
    let (conn, sql) = (str_of(p, "connection"), str_of(p, "sql"));
    let (rk, ck, vk) = (str_of(p, "rows"), str_of(p, "columns"), str_of(p, "value"));
    let agg_name = opt_str(p, "agg").unwrap_or("count");
    call(crate::ids::VIZ_PIVOT, sql, Some((conn, sql)), || {
        let agg = Agg::from_name(agg_name).ok_or_else(|| refuse(RefusalCode::ParseError, "unknown aggregate"))?;
        let e = read::entry(conn)?;
        let stmt = read::classify_read(&e, sql)?;
        let rows = read::run(&e, &stmt, AGENT_ROW_CAP_DEFAULT, AGENT_TIMEOUT_MS_DEFAULT)?;
        let data = PivotData::build(&rows.columns, &rows.rows, rk, ck, vk, agg).map_err(|m| refuse(RefusalCode::NotFound, m))?;
        let (row_count, column_count) = (data.row_keys.len() as u64, data.column_keys.len() as u64);
        let truncated = rows.truncated || data.capped;
        let spec = ChartSpec {
            panel: PanelId(0),
            kind: ChartKind::Pivot,
            connection: e.spec().id.clone(),
            sql: sql.to_string(),
            x: rk.to_string(),
            y: Some(vk.to_string()),
            series: Some(ck.to_string()),
            title: format!("{agg_name}({vk}) by {rk} × {ck}"),
            by_agent: true,
            tab: None,
            agg: Some(agg_name.to_string()),
        };
        let panel = pin(s, spec, PanelData::Pivot(data));
        Ok(Done { result: PivotResult { panel, row_count, column_count, truncated }, rows: Some(row_count), jump: Some(Jump::Panel(panel)) })
    })
}

pub fn viz_close(s: &mut S, p: &Value) -> R {
    let panel = PanelId(p.get("panel").and_then(Value::as_u64).unwrap_or(0));
    call(crate::ids::VIZ_CLOSE, &format!("panel {}", panel.0), None, || {
        if !s.doc().charts.iter().any(|c| c.panel == panel) {
            return Err(refuse(RefusalCode::NotFound, format!("no panel {}", panel.0)));
        }
        s.edit("Close Chart", |w| w.charts.retain(|c| c.panel != panel));
        quarry_viz::store::remove(panel);
        Ok(done(CloseResult { closed: panel }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_ranges_parse_both_forms() {
        assert_eq!(row_ranges(&json!([0, 5, {"from": 9, "to": 7}])).unwrap(), vec![[0, 0], [5, 5], [7, 9]]);
        assert!(row_ranges(&json!("0-5")).is_err());
        assert!(row_ranges(&json!([-1])).is_err());
    }
}

//! `edit.commit`, a human-only action (`crate::human::HumanAction::CommitEdits`): a table tab's
//! pending edits become [`super::edits::statements`], which run in one transaction
//! (`HumanConnection::execute_in_tx`) on the person's connection, the one the window opened
//! (`crate::services::Connections::attach_human`). Each statement is audited with its inverse.
//! On success the pending set is cleared (and the undo history with it: undo must not bring back
//! edits that are already in the database) and the grid reloads; on failure nothing changed in
//! the database and the edits stay pending.
//!
//! The window shows the diff and asks for a second, confirming click before it gets here
//! (`super::draw`); both clicks need a person (`crate::human::HumanTicket`).

use ashlar::prelude::*;
use quarry_core::{Actor, AuditEntry, AuditKind, ConnectionId, PendingEdits, TabId, Workspace};

use super::edits;
use crate::human::HumanTicket;
use crate::services::{now, services};

/// What a commit runs, fixed when the person confirmed it.
#[derive(Clone, Debug)]
pub struct Plan {
    pub tab: TabId,
    pub connection: ConnectionId,
    /// The pending set the statements came from.
    pub edits: PendingEdits,
    /// Each statement and its inverse.
    pub statements: Vec<(String, Option<String>)>,
}

/// The commit of `tab`'s pending edits, or why there is none.
pub fn plan(doc: &Workspace, tab: TabId) -> Result<Plan, String> {
    let e = doc.edits(tab).filter(|e| !e.is_empty()).ok_or("nothing to commit")?;
    let connection = doc.tab(tab).and_then(|t| t.connection.clone()).ok_or("the tab has no connection")?;
    let entry = services().connections().get(connection.as_str()).ok_or_else(|| format!("`{connection}` is not open"))?;
    if entry.spec().human_read_only {
        return Err("this connection is set read-only".into());
    }
    let statements = edits::with_undo(e, entry.spec().engine())?;
    Ok(Plan { tab, connection, edits: e.clone(), statements })
}

/// Run `plan` in one transaction on the person's connection and audit every statement
/// (blocking: call it from a job). Returns the rows affected.
pub fn run(plan: &Plan) -> Result<u64, String> {
    let entry = services().connections().get(plan.connection.as_str()).ok_or_else(|| format!("`{}` is not open", plan.connection))?;
    let sql: Vec<String> = plan.statements.iter().map(|(s, _)| s.clone()).collect();
    let r = entry.with_human(|h| h.execute_in_tx(&sql, None)).map_err(|e| e.to_string());
    for (i, (stmt, undo)) in plan.statements.iter().enumerate() {
        let (rows_affected, note) = match &r {
            Ok(out) => (out.per_statement.get(i).copied(), None),
            Err(e) => (None, Some(format!("rolled back: {e}"))),
        };
        services().audit().record(AuditEntry {
            at: now(),
            actor: Actor::Human,
            kind: AuditKind::HumanWrite,
            connection: plan.connection.clone(),
            proposal: None,
            sql: stmt.clone(),
            rows_affected,
            undo_sql: undo.clone().filter(|_| r.is_ok()).map(|u| vec![u]),
            note,
        });
    }
    r.map(|out| out.rows_affected)
}

/// After [`run`]: on success drop the committed edits (edits made meanwhile stay), clear the undo
/// history and reload the grid; on failure keep everything and say why on the grid.
pub fn finish(s: &mut Session<Workspace>, plan: &Plan, result: &Result<u64, String>) {
    match result {
        Ok(rows) => {
            let (tab, done) = (plan.tab, &plan.edits.changes);
            s.edit("Commit Pending Changes", |w| {
                if let Some(e) = w.edits.iter_mut().find(|e| e.tab == tab) {
                    e.changes.retain(|c| !done.contains(c));
                }
                w.edits.retain(|e| !e.is_empty());
            });
            s.store_mut().history_mut().clear();
            super::detach(s, tab);
            log::info!("committed {} statement(s), {rows} row(s)", plan.statements.len());
        }
        Err(e) => {
            let msg = format!("Commit failed, nothing changed: {e}");
            if super::with_grid(s, plan.tab, |g, _| g.notice = Some(msg.clone())).is_none() {
                s.toast(ToastLevel::Error, msg);
            }
        }
    }
}

/// Commit `tab`'s pending edits now (the ticket is consumed). Blocking: the window runs
/// [`start`] instead; this is `crate::human::perform`'s path.
pub fn commit_now(s: &mut Session<Workspace>, ticket: HumanTicket, tab: TabId) -> Result<u64, String> {
    let _consumed: HumanTicket = ticket;
    log::info!("human action {}", crate::ids::EDIT_COMMIT);
    let plan = plan(s.doc(), tab)?;
    let r = run(&plan);
    finish(s, &plan, &r);
    r
}

/// Commit `tab`'s pending edits in a job (the ticket is consumed).
pub fn start(cx: &mut Cx<'_, Workspace>, ticket: HumanTicket, tab: TabId) -> Result<(), String> {
    let _consumed: HumanTicket = ticket;
    log::info!("human action {}", crate::ids::EDIT_COMMIT);
    let plan = plan(cx.doc(), tab)?;
    let label = format!("Committing {} statement(s)", plan.statements.len());
    cx.jobs.job("quarry.commit", label).spawn(plan, |plan, _| {
        let r = run(&plan);
        Ok(apply(move |s: &mut Session<Workspace>| {
            finish(s, &plan, &r);
            Ok(json!({"tab": plan.tab, "rows_affected": r.as_ref().ok()}))
        }))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use quarry_core::{AgentAccess, ConnColor, ConnectionSpec, Tab, TabKind, TableRef, Target, Value};

    use super::*;

    fn setup(name: &str, read_only: bool) -> (tempfile::TempDir, std::path::PathBuf, ashlar::app::Headless<Workspace>) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.sqlite");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT NOT NULL);
             INSERT INTO orders VALUES (1, 'paid'), (2, 'paid'), (3, 'paid');",
        )
        .unwrap();
        services().audit().set_path(Some(dir.path().join("audit.jsonl")));
        services().connections().register(
            ConnectionSpec {
                id: ConnectionId::new(name),
                name: name.into(),
                target: Target::Sqlite { path: db.clone() },
                color: ConnColor::Local,
                agent_access: AgentAccess::ReadOnly,
                sensitive_patterns: vec![],
                human_read_only: read_only,
            },
            None,
        );
        let mut h = crate::app().headless();
        let conn = ConnectionId::new(name);
        h.session_mut().edit("Open", |w| {
            w.tabs.push(Tab {
                id: TabId(1),
                title: "orders".into(),
                connection: Some(conn),
                kind: TabKind::Table { table: TableRef::new(None, "orders"), filter: None },
            });
            w.active = Some(TabId(1));
            let mut e = PendingEdits::new(TabId(1), TableRef::new(None, "orders"), vec!["id".into()]);
            e.set(vec![Value::Int(1)], "status", Value::text("paid"), Value::text("shipped"));
            e.set(vec![Value::Int(2)], "id", Value::Int(2), Value::Int(20));
            e.delete(vec![Value::Int(3)], vec![("id".into(), Value::Int(3)), ("status".into(), Value::text("paid"))]);
            w.edits.push(e);
        });
        (dir, db, h)
    }

    fn rows(db: &std::path::Path) -> Vec<(i64, String)> {
        let c = rusqlite::Connection::open(db).unwrap();
        let mut s = c.prepare("SELECT id, status FROM orders ORDER BY id").unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    }

    #[test]
    fn a_commit_runs_in_one_transaction_audits_each_statement_and_clears_the_edits() {
        let (_dir, db, mut h) = setup("commit-ok", false);
        let rows_affected = commit_now(h.session_mut(), HumanTicket::for_tests(), TabId(1)).unwrap();
        assert_eq!(rows_affected, 3);
        assert_eq!(rows(&db), vec![(1, "shipped".into()), (20, "paid".into())]);
        assert!(h.session().doc().edits(TabId(1)).is_none(), "the committed edits are gone");
        assert!(!h.session().can_undo(), "undo cannot bring committed edits back");
        let audit: Vec<AuditEntry> = services().audit().recent().into_iter().filter(|a| a.connection.as_str() == "commit-ok").collect();
        assert_eq!(audit.len(), 3, "{audit:?}");
        assert!(audit.iter().all(|a| a.kind == AuditKind::HumanWrite && a.actor == Actor::Human && a.note.is_none()));
        // the inverses, newest first, put every row back (the key change included)
        let c = rusqlite::Connection::open(&db).unwrap();
        for a in audit.iter().rev() {
            for u in a.undo_sql.as_ref().unwrap() {
                c.execute(u, []).unwrap();
            }
        }
        assert_eq!(rows(&db), vec![(1, "paid".into()), (2, "paid".into()), (3, "paid".into())]);
    }

    #[test]
    fn a_failed_commit_changes_nothing_and_keeps_the_edits() {
        let (_dir, db, mut h) = setup("commit-fail", false);
        h.session_mut().edit("Bad", |w| {
            if let Some(e) = w.edits.first_mut() {
                e.set(vec![Value::Int(1)], "status", Value::text("paid"), Value::Null); // NOT NULL
            }
        });
        let before = h.session().doc().edits(TabId(1)).cloned();
        let err = commit_now(h.session_mut(), HumanTicket::for_tests(), TabId(1)).unwrap_err();
        assert!(err.contains("NOT NULL"), "{err}");
        assert_eq!(rows(&db), vec![(1, "paid".into()), (2, "paid".into()), (3, "paid".into())], "rolled back");
        assert_eq!(h.session().doc().edits(TabId(1)).cloned(), before);
        let audit: Vec<AuditEntry> = services().audit().recent().into_iter().filter(|a| a.connection.as_str() == "commit-fail").collect();
        assert!(!audit.is_empty() && audit.iter().all(|a| a.note.as_deref().is_some_and(|n| n.starts_with("rolled back")) && a.undo_sql.is_none()));
    }

    #[test]
    fn read_only_connections_and_empty_sets_are_refused() {
        let (_dir, db, mut h) = setup("commit-ro", true);
        assert!(commit_now(h.session_mut(), HumanTicket::for_tests(), TabId(1)).unwrap_err().contains("read-only"));
        assert_eq!(rows(&db).len(), 3);
        assert_eq!(plan(h.session().doc(), TabId(9)).unwrap_err(), "nothing to commit");
    }
}

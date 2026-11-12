#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Proposals expire after `PROPOSAL_TTL_MS` (SECURITY.md layer C): status says so, the audit log
//! records it with the system as the actor, and the pending cap counts only live proposals. Its
//! own binary: expiring moves the process-wide clock of the proposal book forward.

use ashlar::prelude::*;
use quarry::services::{now, services};
use quarry_core::limits::{MAX_PENDING_PROPOSALS, PROPOSAL_TTL_MS};
use quarry_core::*;
use serde_json::json;

#[test]
fn pending_proposals_expire_and_the_cap_holds_until_they_do() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.sqlite");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, 'a');")
        .unwrap();
    services().audit().set_path(None);
    services().connections().register(
        ConnectionSpec {
            id: ConnectionId::new("x"),
            name: "x".into(),
            target: Target::Sqlite { path: db.clone() },
            color: ConnColor::Prod,
            agent_access: AgentAccess::ReadOnly,
            sensitive_patterns: vec![],
            human_read_only: false,
        },
        None,
    );
    let mut h = quarry::app().headless();
    let propose = |h: &mut Headless<Workspace>| {
        h.execute(quarry::ids::DB_PROPOSE_WRITE, json!({"connection": "x", "sql": "UPDATE t SET v = 'b' WHERE id = 1", "reason": "r"}))
    };
    let ids: Vec<u64> = (0..MAX_PENDING_PROPOSALS).map(|_| propose(&mut h).unwrap()["proposal_id"].as_u64().unwrap()).collect();
    let e = propose(&mut h).unwrap_err();
    assert!(e.to_string().contains("rate_limited"), "{e}");

    let expired = quarry::agent::approval::expire_due(Timestamp(now().0 + PROPOSAL_TTL_MS + 1));
    assert_eq!(expired.len(), MAX_PENDING_PROPOSALS);
    for id in &ids {
        let st = h.execute(quarry::ids::DB_PROPOSAL_STATUS, json!({"proposal_id": id})).unwrap();
        assert_eq!(st["status"], json!("expired"));
        assert!(st["decided_at"].is_number());
    }
    let audit = services().audit().recent();
    assert_eq!(audit.iter().filter(|a| a.kind == AuditKind::Expired && a.actor == Actor::System).count(), MAX_PENDING_PROPOSALS);
    // expired ones free the cap; nothing ran
    assert!(propose(&mut h).is_ok());
    let v: String = rusqlite::Connection::open(&db).unwrap().query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(v, "a");
}

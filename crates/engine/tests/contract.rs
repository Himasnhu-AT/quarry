#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The engine contract in M0: both engines exist, refuse with `NotYet`, and SQLite is bundled.
//! PostgreSQL tests (M1) run only when `QUARRY_PG_URL` is set.

use std::path::PathBuf;

use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, EngineKind, Target};
use quarry_engine::{EngineError, engine};

fn sqlite_spec(path: PathBuf) -> ConnectionSpec {
    ConnectionSpec {
        id: ConnectionId::new("fixture"),
        name: "Fixture".into(),
        target: Target::Sqlite { path },
        color: ConnColor::Local,
        agent_access: AgentAccess::ReadOnly,
        sensitive_patterns: vec![],
        human_read_only: false,
    }
}

#[test]
fn engines_are_stubbed_and_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let spec = sqlite_spec(dir.path().join("x.sqlite"));
    for kind in [EngineKind::Postgres, EngineKind::Sqlite] {
        let e = engine(kind);
        assert_eq!(e.kind(), kind);
        assert_eq!(e.connect_human(&spec, None).err(), Some(EngineError::NotYet));
        assert_eq!(e.connect_agent(&spec, None).err(), Some(EngineError::NotYet));
    }
}

#[test]
fn sqlite_is_bundled() {
    assert!(quarry_engine::sqlite::library_version().starts_with("3."));
}

#[test]
fn agent_statements_come_from_the_guard_or_the_unchecked_test_hook() {
    let s = quarry_engine::ReadStatement::unchecked("DELETE FROM t", EngineKind::Sqlite);
    assert_eq!(s.sql(), "DELETE FROM t");
}

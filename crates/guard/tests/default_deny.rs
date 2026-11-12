#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! M0: the stub guard denies everything; these stay true when the allow-list lands.

use quarry_core::{Column, EngineKind, Page, RefusalCode, Value};
use quarry_guard::{Policy, Verdict, check_filter, classify, redact};

fn code(sql: &str, engine: EngineKind) -> Option<RefusalCode> {
    match classify(sql, engine, &Policy::default()) {
        Verdict::Refused(r) => Some(r.code),
        _ => None,
    }
}

#[test]
fn batches_garbage_and_writes_never_pass() {
    for engine in [EngineKind::Postgres, EngineKind::Sqlite] {
        assert_eq!(code("SELECT 1; DELETE FROM t", engine), Some(RefusalCode::MultipleStatements));
        assert_eq!(code("SELEKT", engine), Some(RefusalCode::ParseError));
        assert_eq!(code("", engine), Some(RefusalCode::ParseError));
        for write in ["DELETE FROM t", "DROP TABLE users", "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", "SELECT nextval('s')"] {
            assert!(!matches!(classify(write, engine, &Policy::default()), Verdict::Read(_)), "{write} on {engine:?}");
        }
    }
    assert!(check_filter("1=1); DROP TABLE t; --", EngineKind::Postgres, &Policy::default()).is_err());
}

#[test]
fn sensitive_columns_are_redacted_with_length_kept() {
    let mut cols = vec![Column::new("email", "text"), Column::new("password_hash", "text"), Column::new("note", "text")];
    let mut page = Page { first_row: 0, rows: vec![vec![Value::text("a@example.com"), Value::text("abcdef"), Value::Null]] };
    redact(&mut cols, &mut page, &Policy { sensitive_patterns: vec!["NOTE".into()], ..Policy::default() });
    assert_eq!(page.rows[0], vec![Value::text("a@example.com"), Value::Redacted { len: 6 }, Value::Null]);
    assert!(!cols[0].sensitive && cols[1].sensitive && cols[2].sensitive);
}

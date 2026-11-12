#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The proposal draft, the row cap, filters and read kinds.

use quarry_core::{EngineKind, RefusalCode, TableRef};
use quarry_guard::{Policy, ReadKind, ReadStatement, Verdict, WriteKind, allowed_functions, cap_rows, check_filter, classify, propose_write};

const PG: EngineKind = EngineKind::Postgres;
const LITE: EngineKind = EngineKind::Sqlite;

fn read(sql: &str, engine: EngineKind) -> ReadStatement {
    match classify(sql, engine, &Policy::default()) {
        Verdict::Read(r) => r,
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn proposal_drafts_carry_kind_tables_and_the_count_hook() {
    let p = Policy::default();
    let w = propose_write("DELETE FROM public.orders WHERE customer_id = 7", PG, &p).unwrap();
    assert_eq!(w.kind, WriteKind::Delete);
    assert_eq!(w.tables, vec![TableRef::new(Some("public"), "orders")]);
    assert!(!w.unbounded && w.approve_similar_allowed());
    let count = w.count.unwrap();
    assert_eq!(count.sql(), "SELECT count(*) FROM public.orders WHERE customer_id = 7");
    assert_eq!(count.kind(), ReadKind::Select);

    let w = propose_write("UPDATE orders o SET note = 'x' WHERE o.total > 10", LITE, &p).unwrap();
    assert_eq!((w.kind, w.tables.clone()), (WriteKind::Update, vec![TableRef::new(None, "orders")]));
    assert_eq!(w.count.unwrap().sql(), "SELECT count(*) FROM orders o WHERE o.total > 10");

    let w = propose_write("UPDATE orders SET note = NULL", PG, &p).unwrap();
    assert!(w.unbounded && !w.approve_similar_allowed());
    assert_eq!(w.count.unwrap().sql(), "SELECT count(*) FROM orders");

    // No count when the WHERE itself is not a safe read, or the statement joins.
    assert!(propose_write("DELETE FROM t WHERE id = nextval('s')", PG, &p).unwrap().count.is_none());
    assert!(propose_write("DELETE FROM t USING s WHERE s.id = t.id", PG, &p).unwrap().count.is_none());
    assert!(propose_write("INSERT INTO t VALUES (1)", PG, &p).unwrap().count.is_none());

    let w = propose_write("INSERT INTO s.t (a) VALUES (1)", PG, &p).unwrap();
    assert_eq!((w.kind, w.tables), (WriteKind::Insert, vec![TableRef::new(Some("s"), "t")]));
    let w = propose_write("TRUNCATE a, b", PG, &p).unwrap();
    assert_eq!(w.tables.len(), 2);
    assert!(w.unbounded);
    let w = propose_write("SELECT * INTO archive FROM orders", PG, &p).unwrap();
    assert_eq!((w.kind, w.tables), (WriteKind::Ddl, vec![TableRef::new(None, "archive")]));
    // The SQL on the card is exactly the agent's, comments and all.
    let sql = "DELETE FROM t -- old rows\nWHERE id < 10";
    assert_eq!(propose_write(sql, PG, &p).unwrap().sql.as_ref(), sql);
    // A plain read is accepted as a (pointless) proposal; garbage and batches are not.
    assert_eq!(propose_write("SELECT 1", PG, &p).unwrap().kind, WriteKind::Select);
    assert_eq!(propose_write("SELEKT", PG, &p).unwrap_err().code, RefusalCode::ParseError);
    assert_eq!(propose_write("DELETE FROM a; DELETE FROM b", PG, &p).unwrap_err().code, RefusalCode::MultipleStatements);
}

#[test]
fn updates_name_the_columns_they_assign() {
    let p = Policy::default();
    let a = |sql: &str, e| propose_write(sql, e, &p).unwrap().assigned;
    assert_eq!(a("UPDATE orders SET status = 'x', \"Total\" = 1 WHERE id = 3", PG), Some(vec!["status".into(), "Total".into()]));
    assert_eq!(a("UPDATE orders o SET o.id = id + 1 WHERE id = 3", LITE), Some(vec!["id".into()]));
    assert_eq!(a("UPDATE t SET (a, b) = (1, 2)", PG), Some(vec!["a".into(), "b".into()]));
    // wider than the SET list says, or not an UPDATE: unknown
    assert_eq!(a("UPDATE OR REPLACE t SET id = 2 WHERE id = 1", LITE), None);
    assert_eq!(a("UPDATE t SET v = s.v FROM s WHERE s.id = t.id", PG), None);
    assert_eq!(a("DELETE FROM t WHERE id = 1", PG), None);
}

#[test]
fn row_cap_goes_into_the_sql() {
    let cases = [
        ("SELECT * FROM t", 100, "SELECT * FROM t LIMIT 100"),
        ("SELECT * FROM t LIMIT 10", 100, "SELECT * FROM t LIMIT 10"),
        ("SELECT * FROM t LIMIT 1000", 100, "SELECT * FROM t LIMIT 100"),
        ("SELECT * FROM t LIMIT 1000 OFFSET 5", 100, "SELECT * FROM t LIMIT 100 OFFSET 5"),
        ("SELECT * FROM t ORDER BY id", 100_000, "SELECT * FROM t ORDER BY id LIMIT 5000"),
        ("SELECT 1 UNION SELECT 2", 1, "SELECT 1 UNION SELECT 2 LIMIT 1"),
        ("SELECT * FROM t", 0, "SELECT * FROM t LIMIT 1"),
    ];
    for (sql, cap, want) in cases {
        for engine in [PG, LITE] {
            assert_eq!(cap_rows(&read(sql, engine), cap).sql(), want, "{sql} on {engine:?}");
        }
    }
    let wrapped = cap_rows(&read("SELECT * FROM t FETCH FIRST 9000 ROWS ONLY", PG), 50);
    assert_eq!(wrapped.sql(), "SELECT * FROM (SELECT * FROM t FETCH FIRST 9000 ROWS ONLY) AS quarry_capped LIMIT 50");
    let computed = cap_rows(&read("SELECT * FROM t LIMIT (SELECT 3)", PG), 50);
    assert!(computed.sql().ends_with("AS quarry_capped LIMIT 50"), "{}", computed.sql());
    for (sql, engine) in [("EXPLAIN SELECT 1", PG), ("SHOW ALL", PG), ("PRAGMA page_count", LITE)] {
        let r = read(sql, engine);
        assert_eq!(cap_rows(&r, 5), r);
    }
}

#[test]
fn read_kinds() {
    assert_eq!(read("EXPLAIN SELECT 1", PG).kind(), ReadKind::Explain { analyze: false });
    assert_eq!(read("EXPLAIN ANALYZE SELECT 1", PG).kind(), ReadKind::Explain { analyze: true });
    assert_eq!(read("EXPLAIN (ANALYZE true, COSTS off) SELECT 1", PG).kind(), ReadKind::Explain { analyze: true });
    assert_eq!(read("EXPLAIN QUERY PLAN SELECT 1", LITE).kind(), ReadKind::Explain { analyze: false });
    assert_eq!(read("SHOW work_mem", PG).kind(), ReadKind::Show);
    assert_eq!(read("PRAGMA table_info('t')", LITE).kind(), ReadKind::Pragma);
    assert_eq!(read("VALUES (1)", LITE).kind(), ReadKind::Select);
}

#[test]
fn filters_are_one_canonical_expression() {
    let p = Policy::default();
    assert_eq!(check_filter("  id=1  /* why */ ", PG, &p).unwrap(), "id = 1");
    assert_eq!(check_filter("lower(name) LIKE 'a%'", LITE, &p).unwrap(), "lower(name) LIKE 'a%'");
    assert_eq!(check_filter("id IN (SELECT customer_id FROM orders)", PG, &p).unwrap(), "id IN (SELECT customer_id FROM orders)");
    assert_eq!(check_filter("", PG, &p).unwrap_err().code, RefusalCode::ParseError);
    assert_eq!(check_filter("id = 1; DROP TABLE t", PG, &p).unwrap_err().code, RefusalCode::ParseError);
    assert_eq!(check_filter("nextval('s') > 0", PG, &p).unwrap_err().code, RefusalCode::DeniedFunction);
    assert_eq!(check_filter("password = 'x'", PG, &p).unwrap_err().code, RefusalCode::Sensitive);
    assert_eq!(check_filter("EXISTS (WITH d AS (DELETE FROM t RETURNING 1) SELECT 1)", PG, &p).unwrap_err().code, RefusalCode::NotReadOnly);
}

#[test]
fn allow_lists_are_documented_size() {
    // SECURITY.md lists the families; this keeps the lists from silently emptying.
    assert!(allowed_functions(PG).len() > 200);
    assert!(allowed_functions(LITE).len() > 100);
    assert!(allowed_functions(PG).contains(&"date_trunc") && allowed_functions(LITE).contains(&"json_extract"));
}

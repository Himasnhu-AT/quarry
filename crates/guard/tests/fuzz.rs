#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Property tests with a seeded generator (reproducible, no extra dependency): random input never
//! panics; anything accepted re-parses as exactly one read statement and classifies the same
//! again; random `;`-joined batches are always refused.

use quarry_core::{EngineKind, RefusalCode};
use quarry_guard::{Policy, ReadKind, Verdict, cap_rows, check_filter, classify, propose_write};
use sqlparser::ast::Statement;
use sqlparser::dialect::{Dialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

/// xorshift64*: small, seeded, good enough to shake a parser.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const ENGINES: [EngineKind; 2] = [EngineKind::Postgres, EngineKind::Sqlite];

const FRAGMENTS: &[&str] = &[
    "SELECT",
    "select",
    "FROM",
    "WHERE",
    "WITH",
    "AS",
    "(",
    ")",
    ",",
    ";",
    ";;",
    "--",
    "\n",
    "/*",
    "*/",
    "'",
    "''",
    "\"",
    "$$",
    "$a$",
    "E'",
    "\\",
    "`",
    "[",
    "]",
    "*",
    "1",
    "0.5",
    "-",
    "+",
    "||",
    "=",
    "<>",
    "t",
    "users",
    "password",
    "x",
    "DELETE",
    "INSERT",
    "INTO",
    "UPDATE",
    "SET",
    "DROP",
    "TABLE",
    "VALUES",
    "RETURNING",
    "UNION",
    "ALL",
    "FOR",
    "SHARE",
    "EXPLAIN",
    "ANALYZE",
    "PRAGMA",
    "ATTACH",
    "COPY",
    "TO",
    "PROGRAM",
    "nextval(",
    "lower(",
    "count(*)",
    "pg_sleep(",
    "load_extension(",
    "CALL",
    "DO",
    "BEGIN",
    "COMMIT",
    "LIMIT",
    "ORDER BY",
    "GROUP BY",
    "NOT",
    "AND",
    "OR",
    "IS",
    "NULL",
    "::",
    "->",
    "->>",
    "@>",
    "\u{0}",
    "\u{feff}",
    "\u{037e}",
    "é",
    "😀",
    " ",
    "  ",
];

const SEEDS: &[&str] = &[
    "SELECT 1",
    "SELECT * FROM orders WHERE id = 1",
    "WITH t AS (SELECT id FROM orders) SELECT * FROM t",
    "SELECT count(*) FROM orders GROUP BY customer_id",
    "DELETE FROM t WHERE id = 1",
    "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
    "UPDATE t SET a = 1",
    "SELECT nextval('s')",
    "SELECT * FROM t FOR UPDATE",
    "COPY t TO PROGRAM 'x'",
    "SET default_transaction_read_only = off",
    "PRAGMA table_info('t')",
    "EXPLAIN ANALYZE SELECT 1",
    "SELECT $$a;b$$, 'c;d', \"e;f\"",
    "SELECT 1 /* ; */ -- ;",
];

fn dialect(engine: EngineKind) -> Box<dyn Dialect> {
    match engine {
        EngineKind::Postgres => Box::new(PostgreSqlDialect {}),
        EngineKind::Sqlite => Box::new(SQLiteDialect {}),
    }
}

/// Everything the guard does with `sql`; panics only on a broken property.
fn check(sql: &str) {
    let policy = Policy::default();
    for engine in ENGINES {
        let _ = check_filter(sql, engine, &policy);
        let _ = propose_write(sql, engine, &policy);
        if let Verdict::Read(r) = classify(sql, engine, &policy) {
            let parsed = Parser::parse_sql(&*dialect(engine), r.sql()).unwrap_or_else(|e| panic!("{sql:?} → {:?} does not parse: {e}", r.sql()));
            let [stmt] = parsed.as_slice() else { panic!("{sql:?} → {:?} is {} statements", r.sql(), parsed.len()) };
            let ok = match (r.kind(), stmt) {
                (ReadKind::Select, Statement::Query(_)) => true,
                (ReadKind::Explain { .. }, Statement::Explain { statement, .. }) => matches!(statement.as_ref(), Statement::Query(_)),
                (ReadKind::Show, Statement::ShowVariable { .. }) => engine == EngineKind::Postgres,
                (ReadKind::Pragma, Statement::Pragma { is_eq: false, .. }) => engine == EngineKind::Sqlite,
                _ => false,
            };
            assert!(ok, "{sql:?} accepted as {:?} but re-parses as {stmt:?}", r.kind());
            match classify(r.sql(), engine, &policy) {
                Verdict::Read(again) => assert_eq!(again.sql(), r.sql()),
                other => panic!("{sql:?}: canonical {:?} is now {other:?}", r.sql()),
            }
            let capped = cap_rows(&r, 10);
            assert!(matches!(classify(capped.sql(), engine, &policy), Verdict::Read(_)), "{sql:?}: capped {:?} is not a read", capped.sql());
        }
    }
}

#[test]
fn random_token_soup_never_panics_and_accepts_only_reads() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..4_000 {
        let n = 1 + rng.below(14);
        let sql: Vec<&str> = (0..n).map(|_| rng.pick(FRAGMENTS)).collect();
        check(&sql.join(" "));
        check(&sql.concat());
    }
}

#[test]
fn mutated_statements_never_panic_and_accept_only_reads() {
    let mut rng = Rng(42);
    for _ in 0..4_000 {
        let mut s: Vec<char> = rng.pick(SEEDS).chars().collect();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(s.len() + 1);
            match rng.below(3) {
                0 => s.insert(at.min(s.len()), rng.pick(FRAGMENTS).chars().next().unwrap_or(' ')),
                1 if at < s.len() => {
                    s.remove(at);
                }
                _ => {
                    let frag: Vec<char> = rng.pick(FRAGMENTS).chars().collect();
                    let at = at.min(s.len());
                    s.splice(at..at, frag);
                }
            }
        }
        check(&s.into_iter().collect::<String>());
    }
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(7);
    for _ in 0..3_000 {
        let len = rng.below(64);
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next() & 0xff) as u8).collect();
        check(&String::from_utf8_lossy(&bytes));
    }
}

#[test]
fn random_batches_are_refused() {
    let mut rng = Rng(1234);
    let policy = Policy::default();
    for _ in 0..2_000 {
        let n = 2 + rng.below(3);
        // (A seed ending in a `--` comment would swallow the rest of the line: not a batch.)
        let seeds: Vec<&str> = SEEDS.iter().copied().filter(|s| !s.contains("--")).collect();
        let parts: Vec<&str> = (0..n).map(|_| rng.pick(&seeds)).collect();
        let sep = rng.pick(&[";", "; ", ";\n", " ;", "; -- c\n", "; /* c */ "]);
        let sql = parts.join(sep);
        for engine in ENGINES {
            match classify(&sql, engine, &policy) {
                Verdict::Refused(r) => {
                    assert!(matches!(r.code, RefusalCode::MultipleStatements | RefusalCode::ParseError), "{sql:?}: {r}");
                }
                other => panic!("{sql:?} on {engine:?} was {other:?}"),
            }
            assert!(propose_write(&sql, engine, &policy).is_err(), "{sql:?}");
        }
    }
}

/// The worst input under the limits fits in a small thread stack (the app classifies from jobs).
#[test]
fn worst_case_inputs_fit_a_small_stack() {
    let inputs = vec![
        format!("SELECT 1{}", " + 1".repeat(quarry_guard::MAX_OPERATORS - 1)),
        format!("SELECT 1 WHERE {}", "1 = 1 AND ".repeat(quarry_guard::MAX_OPERATORS / 2 - 1) + "1 = 1"),
        format!("SELECT {}1{}", "(".repeat(60), ")".repeat(60)),
        format!("SELECT {}1", "- ".repeat(200)),
        format!("SELECT {}1", "NOT ".repeat(200)),
        format!("SELECT * FROM t WHERE id IN ({})", vec!["1"; 40_000].join(",")),
        format!("SELECT {}", vec!["lower('x')"; 10_000].join(",")),
        "SELECT (SELECT (SELECT (SELECT (SELECT 1))))".to_string(),
    ];
    let worker = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            for sql in &inputs {
                for engine in ENGINES {
                    let _ = classify(sql, engine, &Policy::default());
                    let _ = check_filter(sql, engine, &Policy::default());
                }
            }
        })
        .unwrap();
    worker.join().unwrap();
}

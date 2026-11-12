//! Fixture grids for the scenes (`quarry --scene results`, golden snapshots, `.ui` scripts): rows
//! shaped like `cargo xtask fixtures` (`orders`, `events`), generated in memory, so scenes need
//! no database.

use std::sync::Arc;

use ashlar::prelude::*;
use quarry_core::{Column, ConnectionId, EngineKind, PendingEdits, Tab, TabId, TabKind, TableRef, Total, Value, Workspace};

use super::model::{EditTarget, GridState, Inline, Status};
use super::source::{MemSource, Order};
use super::{attach, with_grid};

/// The grid's own scenes (after the shared ones in `crate::scenes::SCENES`).
pub const SCENES: &[&str] = &["grid-loading", "grid-values", "grid-edits", "grid-wide"];

const STATUSES: &[&str] = &["pending", "paid", "paid", "shipped", "delivered", "delivered", "refunded", "cancelled"];
const CURRENCIES: &[&str] = &["USD", "USD", "EUR", "GBP", "JPY"];

/// A small deterministic mix (splitmix64).
fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn pick<'a>(items: &[&'a str], r: u64) -> &'a str {
    items.get((r % items.len() as u64) as usize).copied().unwrap_or("")
}

fn iso(secs: u64) -> String {
    // days since 2023-01-01, good enough for fixtures: 30-day months
    let (d, s) = (secs / 86_400, secs % 86_400);
    format!("2023-{:02}-{:02}T{:02}:{:02}:{:02}Z", (d / 30) % 12 + 1, d % 30 + 1, s / 3600, (s / 60) % 60, s % 60)
}

/// The `orders` columns.
pub fn orders_columns() -> Arc<[Column]> {
    Arc::from(vec![
        Column::new("id", "INTEGER"),
        Column::new("customer_id", "INTEGER"),
        Column::new("status", "TEXT"),
        Column::new("total_cents", "INTEGER"),
        Column::new("currency", "TEXT"),
        Column::new("created_at", "TEXT"),
        Column::new("note", "TEXT"),
    ])
}

/// Row `i` of `orders` (id = i + 1).
pub fn order(i: u64) -> Vec<Value> {
    let r = mix(i);
    let note = match r % 10 {
        0..=5 => Value::Null,
        6 => Value::text(format!(
            "Leave at the door. Call {} on arrival; the bell is broken and the dog will bark at anyone who knocks.",
            5550100 + r % 99
        )),
        7 => Value::text("gift wrap\nplease, with a card"),
        _ => Value::text(pick(&["expedite", "fragile", "repeat customer", "address checked"], r >> 8)),
    };
    vec![
        Value::Int(i as i64 + 1),
        Value::Int(((r >> 4) % 100_000 + 1) as i64),
        Value::text(pick(STATUSES, r >> 12)),
        Value::Int(((r >> 20) % 50_000 + 199) as i64),
        Value::text(pick(CURRENCIES, r >> 28)),
        Value::Time(iso((r >> 32) % (365 * 86_400)).into()),
        note,
    ]
}

/// `orders` with `rows` rows, read like a server-side cursor (totals grow as pages arrive).
pub fn orders(rows: u64) -> GridState {
    let src = MemSource::generated(rows, Arc::new(order)).streaming(true);
    GridState::open(orders_columns(), Total::AtLeast(0), Box::new(src)).dialect(EngineKind::Sqlite)
}

/// Rows with every kind of value: NULL, JSON, bytes, long and clipped text, floats, booleans.
pub fn values() -> GridState {
    let columns: Arc<[Column]> = Arc::from(vec![
        Column::new("id", "int8"),
        Column::new("kind", "text"),
        Column::new("payload", "jsonb"),
        Column::new("ok", "bool"),
        Column::new("score", "float8"),
        Column::new("thumb", "bytea"),
        Column::new("body", "text"),
        Column::new("at", "timestamptz"),
    ]);
    let long = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(40);
    let rows: Vec<Vec<Value>> = (0..60u64)
        .map(|i| {
            let r = mix(i + 7);
            vec![
                Value::Int(i as i64 + 1),
                Value::text(pick(&["page_view", "add_to_cart", "checkout", "login", "search"], r)),
                if i % 5 == 3 {
                    Value::Null
                } else {
                    Value::Json(
                        format!(
                            "{{\"path\": \"/p/{}\", \"ms\": {}, \"tags\": [\"a\", \"b\"], \"user\": {{\"id\": {}, \"beta\": {}}}}}",
                            r % 900,
                            r % 300,
                            r % 10_000,
                            i % 2 == 0
                        )
                        .into(),
                    )
                },
                if i % 7 == 2 { Value::Null } else { Value::Bool(!r.is_multiple_of(3)) },
                match i % 9 {
                    4 => Value::Float(f64::NAN),
                    6 => Value::Null,
                    _ => Value::Float((r % 100_000) as f64 / 997.0),
                },
                match i % 4 {
                    0 => Value::Bytes {
                        len: 48_213,
                        preview: Arc::from(&[0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52][..]),
                    },
                    1 => Value::Null,
                    _ => Value::Bytes { len: 4, preview: Arc::from(&[0xde_u8, 0xad, 0xbe, 0xef][..]) },
                },
                match i % 6 {
                    0 => Value::Clipped { preview: long.clone().into(), len: 182_044 },
                    1 => Value::text(long.get(..300).unwrap_or("")),
                    2 => Value::Null,
                    3 => Value::text("first line\nsecond line\nthird"),
                    _ => Value::text("short"),
                },
                Value::Time(iso(i * 7_919).into()),
            ]
        })
        .collect();
    GridState::open(columns, Total::Known(60), Box::new(MemSource::new(rows)))
}

fn tab(s: &mut Session<Workspace>, title: &str, kind: TabKind) -> TabId {
    let title = title.to_string();
    s.edit("Scene", |w| {
        let id = TabId(w.next_id());
        w.tabs.push(Tab { id, title, connection: Some(ConnectionId::new("fixture")), kind });
        w.active = Some(id);
        id
    })
}

fn editor(s: &mut Session<Workspace>, sql: &str) -> TabId {
    tab(s, "Query 1", TabKind::Editor { sql: sql.into() })
}

/// The grid for the shared scene `results` (its editor tab and run state are the shell's), or the
/// grid's own scene: tabs, grid and document. `query-running` / `query-error` are the shell's.
pub fn setup(name: &str, s: &mut Session<Workspace>) {
    let active = s.doc().active;
    match (name, active) {
        ("results", Some(t)) => {
            attach(t, orders(10_000_000));
        }
        ("grid-loading", _) => {
            let t = editor(s, "SELECT * FROM orders");
            let mut g = orders(10_000_000);
            g.status = Status::Running;
            attach(t, g);
        }
        ("grid-values", _) => {
            let t = editor(s, "SELECT * FROM events_sample");
            attach(t, values());
            with_grid(t, |g, _| {
                g.layout.frozen = true;
                g.selection.select_only(2);
                g.cells.span = Some((2, 2));
                g.detail = Some((2, 2));
            });
        }
        ("grid-edits", _) => {
            let table = TableRef::new(None, "orders");
            let t = tab(s, "orders", TabKind::Table { table: table.clone(), filter: None });
            attach(t, orders(1_000_000).editable(EditTarget { table: table.clone(), key: vec![0] }));
            let mut e = PendingEdits::new(t, table, vec!["id".into()]);
            let row = |i: u64| order(i);
            let o2 = row(2);
            e.set(vec![Value::Int(3)], "status", o2.get(2).cloned().unwrap_or(Value::Null), Value::text("shipped"));
            e.set(vec![Value::Int(3)], "note", o2.get(6).cloned().unwrap_or(Value::Null), Value::text("left with neighbour"));
            e.set(vec![Value::Int(6)], "total_cents", row(5).get(3).cloned().unwrap_or(Value::Null), Value::Int(4_200));
            let before = orders_columns().iter().map(|c| c.name.clone()).zip(row(4)).collect();
            e.delete(vec![Value::Int(5)], before);
            e.insert(vec![
                ("customer_id".into(), Value::Int(42)),
                ("status".into(), Value::text("pending")),
                ("total_cents".into(), Value::Int(1_999)),
            ]);
            s.edit("Scene", |w| w.edits.push(e));
            with_grid(t, |_, d| d.review = true);
        }
        ("grid-wide", _) => {
            let t = editor(
                s,
                "SELECT o.*, c.name, c.email, c.country, c.created_at AS customer_since FROM orders o JOIN customers c ON c.id = o.customer_id",
            );
            let mut columns = orders_columns().to_vec();
            for (n, ty) in [("name", "TEXT"), ("email", "TEXT"), ("country", "TEXT"), ("customer_since", "TEXT")] {
                columns.push(Column::new(n, ty));
            }
            let rows: Vec<Vec<Value>> = (0..5_000)
                .map(|i| {
                    let mut r = order(i);
                    let m = mix(i ^ 0xC0FFEE);
                    let first = pick(&["Ada", "Grace", "Linus", "Zoë", "Åsa", "José", "Mei", "Priya"], m);
                    let last = pick(&["Lovelace", "Hopper", "Torvalds", "Okafor", "García", "Müller"], m >> 8);
                    r.push(Value::text(format!("{first} {last}")));
                    r.push(Value::text(format!("{}.{}@example.com", first.to_lowercase(), last.to_lowercase())));
                    r.push(Value::text(pick(&["US", "DE", "IN", "BR", "JP", "NG", "FR"], m >> 16)));
                    r.push(Value::Time(iso((m >> 24) % (365 * 86_400)).into()));
                    r
                })
                .collect();
            let src = MemSource::new(rows);
            let mut g = GridState::open(Arc::from(columns), src.total(), Box::new(src)).dialect(EngineKind::Sqlite);
            // a small, fully loaded result sorts on the client
            g.pump(0..30, &mut Inline);
            g.sort_by(Some(Order { column: 3, descending: true }), &mut Inline);
            g.pump(0..30, &mut Inline);
            g.layout.frozen = true;
            g.layout.scroll_by(3);
            g.selection.select_only(1);
            g.selection.extend_to(4);
            g.cells.span = Some((4, 5));
            attach(t, g);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_rows_are_stable_and_shaped_like_orders() {
        assert_eq!(order(0), order(0));
        assert_eq!(order(41).len(), orders_columns().len());
        assert_eq!(order(41).first(), Some(&Value::Int(42)));
        assert!((0..100).any(|i| order(i).get(6) == Some(&Value::Null)));
    }
}

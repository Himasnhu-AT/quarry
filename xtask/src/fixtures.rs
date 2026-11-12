//! A deterministic SQLite fixture: the same `--rows` and `--seed` give the same file, byte for
//! byte, on every OS (bundled SQLite, a fixed PRNG, no clock).

use std::path::Path;

use rusqlite::{Connection, params};

pub const DEFAULT_SEED: u64 = 0x05EE_D0F0_AA12;

const SCHEMA: &str = "
PRAGMA page_size = 4096;
CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    country TEXT NOT NULL,
    created_at TEXT NOT NULL,
    password_hash TEXT NOT NULL
);
CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    status TEXT NOT NULL,
    total_cents INTEGER NOT NULL,
    currency TEXT NOT NULL,
    created_at TEXT NOT NULL,
    note TEXT
);
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    order_id INTEGER REFERENCES orders(id),
    kind TEXT NOT NULL,
    at TEXT NOT NULL,
    payload TEXT NOT NULL
);
";

const INDEXES: &str = "
CREATE INDEX orders_customer ON orders(customer_id);
CREATE INDEX orders_created ON orders(created_at);
CREATE INDEX events_at ON events(at);
CREATE INDEX events_order ON events(order_id);
ANALYZE;
";

const FIRST: &[&str] =
    &["Ada", "Grace", "Linus", "Zoë", "Åsa", "José", "Mei", "Olu", "Priya", "Sven", "Yuki", "Émile", "Noor", "Ivan", "Fatima", "Kofi"];
const LAST: &[&str] =
    &["Lovelace", "Hopper", "Torvalds", "Ng", "Okafor", "García", "Müller", "Sato", "Kowalski", "Haddad", "O'Brien", "Nakamura", "Silva", "Rossi"];
const COUNTRIES: &[&str] = &["US", "DE", "IN", "BR", "JP", "NG", "FR", "GB", "SE", "MX", "KR", "EG"];
const STATUSES: &[&str] = &["pending", "paid", "paid", "paid", "shipped", "shipped", "delivered", "delivered", "delivered", "refunded", "cancelled"];
const CURRENCIES: &[&str] = &["USD", "USD", "USD", "EUR", "EUR", "GBP", "JPY", "INR"];
const KINDS: &[&str] = &["page_view", "page_view", "page_view", "add_to_cart", "checkout", "login", "logout", "search", "refund_request"];

/// 2023-01-01T00:00:00Z
const EPOCH_2023: u64 = 1_672_531_200;
/// Two years of seconds.
const SPAN: u64 = 2 * 365 * 24 * 3600;

/// splitmix64: small, fast, the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// 0..n (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let i = usize::try_from(self.below(items.len() as u64)).unwrap_or(0);
        items.get(i).copied().unwrap_or("")
    }
}

/// `2023-04-05T06:07:08Z` for Unix seconds (civil-from-days, Howard Hinnant).
fn iso(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `10M`, `250k`, `1_000` → a count.
pub fn parse_count(s: &str) -> Result<u64, String> {
    let t = s.trim().replace('_', "");
    let (num, mul) = if let Some(n) = t.strip_suffix(['k', 'K']) {
        (n, 1_000)
    } else if let Some(n) = t.strip_suffix(['m', 'M']) {
        (n, 1_000_000)
    } else {
        (t.as_str(), 1)
    };
    num.parse::<u64>().ok().and_then(|n| n.checked_mul(mul)).filter(|n| *n > 0).ok_or_else(|| format!("`{s}` is not a row count (e.g. 100k, 10M)"))
}

/// `10M`, `100k` or the plain number, for file names.
pub fn count_label(n: u64) -> String {
    if n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else if n.is_multiple_of(1_000) {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// SQLite integers are i64; fixture numbers stay far below `i64::MAX`.
fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn sql_err(e: rusqlite::Error) -> String {
    format!("sqlite: {e}")
}

/// Write the fixture to `out` (replacing it): `rows` orders and events, `rows / 10` customers.
/// `progress(done, total)` is called every million rows.
pub fn generate(out: &Path, rows: u64, seed: u64, progress: &mut dyn FnMut(u64, u64)) -> Result<String, String> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    match std::fs::remove_file(out) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{}: {e}", out.display())),
        _ => {}
    }
    let customers = (rows / 10).max(1);
    let mut db = Connection::open(out).map_err(sql_err)?;
    db.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF;").map_err(sql_err)?;
    db.execute_batch(SCHEMA).map_err(sql_err)?;
    let mut rng = Rng(seed);
    let total = customers + 2 * rows;
    let mut done = 0u64;
    let tx = db.transaction().map_err(sql_err)?;
    {
        let mut c = tx.prepare("INSERT INTO customers VALUES (?1, ?2, ?3, ?4, ?5, ?6)").map_err(sql_err)?;
        for id in 1..=customers {
            let (first, last) = (rng.pick(FIRST), rng.pick(LAST));
            let email = format!("{}.{}{id}@example.com", first.to_lowercase(), last.to_lowercase().replace('\'', ""));
            let hash = format!("$argon2id$v=19${:016x}{:016x}", rng.next(), rng.next());
            c.execute(params![int(id), format!("{first} {last}"), email, rng.pick(COUNTRIES), iso(EPOCH_2023 + rng.below(SPAN)), hash])
                .map_err(sql_err)?;
            done += 1;
            if done.is_multiple_of(1_000_000) {
                progress(done, total);
            }
        }
        let mut o = tx.prepare("INSERT INTO orders VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)").map_err(sql_err)?;
        for id in 1..=rows {
            let note: Option<String> = match rng.below(100) {
                0..=79 => None,
                80..=97 => Some(format!("gift wrap #{}", rng.below(1000))),
                // a few long notes for the cell viewer
                _ => Some("Leave at the door. ".repeat(usize::try_from(20 + rng.below(200)).unwrap_or(20))),
            };
            let cents = i64::try_from(rng.below(500_000)).unwrap_or(0) + 99;
            o.execute(params![
                int(id),
                int(1 + rng.below(customers)),
                rng.pick(STATUSES),
                cents,
                rng.pick(CURRENCIES),
                iso(EPOCH_2023 + id * SPAN / rows),
                note
            ])
            .map_err(sql_err)?;
            done += 1;
            if done.is_multiple_of(1_000_000) {
                progress(done, total);
            }
        }
        let mut e = tx.prepare("INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5, ?6)").map_err(sql_err)?;
        for id in 1..=rows {
            let kind = rng.pick(KINDS);
            let order = matches!(kind, "checkout" | "refund_request").then(|| int(1 + rng.below(rows)));
            let payload = format!("{{\"session\":\"{:012x}\",\"ms\":{},\"path\":\"/p/{}\"}}", rng.next() >> 16, rng.below(5_000), rng.below(500));
            e.execute(params![int(id), int(1 + rng.below(customers)), order, kind, iso(EPOCH_2023 + id * SPAN / rows), payload]).map_err(sql_err)?;
            done += 1;
            if done.is_multiple_of(1_000_000) {
                progress(done, total);
            }
        }
    }
    tx.commit().map_err(sql_err)?;
    db.execute_batch(INDEXES).map_err(sql_err)?;
    db.close().map_err(|(_, e)| sql_err(e))?;
    Ok(format!("{customers} customers, {rows} orders, {rows} events"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_parse() {
        assert_eq!(parse_count("10M"), Ok(10_000_000));
        assert_eq!(parse_count("250k"), Ok(250_000));
        assert_eq!(parse_count("1_000"), Ok(1_000));
        assert!(parse_count("0").is_err() && parse_count("x").is_err() && parse_count("99999999999999999M").is_err());
        assert_eq!(count_label(10_000_000), "10M");
        assert_eq!(count_label(1_500), "1500");
    }

    #[test]
    fn dates_are_iso() {
        assert_eq!(iso(EPOCH_2023), "2023-01-01T00:00:00Z");
        assert_eq!(iso(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn the_same_inputs_give_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (dir.path().join("a.sqlite"), dir.path().join("b.sqlite"), dir.path().join("c.sqlite"));
        let s = generate(&a, 2_000, 7, &mut |_, _| {}).unwrap();
        assert_eq!(s, "200 customers, 2000 orders, 2000 events");
        generate(&b, 2_000, 7, &mut |_, _| {}).unwrap();
        generate(&c, 2_000, 8, &mut |_, _| {}).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert_ne!(std::fs::read(&a).unwrap(), std::fs::read(&c).unwrap());
        let db = Connection::open(&a).unwrap();
        let n: i64 = db.query_row("SELECT count(*) FROM orders o JOIN customers c ON c.id = o.customer_id", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2_000);
        let stat: i64 = db.query_row("SELECT count(*) FROM sqlite_stat1", [], |r| r.get(0)).unwrap();
        assert!(stat > 0, "ANALYZE ran: row estimates for the schema tree");
    }
}

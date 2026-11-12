//! PostgreSQL binary values to [`Value`]. tokio-postgres asks for binary results; every column
//! is read as raw bytes ([`Raw`]) and decoded here by type, so no type ever fails a row:
//!
//! | Type | Value |
//! |---|---|
//! | bool | `Bool` |
//! | int2/4/8, oid, xid, reg* | `Int` |
//! | float4/8 | `Float` |
//! | numeric, money | `Text` (exact decimal) |
//! | text, varchar, bpchar, name, char, xml, citext, enums | `Text` |
//! | bytea | `Bytes` (length + preview) |
//! | json, jsonb | `Json` |
//! | date, time, timetz, timestamp, timestamptz, interval | `Time` (ISO 8601) |
//! | uuid, inet, cidr, macaddr, bit, varbit | `Text` |
//! | arrays, composites | `Json` (arrays nested, composites as objects) |
//! | ranges | `Text` (`[a,b)`) |
//! | domains | as their base type |
//! | anything else | `Other`: printable UTF-8 as is, else `\x` hex |
//!
//! Text and JSON are capped (`cells`). Malformed input never panics: it falls back to `Other`.

use std::error::Error;
use std::fmt::Write;

use quarry_core::Value;
use serde_json::Value as J;
use tokio_postgres::types::{FromSql, Kind, Type};

use crate::cells;

/// A column's bytes as the server sent them.
pub(crate) struct Raw<'a>(pub(crate) &'a [u8]);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        Ok(Raw(raw))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

/// Nesting of arrays and composites decoded; deeper values are shown as hex.
const MAX_DEPTH: u32 = 8;
/// Elements of one array or fields of one composite decoded.
const MAX_ELEMENTS: usize = 100_000;

/// One cell of a row.
pub(crate) fn cell(row: &tokio_postgres::Row, i: usize) -> Value {
    let Some(col) = row.columns().get(i) else { return Value::Null };
    match row.try_get::<_, Option<Raw<'_>>>(i) {
        Ok(Some(raw)) => decode(col.type_(), raw.0),
        Ok(None) | Err(_) => Value::Null,
    }
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N).and_then(|s| s.try_into().ok())
    }
    fn u8(&mut self) -> Option<u8> {
        self.arr::<1>().map(|a| a[0])
    }
    fn i16(&mut self) -> Option<i16> {
        self.arr().map(i16::from_be_bytes)
    }
    fn u16(&mut self) -> Option<u16> {
        self.arr().map(u16::from_be_bytes)
    }
    fn i32(&mut self) -> Option<i32> {
        self.arr().map(i32::from_be_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.arr().map(u32::from_be_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.arr().map(i64::from_be_bytes)
    }
    /// A length-prefixed value (`-1`: NULL).
    fn value(&mut self) -> Option<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len < 0 { Some(None) } else { self.take(usize::try_from(len).ok()?).map(Some) }
    }
}

fn exact<const N: usize>(raw: &[u8]) -> Option<[u8; N]> {
    raw.try_into().ok()
}

pub(crate) fn decode(ty: &Type, raw: &[u8]) -> Value {
    decode_at(ty, raw, 0).unwrap_or_else(|| cells::unknown(raw))
}

fn decode_at(ty: &Type, raw: &[u8], depth: u32) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    match ty.kind() {
        Kind::Array(elem) => return array(elem, raw, depth).map(|j| cells::json(&j.to_string())),
        Kind::Domain(base) => return decode_at(base, raw, depth + 1),
        Kind::Enum(_) => return std::str::from_utf8(raw).ok().map(cells::text),
        Kind::Range(elem) => return range(elem, raw, depth).map(|s| cells::text(&s)),
        Kind::Composite(fields) => {
            return composite(raw, depth, Some(fields.iter().map(|f| (f.name(), f.type_().clone())).collect())).map(|j| cells::json(&j.to_string()));
        }
        _ => {}
    }
    Some(match *ty {
        Type::BOOL => Value::Bool(exact::<1>(raw)?[0] != 0),
        Type::INT2 => Value::Int(i64::from(i16::from_be_bytes(exact(raw)?))),
        Type::INT4 => Value::Int(i64::from(i32::from_be_bytes(exact(raw)?))),
        Type::INT8 => Value::Int(i64::from_be_bytes(exact(raw)?)),
        Type::OID
        | Type::XID
        | Type::CID
        | Type::REGPROC
        | Type::REGPROCEDURE
        | Type::REGOPER
        | Type::REGOPERATOR
        | Type::REGCLASS
        | Type::REGTYPE
        | Type::REGROLE
        | Type::REGNAMESPACE
        | Type::REGCONFIG
        | Type::REGDICTIONARY => Value::Int(i64::from(u32::from_be_bytes(exact(raw)?))),
        Type::XID8 => Value::Int(i64::try_from(u64::from_be_bytes(exact(raw)?)).ok()?),
        Type::FLOAT4 => Value::Float(f64::from(f32::from_be_bytes(exact(raw)?))),
        Type::FLOAT8 => Value::Float(f64::from_be_bytes(exact(raw)?)),
        Type::NUMERIC => Value::Text(numeric(raw)?.into()),
        Type::MONEY => Value::Text(money(i64::from_be_bytes(exact(raw)?)).into()),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::XML | Type::UNKNOWN | Type::CHAR => cells::text(&String::from_utf8_lossy(raw)),
        Type::BYTEA => cells::bytes(raw),
        Type::JSON => cells::json(&String::from_utf8_lossy(raw)),
        Type::JSONB => match raw.split_first() {
            Some((1, rest)) => cells::json(&String::from_utf8_lossy(rest)),
            _ => return None,
        },
        Type::UUID => Value::Text(uuid(&exact::<16>(raw)?).into()),
        Type::DATE => Value::Time(date(i32::from_be_bytes(exact(raw)?)).into()),
        Type::TIME => Value::Time(time_of_day(i64::from_be_bytes(exact(raw)?)).into()),
        Type::TIMETZ => {
            let mut r = Rd(raw);
            let (us, zone) = (r.i64()?, r.i32()?);
            Value::Time(format!("{}{}", time_of_day(us), offset(zone.checked_neg()?)).into())
        }
        Type::TIMESTAMP => Value::Time(timestamp(i64::from_be_bytes(exact(raw)?), false).into()),
        Type::TIMESTAMPTZ => Value::Time(timestamp(i64::from_be_bytes(exact(raw)?), true).into()),
        Type::INTERVAL => {
            let mut r = Rd(raw);
            let (us, days, months) = (r.i64()?, r.i32()?, r.i32()?);
            Value::Time(interval(us, days, months).into())
        }
        Type::INET | Type::CIDR => Value::Text(inet(raw)?.into()),
        Type::MACADDR | Type::MACADDR8 => Value::Text(raw.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":").into()),
        Type::BIT | Type::VARBIT => {
            let mut r = Rd(raw);
            let bits = usize::try_from(r.i32()?).ok()?;
            let data = r.0;
            let mut s = String::with_capacity(bits.min(cells_cap()));
            for i in 0..bits.min(cells_cap()) {
                let byte = data.get(i / 8)?;
                s.push(if byte & (0x80 >> (i % 8)) != 0 { '1' } else { '0' });
            }
            cells::text(&s)
        }
        Type::RECORD => composite(raw, depth, None).map(|j| cells::json(&j.to_string()))?,
        _ => return None,
    })
}

fn cells_cap() -> usize {
    quarry_core::limits::CELL_CAP_BYTES
}

/// An element (or field) as JSON.
fn element(ty: &Type, raw: Option<&[u8]>, depth: u32) -> J {
    match raw {
        None => J::Null,
        Some(raw) => match ty.kind() {
            Kind::Array(elem) => array(elem, raw, depth + 1).unwrap_or_else(|| J::String(cells::as_text(&cells::unknown(raw)))),
            _ if *ty == Type::RECORD || matches!(ty.kind(), Kind::Composite(_)) => {
                let fields = match ty.kind() {
                    Kind::Composite(f) => Some(f.iter().map(|f| (f.name(), f.type_().clone())).collect()),
                    _ => None,
                };
                composite(raw, depth + 1, fields).unwrap_or_else(|| J::String(cells::as_text(&cells::unknown(raw))))
            }
            _ => cells::as_json(&decode_at(ty, raw, depth + 1).unwrap_or_else(|| cells::unknown(raw))),
        },
    }
}

/// Binary array: ndim, flags, element oid, (len, lower bound) per dimension, then the elements.
fn array(elem: &Type, raw: &[u8], depth: u32) -> Option<J> {
    let mut r = Rd(raw);
    let ndim = usize::try_from(r.i32()?).ok()?;
    let _flags = r.i32()?;
    let _oid = r.u32()?;
    if ndim == 0 {
        return Some(J::Array(Vec::new()));
    }
    if ndim > 6 {
        return None;
    }
    let mut dims = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        dims.push(usize::try_from(r.i32()?).ok()?);
        let _lower = r.i32()?;
    }
    let total = dims.iter().try_fold(1usize, |a, d| a.checked_mul(*d))?;
    if total > MAX_ELEMENTS {
        return None;
    }
    let mut flat = Vec::with_capacity(total);
    for _ in 0..total {
        flat.push(element(elem, r.value()?, depth));
    }
    // nest from the innermost dimension out
    let mut level = flat;
    for d in dims.iter().skip(1).rev() {
        let d = (*d).max(1);
        let mut next = Vec::with_capacity(level.len() / d);
        let mut it = level.into_iter();
        loop {
            let chunk: Vec<J> = it.by_ref().take(d).collect();
            if chunk.is_empty() {
                break;
            }
            next.push(J::Array(chunk));
        }
        level = next;
    }
    Some(J::Array(level))
}

/// Binary record: field count, then (oid, value) per field. Named fields give an object.
fn composite(raw: &[u8], depth: u32, fields: Option<Vec<(&str, Type)>>) -> Option<J> {
    let mut r = Rd(raw);
    let n = usize::try_from(r.i32()?).ok()?;
    if n > MAX_ELEMENTS {
        return None;
    }
    let mut values = Vec::with_capacity(n);
    for i in 0..n {
        let oid = r.u32()?;
        let v = r.value()?;
        let ty = fields.as_ref().and_then(|f| f.get(i)).map(|f| f.1.clone()).or_else(|| Type::from_oid(oid));
        values.push(match ty {
            Some(ty) => element(&ty, v, depth),
            None => v.map_or(J::Null, |b| J::String(cells::as_text(&cells::unknown(b)))),
        });
    }
    match fields {
        Some(f) if f.len() == values.len() => Some(J::Object(f.iter().map(|(name, _)| name.to_string()).zip(values).collect())),
        _ => Some(J::Array(values)),
    }
}

/// Binary range: flags, then the bounds that are present.
fn range(elem: &Type, raw: &[u8], depth: u32) -> Option<String> {
    let mut r = Rd(raw);
    let flags = r.u8()?;
    if flags & 0x01 != 0 {
        return Some("empty".into());
    }
    let bound = |r: &mut Rd<'_>, inf: bool| -> Option<String> {
        if inf {
            return Some(String::new());
        }
        let v = r.value()??;
        Some(cells::as_text(&decode_at(elem, v, depth + 1).unwrap_or_else(|| cells::unknown(v))))
    };
    let lower = bound(&mut r, flags & 0x08 != 0)?;
    let upper = bound(&mut r, flags & 0x10 != 0)?;
    Some(format!("{}{lower},{upper}{}", if flags & 0x02 != 0 { '[' } else { '(' }, if flags & 0x04 != 0 { ']' } else { ')' }))
}

/// Binary numeric: ndigits, weight, sign, dscale, then base-10000 digits.
fn numeric(raw: &[u8]) -> Option<String> {
    let mut r = Rd(raw);
    let ndigits = usize::try_from(r.i16()?).ok()?;
    let weight = i64::from(r.i16()?);
    let sign = r.u16()?;
    let dscale = usize::from(r.u16()?);
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        0x0000 | 0x4000 => {}
        _ => return None,
    }
    let mut digits = Vec::with_capacity(ndigits);
    for _ in 0..ndigits {
        let d = r.i16()?;
        if !(0..10_000).contains(&d) {
            return None;
        }
        digits.push(d);
    }
    let digit = |i: i64| usize::try_from(i).ok().and_then(|i| digits.get(i)).copied().unwrap_or(0);
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                let _ = write!(s, "{}", digit(i));
            } else {
                let _ = write!(s, "{:04}", digit(i));
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::with_capacity(dscale + 4);
        let mut i = weight + 1;
        while frac.len() < dscale {
            let _ = write!(frac, "{:04}", digit(i));
            i += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s.push_str(&frac);
    }
    Some(s)
}

fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let a = cents.unsigned_abs();
    format!("{sign}{}.{:02}", a / 100, a % 100)
}

fn uuid(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn inet(raw: &[u8]) -> Option<String> {
    let mut r = Rd(raw);
    let (family, bits, _is_cidr, n) = (r.u8()?, r.u8()?, r.u8()?, usize::from(r.u8()?));
    let addr = r.take(n)?;
    let (s, max) = match family {
        2 => (std::net::Ipv4Addr::from(<[u8; 4]>::try_from(addr).ok()?).to_string(), 32),
        3 => (std::net::Ipv6Addr::from(<[u8; 16]>::try_from(addr).ok()?).to_string(), 128),
        _ => return None,
    };
    Some(if bits == max { s } else { format!("{s}/{bits}") })
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Days from 1970-01-01 to PostgreSQL's epoch, 2000-01-01.
const PG_EPOCH_DAYS: i64 = 10_957;
const DAY_US: i64 = 86_400_000_000;

fn ymd(days_since_2000: i64) -> (String, bool) {
    let (y, m, d) = civil(days_since_2000 + PG_EPOCH_DAYS);
    if y <= 0 { (format!("{:04}-{m:02}-{d:02}", 1 - y), true) } else { (format!("{y:04}-{m:02}-{d:02}"), false) }
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        d => {
            let (s, bc) = ymd(i64::from(d));
            if bc { format!("{s} BC") } else { s }
        }
    }
}

fn time_of_day(us: i64) -> String {
    let us = us.rem_euclid(DAY_US + 1); // 24:00:00 is a valid time
    let (h, m, s, f) = (us / 3_600_000_000, us / 60_000_000 % 60, us / 1_000_000 % 60, us % 1_000_000);
    if f == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        let frac = format!("{f:06}");
        format!("{h:02}:{m:02}:{s:02}.{}", frac.trim_end_matches('0'))
    }
}

fn offset(east_secs: i32) -> String {
    let sign = if east_secs < 0 { '-' } else { '+' };
    let a = east_secs.unsigned_abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    if s == 0 { format!("{sign}{h:02}:{m:02}") } else { format!("{sign}{h:02}:{m:02}:{s:02}") }
}

fn timestamp(us: i64, utc: bool) -> String {
    match us {
        i64::MAX => "infinity".into(),
        i64::MIN => "-infinity".into(),
        us => {
            let (d, t) = ymd(us.div_euclid(DAY_US));
            let mut s = format!("{d}T{}", time_of_day(us.rem_euclid(DAY_US)));
            if utc {
                s.push('Z');
            }
            if t {
                s.push_str(" BC");
            }
            s
        }
    }
}

/// ISO 8601 duration (`P1Y2M3DT4H5M6.5S`), signs per part as PostgreSQL keeps them.
fn interval(us: i64, days: i32, months: i32) -> String {
    let mut s = String::from("P");
    let (y, mo) = (months / 12, months % 12);
    for (n, u) in [(i64::from(y), 'Y'), (i64::from(mo), 'M'), (i64::from(days), 'D')] {
        if n != 0 {
            let _ = write!(s, "{n}{u}");
        }
    }
    if us != 0 {
        s.push('T');
        let neg = us < 0;
        let a = us.unsigned_abs();
        let (h, m, sec, f) = (a / 3_600_000_000, a / 60_000_000 % 60, a / 1_000_000 % 60, a % 1_000_000);
        let sign = if neg { "-" } else { "" };
        if h != 0 {
            let _ = write!(s, "{sign}{h}H");
        }
        if m != 0 {
            let _ = write!(s, "{sign}{m}M");
        }
        if sec != 0 || f != 0 {
            if f == 0 {
                let _ = write!(s, "{sign}{sec}S");
            } else {
                let frac = format!("{f:06}");
                let _ = write!(s, "{sign}{sec}.{}S", frac.trim_end_matches('0'));
            }
        }
    }
    if s == "P" { "PT0S".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [ndigits.to_be_bytes(), weight.to_be_bytes(), sign.to_be_bytes(), dscale.to_be_bytes()] {
            b.extend_from_slice(&v);
        }
        for d in digits {
            b.extend_from_slice(&d.to_be_bytes());
        }
        b
    }

    #[test]
    fn numerics_are_exact_text() {
        assert_eq!(numeric(&num(2, 0, 0, 2, &[123, 4500])).as_deref(), Some("123.45"));
        assert_eq!(numeric(&num(1, 1, 0x4000, 0, &[1])).as_deref(), Some("-10000"));
        assert_eq!(numeric(&num(1, -1, 0, 4, &[5])).as_deref(), Some("0.0005"));
        assert_eq!(numeric(&num(0, 0, 0, 0, &[])).as_deref(), Some("0"));
        assert_eq!(numeric(&num(0, 0, 0xC000, 0, &[])).as_deref(), Some("NaN"));
        assert_eq!(numeric(&num(1, 0, 0, 0, &[20_000])), None);
        assert_eq!(numeric(&[1]), None);
    }

    #[test]
    fn dates_and_times_are_iso() {
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(date(-1), "1999-12-31");
        assert_eq!(date(8_887), "2024-05-01");
        assert_eq!(date(i32::MAX), "infinity");
        assert_eq!(timestamp(0, true), "2000-01-01T00:00:00Z");
        assert_eq!(timestamp(767_880_000_000_000 + 500_000, false), "2024-05-01T12:00:00.5");
        assert_eq!(timestamp(-1, true), "1999-12-31T23:59:59.999999Z");
        assert!(date(-730_500).ends_with(" BC"), "{}", date(-730_500));
        assert_eq!(time_of_day(DAY_US), "24:00:00");
        assert_eq!(offset(-19_800), "-05:30");
        assert_eq!(interval(0, 0, 0), "PT0S");
        assert_eq!(interval(3_723_500_000, 3, 14), "P1Y2M3DT1H2M3.5S");
        assert_eq!(interval(-60_000_000, 0, 0), "PT-1M");
    }

    #[test]
    fn misc_types() {
        assert_eq!(money(-12_345), "-123.45");
        assert_eq!(uuid(&[0x12; 16]), "12121212-1212-1212-1212-121212121212");
        assert_eq!(inet(&[2, 32, 0, 4, 10, 0, 0, 1]).as_deref(), Some("10.0.0.1"));
        assert_eq!(inet(&[2, 8, 1, 4, 10, 0, 0, 0]).as_deref(), Some("10.0.0.0/8"));
        assert_eq!(decode(&Type::INT4, &7i32.to_be_bytes()), Value::Int(7));
        assert_eq!(decode(&Type::INT4, &[1, 2]), Value::Other("\\x0102".into()));
        assert_eq!(decode(&Type::JSONB, b"\x01{\"a\":1}"), Value::Json("{\"a\":1}".into()));
        assert_eq!(decode(&Type::BIT, &[0, 0, 0, 3, 0b1010_0000]), Value::text("101"));
    }

    #[test]
    fn arrays_nest_and_hostile_ones_do_not_panic() {
        // int4[][] {{1,2},{3,NULL}}
        let mut b = Vec::new();
        for v in [2i32, 1, 23, 2, 1, 2, 1] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        for e in [Some(1i32), Some(2), Some(3), None] {
            match e {
                Some(v) => {
                    b.extend_from_slice(&4i32.to_be_bytes());
                    b.extend_from_slice(&v.to_be_bytes());
                }
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        assert_eq!(decode(&Type::INT4_ARRAY, &b), Value::Json("[[1,2],[3,null]]".into()));
        assert!(matches!(decode(&Type::INT4_ARRAY, &b[..b.len() - 3]), Value::Other(_)));
        let mut huge = Vec::new();
        for v in [1i32, 0, 23, i32::MAX, 1] {
            huge.extend_from_slice(&v.to_be_bytes());
        }
        assert!(matches!(decode(&Type::INT4_ARRAY, &huge), Value::Other(_)));
        // text[] {"a","é"}
        let mut t = Vec::new();
        for v in [1i32, 0, 25, 2, 1] {
            t.extend_from_slice(&v.to_be_bytes());
        }
        for s in ["a", "é"] {
            t.extend_from_slice(&(s.len() as i32).to_be_bytes());
            t.extend_from_slice(s.as_bytes());
        }
        assert_eq!(decode(&Type::TEXT_ARRAY, &t), Value::Json("[\"a\",\"é\"]".into()));
    }
}

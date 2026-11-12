//! Turning result rows into what a chart or a pivot draws. Pure: no egui, no I/O. Rows come from
//! an agent read (already capped at 5,000 and redacted), so every cap here is about what fits on
//! screen, not about memory.

use std::collections::HashMap;

use quarry_core::{ChartKind, Column, Value};

/// Most categories on a bar or line chart's x axis (the rest are dropped and counted).
pub const MAX_CATEGORIES: usize = 60;
/// Most series (distinct values of the `series` column).
pub const MAX_SERIES: usize = 12;
/// Most pivot rows and columns.
pub const MAX_PIVOT_ROWS: usize = 200;
pub const MAX_PIVOT_COLUMNS: usize = 30;

/// The x axis of a chart.
#[derive(Clone, Debug, PartialEq)]
pub enum XAxis {
    /// Discrete labels; a point's x is the label's index.
    Category(Vec<String>),
    /// Numbers; `time` when they are Unix seconds parsed from dates.
    Number { time: bool },
}

/// One line, bar group member or point cloud.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub name: String,
    /// `[x, y]`, sorted by x for lines and areas.
    pub points: Vec<[f64; 2]>,
}

/// What a chart draws.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartData {
    pub kind: ChartKind,
    pub x_label: String,
    pub y_label: String,
    pub x: XAxis,
    pub series: Vec<Series>,
    /// Histogram only: the width of one bin.
    pub bin_width: Option<f64>,
    /// Rows that had no usable x or y (NULL, text where a number was needed, redacted).
    pub skipped: usize,
    /// Categories or series beyond the caps were dropped.
    pub capped: bool,
}

/// A pivot table: `cells[row][column]`.
#[derive(Clone, Debug, PartialEq)]
pub struct PivotData {
    pub row_label: String,
    pub column_label: String,
    pub value_label: String,
    pub agg: Agg,
    pub row_keys: Vec<String>,
    pub column_keys: Vec<String>,
    pub cells: Vec<Vec<Option<f64>>>,
    pub skipped: usize,
    pub capped: bool,
}

/// A pivot aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl Agg {
    pub const NAMES: &'static [&'static str] = &["count", "sum", "avg", "min", "max"];

    pub fn from_name(s: &str) -> Option<Agg> {
        Some(match s {
            "count" => Agg::Count,
            "sum" => Agg::Sum,
            "avg" => Agg::Avg,
            "min" => Agg::Min,
            "max" => Agg::Max,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Agg::Count => "count",
            Agg::Sum => "sum",
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
        }
    }
}

/// The index of column `name` (exact, then case-insensitive), or an error naming the columns.
pub fn column_index(columns: &[Column], name: &str) -> Result<usize, String> {
    columns.iter().position(|c| c.name == name).or_else(|| columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))).ok_or_else(|| {
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).take(40).collect();
        format!("column `{name}` is not in the result (columns: {})", names.join(", "))
    })
}

/// A cell as a number: integers, floats, booleans, numeric text, and dates / times (as Unix
/// seconds, `true` in the second slot).
pub fn number(v: &Value) -> Option<(f64, bool)> {
    match v {
        Value::Int(i) => Some((*i as f64, false)),
        Value::Float(f) if f.is_finite() => Some((*f, false)),
        Value::Bool(b) => Some((f64::from(u8::from(*b)), false)),
        Value::Time(t) => parse_time(t).map(|s| (s, true)),
        Value::Text(t) | Value::Other(t) => match t.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => Some((f, false)),
            _ => parse_time(t).map(|s| (s, true)),
        },
        _ => None,
    }
}

/// A cell as a label (`None` for redacted and binary cells, which never become labels).
pub fn label(v: &Value) -> Option<String> {
    let s = match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => s.to_string(),
        Value::Clipped { preview, .. } => preview.to_string(),
        Value::Bytes { .. } | Value::Redacted { .. } => return None,
    };
    Some(s.chars().take(80).collect())
}

/// `YYYY-MM-DD[(T| )hh:mm[:ss[.fff]]][Z|±hh:mm]` as Unix seconds (UTC; an offset is applied).
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from.checked_add(len)?)?;
        if part.bytes().all(|c| c.is_ascii_digit()) { part.parse().ok() } else { None }
    };
    if b.len() < 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m, d) as f64 * 86_400.0;
    if b.len() == 10 {
        return Some(secs);
    }
    if !matches!(b.get(10), Some(b'T' | b' ')) {
        return None;
    }
    let (hh, mm) = (num(11, 2)?, num(14, 2)?);
    secs += (hh * 3600 + mm * 60) as f64;
    let mut i = 16;
    if b.get(16) == Some(&b':') {
        secs += num(17, 2)? as f64;
        i = 19;
        if b.get(19) == Some(&b'.') {
            let frac: String = s.get(20..)?.chars().take_while(char::is_ascii_digit).collect();
            i = 20 + frac.len();
            if let Ok(f) = format!("0.{frac}").parse::<f64>() {
                secs += f;
            }
        }
    }
    match b.get(i) {
        None | Some(b'Z') => Some(secs),
        Some(sign @ (b'+' | b'-')) => {
            let (oh, om) = (num(i + 1, 2)?, num(i + 4, 2).unwrap_or(0));
            let off = (oh * 3600 + om * 60) as f64;
            Some(if *sign == b'+' { secs - off } else { secs + off })
        }
        _ => None,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` of Unix seconds (axis labels).
pub fn format_date(secs: f64) -> String {
    let days = (secs / 86_400.0).floor() as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Distinct keys in first-seen order, at most `cap` (the rest map to `None`).
struct Keys {
    order: Vec<String>,
    index: HashMap<String, usize>,
    cap: usize,
    capped: bool,
}

impl Keys {
    fn new(cap: usize) -> Self {
        Self { order: Vec::new(), index: HashMap::new(), cap, capped: false }
    }
    fn get(&mut self, k: String) -> Option<usize> {
        if let Some(i) = self.index.get(&k) {
            return Some(*i);
        }
        if self.order.len() >= self.cap {
            self.capped = true;
            return None;
        }
        let i = self.order.len();
        self.index.insert(k.clone(), i);
        self.order.push(k);
        Some(i)
    }
}

impl ChartData {
    /// Build a `kind` chart of `rows` with `x` against `y` (count of rows when `y` is absent, for
    /// bar charts), split by `series`.
    pub fn build(
        kind: ChartKind,
        columns: &[Column],
        rows: &[Vec<Value>],
        x: &str,
        y: Option<&str>,
        series: Option<&str>,
    ) -> Result<ChartData, String> {
        let xi = column_index(columns, x)?;
        let yi = y.map(|y| column_index(columns, y)).transpose()?;
        let si = series.map(|s| column_index(columns, s)).transpose()?;
        let y_label = y.map_or_else(|| "count".to_string(), str::to_string);
        let mut data = ChartData {
            kind,
            x_label: x.to_string(),
            y_label,
            x: XAxis::Number { time: false },
            series: Vec::new(),
            bin_width: None,
            skipped: 0,
            capped: false,
        };
        match kind {
            ChartKind::Histogram => data.histogram(rows, xi)?,
            ChartKind::Scatter => {
                let yi = yi.ok_or("a scatter chart needs `y`")?;
                data.numeric(rows, xi, yi, si, false)?;
            }
            ChartKind::Line | ChartKind::Area => {
                let yi = yi.ok_or("a line or area chart needs `y`")?;
                let numeric_x = rows.iter().filter_map(|r| r.get(xi)).filter(|v| !v.is_null()).all(|v| number(v).is_some());
                if numeric_x {
                    data.numeric(rows, xi, yi, si, true)?;
                } else {
                    data.categorical(rows, xi, Some(yi), si)?;
                }
            }
            ChartKind::Bar => data.categorical(rows, xi, yi, si)?,
            ChartKind::Pivot => return Err("a pivot is not a chart: use viz.pivot".into()),
        }
        if data.series.iter().all(|s| s.points.is_empty()) {
            return Err(format!("nothing to draw: no row has a usable `{}`{}", data.x_label, y.map(|y| format!(" and `{y}`")).unwrap_or_default()));
        }
        Ok(data)
    }

    fn series_index(&mut self, keys: &mut Keys, row: &[Value], si: Option<usize>) -> Option<usize> {
        let name = match si {
            Some(si) => label(row.get(si)?)?,
            None => self.y_label.clone(),
        };
        let i = keys.get(name.clone())?;
        if i == self.series.len() {
            self.series.push(Series { name, points: Vec::new() });
        }
        Some(i)
    }

    fn numeric(&mut self, rows: &[Vec<Value>], xi: usize, yi: usize, si: Option<usize>, sort: bool) -> Result<(), String> {
        let mut keys = Keys::new(MAX_SERIES);
        let mut time = false;
        for row in rows {
            let (Some((x, t)), Some((y, _))) = (row.get(xi).and_then(number), row.get(yi).and_then(number)) else {
                self.skipped += 1;
                continue;
            };
            time |= t;
            match self.series_index(&mut keys, row, si) {
                Some(i) => {
                    if let Some(s) = self.series.get_mut(i) {
                        s.points.push([x, y]);
                    }
                }
                None => self.skipped += 1,
            }
        }
        if sort {
            for s in &mut self.series {
                s.points.sort_by(|a, b| a[0].total_cmp(&b[0]));
            }
        }
        self.capped |= keys.capped;
        self.x = XAxis::Number { time };
        Ok(())
    }

    fn categorical(&mut self, rows: &[Vec<Value>], xi: usize, yi: Option<usize>, si: Option<usize>) -> Result<(), String> {
        let mut cats = Keys::new(MAX_CATEGORIES);
        let mut keys = Keys::new(MAX_SERIES);
        // (series, category) -> summed y (a bar per category and series)
        let mut sums: HashMap<(usize, usize), f64> = HashMap::new();
        for row in rows {
            let Some(xl) = row.get(xi).and_then(label) else {
                self.skipped += 1;
                continue;
            };
            let y = match yi {
                Some(yi) => match row.get(yi).and_then(number) {
                    Some((y, _)) => y,
                    None => {
                        self.skipped += 1;
                        continue;
                    }
                },
                None => 1.0,
            };
            let (Some(c), Some(s)) = (cats.get(xl), self.series_index(&mut keys, row, si)) else {
                self.skipped += 1;
                continue;
            };
            *sums.entry((s, c)).or_insert(0.0) += y;
        }
        for (si, s) in self.series.iter_mut().enumerate() {
            s.points = (0..cats.order.len()).filter_map(|c| sums.get(&(si, c)).map(|y| [c as f64, *y])).collect();
        }
        self.capped |= cats.capped || keys.capped;
        self.x = XAxis::Category(cats.order);
        Ok(())
    }

    fn histogram(&mut self, rows: &[Vec<Value>], xi: usize) -> Result<(), String> {
        let mut values: Vec<f64> = Vec::with_capacity(rows.len());
        let mut time = false;
        for row in rows {
            match row.get(xi).and_then(number) {
                Some((v, t)) => {
                    time |= t;
                    values.push(v);
                }
                None => self.skipped += 1,
            }
        }
        let (Some(lo), Some(hi)) = (values.iter().copied().reduce(f64::min), values.iter().copied().reduce(f64::max)) else {
            return Err(format!("column `{}` has no numbers", self.x_label));
        };
        let bins = ((values.len() as f64).sqrt().ceil() as usize).clamp(5, 40);
        let width = if hi > lo { (hi - lo) / bins as f64 } else { 1.0 };
        let mut counts = vec![0u64; bins];
        for v in values {
            let i = (((v - lo) / width).floor() as usize).min(bins - 1);
            if let Some(c) = counts.get_mut(i) {
                *c += 1;
            }
        }
        self.y_label = "count".into();
        self.bin_width = Some(width);
        self.x = XAxis::Number { time };
        self.series = vec![Series {
            name: "count".into(),
            points: counts.iter().enumerate().map(|(i, c)| [lo + width * (i as f64 + 0.5), *c as f64]).collect(),
        }];
        Ok(())
    }
}

impl PivotData {
    /// Pivot `rows` by the `row`, `column` and `value` columns with `agg`.
    pub fn build(columns: &[Column], rows: &[Vec<Value>], row: &str, column: &str, value: &str, agg: Agg) -> Result<PivotData, String> {
        let (ri, ci, vi) = (column_index(columns, row)?, column_index(columns, column)?, column_index(columns, value)?);
        #[derive(Clone, Copy)]
        struct Acc {
            n: u64,
            sum: f64,
            min: f64,
            max: f64,
        }
        let mut rk = Keys::new(MAX_PIVOT_ROWS);
        let mut ck = Keys::new(MAX_PIVOT_COLUMNS);
        let mut acc: HashMap<(usize, usize), Acc> = HashMap::new();
        let mut skipped = 0;
        for r in rows {
            let (Some(rl), Some(cl), Some(v)) = (r.get(ri).and_then(label), r.get(ci).and_then(label), r.get(vi)) else {
                skipped += 1;
                continue;
            };
            let x = match (agg, number(v)) {
                (Agg::Count, _) if !matches!(v, Value::Null) => 0.0,
                (_, Some((x, _))) => x,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            let (Some(a), Some(b)) = (rk.get(rl), ck.get(cl)) else {
                skipped += 1;
                continue;
            };
            let e = acc.entry((a, b)).or_insert(Acc { n: 0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY });
            e.n += 1;
            e.sum += x;
            e.min = e.min.min(x);
            e.max = e.max.max(x);
        }
        if acc.is_empty() {
            return Err(format!("nothing to pivot: no row has `{row}`, `{column}` and a usable `{value}`"));
        }
        let cells = (0..rk.order.len())
            .map(|a| {
                (0..ck.order.len())
                    .map(|b| {
                        acc.get(&(a, b)).map(|e| match agg {
                            Agg::Count => e.n as f64,
                            Agg::Sum => e.sum,
                            Agg::Avg => e.sum / e.n.max(1) as f64,
                            Agg::Min => e.min,
                            Agg::Max => e.max,
                        })
                    })
                    .collect()
            })
            .collect();
        Ok(PivotData {
            row_label: row.into(),
            column_label: column.into(),
            value_label: value.into(),
            agg,
            capped: rk.capped || ck.capped,
            row_keys: rk.order,
            column_keys: ck.order,
            cells,
            skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<Column> {
        names.iter().map(|n| Column::new(*n, "")).collect()
    }

    #[test]
    fn bar_sums_by_category_and_series() {
        let rows = vec![
            vec![Value::text("paid"), Value::Int(3), Value::text("EUR")],
            vec![Value::text("paid"), Value::Int(4), Value::text("USD")],
            vec![Value::text("refunded"), Value::Int(1), Value::text("EUR")],
            vec![Value::text("paid"), Value::Redacted { len: 3 }, Value::text("EUR")],
        ];
        let d = ChartData::build(ChartKind::Bar, &cols(&["status", "n", "cur"]), &rows, "status", Some("n"), Some("cur")).unwrap();
        assert_eq!(d.x, XAxis::Category(vec!["paid".into(), "refunded".into()]));
        assert_eq!(d.series.len(), 2);
        assert_eq!(d.series[0].points, vec![[0.0, 3.0], [1.0, 1.0]]);
        assert_eq!(d.skipped, 1, "a redacted value is never drawn");
        let counts = ChartData::build(ChartKind::Bar, &cols(&["status", "n", "cur"]), &rows, "status", None, None).unwrap();
        assert_eq!(counts.series[0].points, vec![[0.0, 3.0], [1.0, 1.0]]);
    }

    #[test]
    fn lines_on_dates_sort_and_histograms_bin() {
        let rows =
            vec![vec![Value::Time("2024-01-02".into()), Value::Float(2.0)], vec![Value::Time("2024-01-01T00:00:00Z".into()), Value::Float(1.0)]];
        let d = ChartData::build(ChartKind::Line, &cols(&["day", "v"]), &rows, "day", Some("v"), None).unwrap();
        assert_eq!(d.x, XAxis::Number { time: true });
        assert_eq!(d.series[0].points[0][1], 1.0);
        assert_eq!(format_date(d.series[0].points[1][0]), "2024-01-02");
        let rows: Vec<Vec<Value>> = (0..100).map(|i| vec![Value::Int(i)]).collect();
        let h = ChartData::build(ChartKind::Histogram, &cols(&["x"]), &rows, "x", None, None).unwrap();
        assert_eq!(h.series[0].points.len(), 10);
        assert_eq!(h.series[0].points.iter().map(|p| p[1]).sum::<f64>(), 100.0);
        assert!(ChartData::build(ChartKind::Scatter, &cols(&["x"]), &rows, "x", None, None).is_err());
        assert!(ChartData::build(ChartKind::Bar, &cols(&["x"]), &rows, "nope", None, None).unwrap_err().contains("columns: x"));
    }

    #[test]
    fn times_parse_with_offsets() {
        assert_eq!(parse_time("1970-01-01"), Some(0.0));
        assert_eq!(parse_time("1970-01-01T01:00:00+01:00"), Some(0.0));
        assert_eq!(parse_time("2000-03-01 00:00:00.5"), Some(951_868_800.5));
        assert_eq!(parse_time("2000-13-01"), None);
        assert_eq!(parse_time("hello"), None);
        assert_eq!(format_date(951_868_800.0), "2000-03-01");
    }

    #[test]
    fn pivots_aggregate() {
        let rows = vec![
            vec![Value::text("US"), Value::text("paid"), Value::Int(10)],
            vec![Value::text("US"), Value::text("paid"), Value::Int(20)],
            vec![Value::text("DE"), Value::text("refunded"), Value::Int(5)],
        ];
        let c = cols(&["country", "status", "total"]);
        let avg = PivotData::build(&c, &rows, "country", "status", "total", Agg::Avg).unwrap();
        assert_eq!(avg.row_keys, vec!["US", "DE"]);
        assert_eq!(avg.cells[0], vec![Some(15.0), None]);
        let n = PivotData::build(&c, &rows, "country", "status", "total", Agg::Count).unwrap();
        assert_eq!(n.cells[0][0], Some(2.0));
        assert_eq!(Agg::NAMES.iter().filter_map(|a| Agg::from_name(a)).count(), 5);
    }
}

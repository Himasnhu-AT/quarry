//! How a cell reads in the grid: one capped line, with NULL, binary, JSON and long text marked.

use std::fmt::Write as _;

use quarry_core::Value;

/// Characters of a cell drawn in the grid (the detail popover shows more).
pub const DISPLAY_CHARS: usize = 200;
/// Bytes of a binary cell drawn as hex in the grid.
pub const HEX_BYTES: usize = 12;
/// Characters the detail popover shows.
pub const DETAIL_CHARS: usize = 64 * 1024;

/// How a cell is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub text: String,
    /// Numbers are right-aligned.
    pub right: bool,
    /// NULL, binary sizes, redacted: muted (NULL also italic).
    pub muted: bool,
    pub italic: bool,
    /// More than the line shows: the popover has the rest (JSON, multi-line or long text, bytes).
    pub more: bool,
}

impl Shown {
    fn new(text: String) -> Self {
        Self { text, right: false, muted: false, italic: false, more: false }
    }
}

/// `n` bytes as `512 B`, `1.2 KB`, `3.4 MB`.
pub fn size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    format!("{v:.1} {unit}")
}

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The first line of `s`, whitespace runs folded to one space, capped at `cap` characters.
/// Returns whether anything was left out.
fn one_line(s: &str, cap: usize) -> (String, bool) {
    let mut out = String::with_capacity(s.len().min(cap + 4));
    let mut n = 0;
    let mut space = false;
    let mut cut = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if c == '\n' || c == '\r' {
                cut = true;
            }
            space = !out.is_empty();
            continue;
        }
        if n >= cap {
            cut = true;
            break;
        }
        if space {
            out.push(' ');
            n += 1;
            space = false;
        }
        // control characters would draw as boxes
        out.push(if c.is_control() { '\u{FFFD}' } else { c });
        n += 1;
    }
    if cut {
        out.push('…');
    }
    (out, cut)
}

/// How `v` is drawn in a grid cell.
pub fn shown(v: &Value) -> Shown {
    match v {
        Value::Null => Shown { text: "NULL".into(), right: false, muted: true, italic: true, more: false },
        Value::Bool(b) => Shown::new(b.to_string()),
        Value::Int(i) => Shown { right: true, ..Shown::new(i.to_string()) },
        Value::Float(f) => Shown { right: true, ..Shown::new(f.to_string()) },
        Value::Text(s) | Value::Time(s) | Value::Other(s) => {
            let (text, cut) = one_line(s, DISPLAY_CHARS);
            Shown { more: cut, ..Shown::new(text) }
        }
        Value::Json(s) => {
            let (text, _) = one_line(s, DISPLAY_CHARS);
            Shown { more: true, ..Shown::new(text) }
        }
        Value::Clipped { preview, len } => {
            let (mut text, _) = one_line(preview, DISPLAY_CHARS);
            if !text.ends_with('…') {
                text.push('…');
            }
            let _ = write!(text, " ({})", size(*len));
            Shown { more: true, ..Shown::new(text) }
        }
        Value::Bytes { len, preview } => {
            let head = preview.get(..preview.len().min(HEX_BYTES)).unwrap_or(&[]);
            let more = (head.len() as u64) < *len;
            let text = format!("0x{}{} ({})", hex(head), if more { "…" } else { "" }, size(*len));
            Shown { text, right: false, muted: true, italic: false, more: true }
        }
        Value::Redacted { len } => Shown { text: format!("redacted ({})", size(*len)), right: false, muted: true, italic: true, more: false },
    }
}

/// The plain text of a cell, for copying and comparing: NULL is empty, bytes are `\x` hex of
/// what was fetched, clipped values are their preview.
pub fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) | Value::Json(s) | Value::Time(s) | Value::Other(s) => s.to_string(),
        Value::Clipped { preview, .. } => preview.to_string(),
        Value::Bytes { preview, .. } => format!("\\x{}", hex(preview)),
        Value::Redacted { .. } => String::new(),
    }
}

/// Whether `v` is all of the value (not a preview): only complete values are copied as SQL or
/// used to identify a row.
pub fn complete(v: &Value) -> bool {
    match v {
        Value::Clipped { .. } | Value::Redacted { .. } => false,
        Value::Bytes { len, preview } => preview.len() as u64 == *len,
        _ => true,
    }
}

/// The popover's text: JSON pretty-printed, bytes as a hex dump, text as is (capped).
pub fn detail(v: &Value) -> String {
    let capped = |s: &str| -> String {
        match s.char_indices().nth(DETAIL_CHARS) {
            Some((at, _)) => format!("{}…", s.get(..at).unwrap_or(s)),
            None => s.to_string(),
        }
    };
    match v {
        Value::Null => "NULL".into(),
        Value::Json(s) if s.len() <= DETAIL_CHARS => {
            // depth is bounded by serde_json's recursion limit (128)
            serde_json::from_str::<serde_json::Value>(s).ok().and_then(|j| serde_json::to_string_pretty(&j).ok()).unwrap_or_else(|| s.to_string())
        }
        Value::Bytes { len, preview } => {
            let mut out = String::new();
            for (i, chunk) in preview.chunks(16).enumerate() {
                let ascii: String = chunk.iter().map(|b| if b.is_ascii_graphic() || *b == b' ' { char::from(*b) } else { '.' }).collect();
                let _ = writeln!(out, "{:08x}  {:<47}  {ascii}", i * 16, chunk.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
            }
            if (preview.len() as u64) < *len {
                let _ = write!(out, "… {} of {} shown", size(preview.len() as u64), size(*len));
            }
            out
        }
        Value::Clipped { preview, len } => format!("{}\n\n… clipped: {} of {} fetched", capped(preview), size(preview.len() as u64), size(*len)),
        other => capped(&plain(other)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn cells_read_as_designed() {
        assert_eq!(shown(&Value::Null), Shown { text: "NULL".into(), right: false, muted: true, italic: true, more: false });
        assert!(shown(&Value::Int(-4)).right);
        let s = shown(&Value::text("line one\n  line two"));
        assert_eq!((s.text.as_str(), s.more), ("line one line two…", true));
        let long = "x".repeat(10_000);
        let s = shown(&Value::text(long));
        assert_eq!(s.text.chars().count(), DISPLAY_CHARS + 1);
        let b = shown(&Value::Bytes { len: 4096, preview: Arc::from(&[0xde_u8, 0xad, 0xbe, 0xef][..]) });
        assert_eq!(b.text, "0xdeadbeef… (4.0 KB)");
        let c = shown(&Value::Clipped { preview: "abc".into(), len: 70_000 });
        assert_eq!(c.text, "abc… (68.4 KB)");
        assert_eq!(shown(&Value::text("a\u{7}b")).text, "a\u{FFFD}b");
        assert!(shown(&Value::Json("{\"a\": 1}".into())).more);
    }

    #[test]
    fn detail_pretty_prints_json_and_dumps_bytes() {
        assert_eq!(detail(&Value::Json("{\"a\":[1,2]}".into())), "{\n  \"a\": [\n    1,\n    2\n  ]\n}");
        assert_eq!(detail(&Value::Json("{bad".into())), "{bad");
        let d = detail(&Value::Bytes { len: 100, preview: Arc::from(&b"AB\x00"[..]) });
        assert!(d.starts_with("00000000  41 42 00") && d.contains("AB.") && d.contains("of 100 B"), "{d}");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
        assert!(!complete(&Value::Bytes { len: 100, preview: Arc::from(&b"AB"[..]) }));
        assert!(complete(&Value::Bytes { len: 2, preview: Arc::from(&b"AB"[..]) }));
    }
}

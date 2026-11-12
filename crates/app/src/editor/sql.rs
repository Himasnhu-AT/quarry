//! Pure SQL text helpers for the editor: split a buffer into statements (for ⌘↵ "the statement
//! under the cursor" and ⇧⌘↵ "all"), map a server error position back into the buffer, and a
//! light formatter. These never decide what is safe to run: the person runs their own SQL on
//! their own connection, and the agent path goes through `quarry_guard`.

use std::ops::Range;

/// Longest buffer the helpers look at (bytes); beyond it the text is one statement.
pub const MAX_SCAN: usize = 4 << 20;

/// Byte ranges of the statements in `sql`, `;` separated, ignoring `;` inside quotes, quoted
/// identifiers, dollar quotes and comments. Ranges are trimmed of whitespace; empty statements
/// (and ones that are only comments) are dropped.
pub fn statements(sql: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if sql.len() > MAX_SCAN {
        push_trimmed(sql, 0..sql.len(), &mut out);
        return out;
    }
    let b = sql.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b.get(i).copied() {
            Some(b'\'') => i = skip_quoted(b, i, b'\''),
            Some(b'"') => i = skip_quoted(b, i, b'"'),
            Some(b'`') => i = skip_quoted(b, i, b'`'),
            Some(b'-') if b.get(i + 1) == Some(&b'-') => i = skip_line(b, i),
            Some(b'/') if b.get(i + 1) == Some(&b'*') => i = skip_block(b, i),
            Some(b'$') => i = skip_dollar(sql, i),
            Some(b';') => {
                push_trimmed(sql, start..i, &mut out);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_trimmed(sql, start..sql.len(), &mut out);
    out
}

/// The statement that holds byte offset `at` (or the nearest one before it; the first one when
/// the cursor sits before every statement).
pub fn statement_at(sql: &str, at: usize) -> Option<Range<usize>> {
    let all = statements(sql);
    all.iter()
        .find(|r| r.start <= at && at <= r.end)
        .cloned()
        .or_else(|| all.iter().rev().find(|r| r.end <= at).cloned())
        .or_else(|| all.first().cloned())
}

/// The byte offset of char index `chars` in `s` (clamped to the end).
pub fn byte_of_char(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(b, _)| b)
}

/// The char index of byte offset `byte` in `s` (clamped).
pub fn char_of_byte(s: &str, byte: usize) -> usize {
    s.char_indices().take_while(|(b, _)| *b < byte).count()
}

/// Line and column (1-based) of a server's 1-based character `position` within `statement`,
/// offset by where the statement starts in the buffer.
pub fn line_col(buffer: &str, statement_start: usize, position: u32) -> Option<(usize, usize)> {
    let stmt = buffer.get(statement_start..)?;
    let pos = usize::try_from(position).ok()?.checked_sub(1)?;
    let byte = statement_start + byte_of_char(stmt, pos);
    let before = buffer.get(..byte)?;
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    Some((line, col))
}

fn push_trimmed(sql: &str, r: Range<usize>, out: &mut Vec<Range<usize>>) {
    let Some(text) = sql.get(r.clone()) else { return };
    let lead = text.len() - text.trim_start().len();
    let trail = text.len() - text.trim_end().len();
    let t = r.start + lead..r.end - trail;
    if t.is_empty() {
        return;
    }
    // a statement made only of comments is not a statement
    if sql.get(t.clone()).is_some_and(|s| strip_comments(s).trim().is_empty()) {
        return;
    }
    out.push(t);
}

fn strip_comments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        let next = match b.get(i).copied() {
            Some(b'-') if b.get(i + 1) == Some(&b'-') => skip_line(b, i),
            Some(b'/') if b.get(i + 1) == Some(&b'*') => skip_block(b, i),
            _ => {
                i += 1;
                continue;
            }
        };
        out.push_str(s.get(last..i).unwrap_or(""));
        i = next;
        last = next;
    }
    out.push_str(s.get(last..).unwrap_or(""));
    out
}

fn skip_quoted(b: &[u8], start: usize, q: u8) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b.get(i) == Some(&q) {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn skip_line(b: &[u8], start: usize) -> usize {
    b.iter().skip(start).position(|c| *c == b'\n').map_or(b.len(), |p| start + p + 1)
}

fn skip_block(b: &[u8], start: usize) -> usize {
    // PostgreSQL nests block comments
    let mut depth = 0usize;
    let mut i = start;
    while i < b.len() {
        if b.get(i) == Some(&b'/') && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b.get(i) == Some(&b'*') && b.get(i + 1) == Some(&b'/') {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// `$tag$ … $tag$` (PostgreSQL); a lone `$1` parameter is not a quote.
fn skip_dollar(sql: &str, start: usize) -> usize {
    let rest = sql.get(start + 1..).unwrap_or("");
    let tag_len = rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
    if rest.as_bytes().get(tag_len) != Some(&b'$') || rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return start + 1;
    }
    let delim = sql.get(start..start + tag_len + 2).unwrap_or("$$");
    let body = start + tag_len + 2;
    match sql.get(body..).and_then(|s| s.find(delim)) {
        Some(p) => body + p + delim.len(),
        None => sql.len(),
    }
}

const CLAUSES: &[&str] = &[
    "select",
    "from",
    "where",
    "group by",
    "order by",
    "having",
    "limit",
    "offset",
    "union all",
    "union",
    "except",
    "intersect",
    "left join",
    "right join",
    "inner join",
    "full join",
    "cross join",
    "join",
    "values",
    "set",
    "returning",
    "with",
];

const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "order",
    "by",
    "having",
    "limit",
    "offset",
    "union",
    "all",
    "except",
    "intersect",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "on",
    "using",
    "as",
    "and",
    "or",
    "not",
    "in",
    "is",
    "null",
    "like",
    "ilike",
    "between",
    "case",
    "when",
    "then",
    "else",
    "end",
    "distinct",
    "asc",
    "desc",
    "nulls",
    "first",
    "last",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "returning",
    "with",
    "recursive",
    "exists",
    "true",
    "false",
    "create",
    "table",
    "view",
    "index",
    "drop",
    "alter",
    "explain",
    "analyze",
];

/// A light formatter: keywords upper case, each main clause on its own line. Strings, quoted
/// identifiers and comments are left exactly as they are.
pub fn format(sql: &str) -> String {
    if sql.len() > MAX_SCAN {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 16);
    for (n, r) in statements(sql).into_iter().enumerate() {
        if n > 0 {
            out.push_str(";\n\n");
        }
        out.push_str(&format_one(sql.get(r).unwrap_or("")));
    }
    if sql.trim_end().ends_with(';') {
        out.push(';');
    }
    out.push('\n');
    out
}

fn format_one(s: &str) -> String {
    // tokens: words, quoted runs, comments, whitespace collapsed to one space
    let b = s.as_bytes();
    let mut toks: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b.get(i).copied().unwrap_or(b' ');
        let end = match c {
            b'\'' | b'"' | b'`' => skip_quoted(b, i, c),
            b'-' if b.get(i + 1) == Some(&b'-') => skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => skip_block(b, i),
            b'$' => skip_dollar(s, i),
            c if c.is_ascii_whitespace() => {
                let n = b.iter().skip(i).take_while(|c| c.is_ascii_whitespace()).count();
                toks.push(" ".into());
                i += n;
                continue;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' => i + b.iter().skip(i).take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count(),
            _ => i + s.get(i..).and_then(|r| r.chars().next()).map_or(1, char::len_utf8),
        };
        let tok = s.get(i..end).unwrap_or("");
        let lower = tok.to_ascii_lowercase();
        toks.push(if KEYWORDS.contains(&lower.as_str()) { lower.to_ascii_uppercase() } else { tok.trim_end_matches('\n').to_string() });
        if tok.starts_with("--") {
            toks.push("\n".into());
        }
        i = end.max(i + 1);
    }
    let mut out = String::new();
    let mut k = 0;
    while k < toks.len() {
        let word = |j: usize| toks.get(j).map(|t| t.to_ascii_lowercase()).unwrap_or_default();
        let two = format!("{} {}", word(k), word(k + 2));
        let prev = k.checked_sub(2).map(word).unwrap_or_default();
        let joined = word(k) == "join" && ["left", "right", "inner", "full", "cross", "outer"].contains(&prev.as_str());
        let at_clause =
            k > 0 && !joined && (CLAUSES.contains(&two.as_str()) && toks.get(k + 1).is_some_and(|t| t == " ") || CLAUSES.contains(&word(k).as_str()));
        if at_clause && !out.trim_end().ends_with('(') {
            while out.ends_with(' ') {
                out.pop();
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        let t = toks.get(k).map(String::as_str).unwrap_or("");
        if !(t == " " && (out.is_empty() || out.ends_with('\n'))) {
            out.push_str(t);
        }
        k += 1;
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        statements(sql).into_iter().map(|r| &sql[r]).collect()
    }

    #[test]
    fn splits_on_semicolons_outside_quotes_and_comments() {
        assert_eq!(texts("SELECT 1; SELECT 2"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(texts("SELECT ';' ; SELECT \"a;b\" FROM t;"), ["SELECT ';'", "SELECT \"a;b\" FROM t"]);
        assert_eq!(texts("SELECT 1 -- ; no\n; SELECT /* ; */ 2"), ["SELECT 1 -- ; no", "SELECT /* ; */ 2"]);
        assert_eq!(texts("SELECT $$a;b$$; SELECT $tag$;$tag$"), ["SELECT $$a;b$$", "SELECT $tag$;$tag$"]);
        assert_eq!(texts("SELECT $1; SELECT 'it''s; fine'"), ["SELECT $1", "SELECT 'it''s; fine'"]);
        assert_eq!(texts(" ;; -- only a comment\n ; "), Vec::<&str>::new());
        assert_eq!(texts("SELECT 'unterminated; still one"), ["SELECT 'unterminated; still one"]);
        assert_eq!(texts("/* a /* nested ; */ ; */ SELECT 1"), ["/* a /* nested ; */ ; */ SELECT 1"]);
    }

    #[test]
    fn statement_under_the_cursor() {
        let sql = "SELECT 1;\n\nSELECT 2;\n";
        assert_eq!(statement_at(sql, 0).map(|r| &sql[r]), Some("SELECT 1"));
        assert_eq!(statement_at(sql, 9).map(|r| &sql[r]), Some("SELECT 1"));
        assert_eq!(statement_at(sql, 13).map(|r| &sql[r]), Some("SELECT 2"));
        assert_eq!(statement_at(sql, sql.len()).map(|r| &sql[r]), Some("SELECT 2"));
        assert_eq!(statement_at("", 0), None);
    }

    #[test]
    fn positions_map_to_lines_and_chars_to_bytes() {
        let buf = "SELECT 1;\nSELECT *\nFROM orderz";
        let start = statements(buf)[1].start;
        assert_eq!(line_col(buf, start, 15), Some((3, 6)));
        assert_eq!(line_col(buf, start, 0), None);
        assert_eq!(byte_of_char("é1", 1), 2);
        assert_eq!(char_of_byte("é1", 2), 1);
        assert_eq!(byte_of_char("ab", 9), 2);
    }

    #[test]
    fn formats_keywords_and_clauses_but_not_strings() {
        let f = format("select id, 'from where' as s from orders where id = 1 order by id");
        assert_eq!(f, "SELECT id, 'from where' AS s\nFROM orders\nWHERE id = 1\nORDER BY id\n");
        assert_eq!(format("select 1; select 2;"), "SELECT 1;\n\nSELECT 2;\n");
        let hostile = "x".repeat(10) + "'" + &"\u{1F600}".repeat(5);
        let _ = format(&hostile);
        let _ = statements(&hostile);
    }
}

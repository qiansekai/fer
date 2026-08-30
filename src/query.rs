//! Filter query language, shared by the CLI and the HTTP API.
//!
//! Grammar (whitespace-separated terms, all case-insensitive unless noted):
//!
//! ```text
//! foo                 substring in the basename ("foo" anywhere in the name)
//! *.rs  foo*  a?c*    wildcard glob (`*` any run, `?` one char)
//! ext:rs,txt          extension filter (comma list, no dot)
//! size:>1mb size:<10kb size:1kb-5mb size:1024
//! dm:today|yesterday|thisweek|thismonth|>2026-01-01|<2026-01-01|2026-01-01..2026-02-01|2026-01-01
//! dc:...              same as dm: but on creation time (indexed when available)
//! type:file|dir       only files / only directories
//! hidden: system: readonly: reparse:     boolean attribute filters
//! parent:D:\proj      everything under that directory (prefix match)
//! path:D:\proj\src    full-path prefix match
//! name:pattern        explicit basename match (substring or wildcard)
//! !term               negate a term
//! ```
//!
//! Units: kb = 1024, mb = 1024², gb = 1024³. Dates use the local timezone.
//! Size with no comparator matches the exact byte count; use `>`, `<` or
//! `a-b` ranges for practical filtering.

use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct Query {
    pub include: Vec<Term>,
    pub exclude: Vec<Term>,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub enum Term {
    /// Case-insensitive substring in the basename.
    Name(String),
    /// Case-insensitive substring in the full path (bare token with `\`/`/`).
    PathSubstr(String),
    /// Pure suffix (from `*.rs`-style patterns).
    Suffix(String),
    /// Wildcard glob against the basename.
    NameWild(String),
    /// Wildcard glob against the full path.
    PathWild(String),
    /// `ext:rs,txt`
    Ext(Vec<String>),
    /// `size:` comparison (bytes).
    Size { min: Option<u64>, max: Option<u64> },
    /// `dm:` modification-time range (unix seconds, half-open [min, max)).
    Mtime { min: Option<i64>, max: Option<i64> },
    /// `dc:` creation-time range.
    Ctime { min: Option<i64>, max: Option<i64> },
    /// `type:file|dir`
    IsDir(bool),
    /// Attribute bit filter: bit mask + expected value.
    Flag { bit: u8, on: bool },
    /// `parent:` / `path:` prefix on the full path (lowercased).
    PathPrefix(String),
}

impl Query {
    pub fn parse(input: &str) -> Result<Query> {
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for token in input.split_whitespace() {
            let (negated, body) = match token.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, token),
            };
            if body.is_empty() {
                continue;
            }
            let term = parse_term(body)?;
            if negated {
                exclude.push(term);
            } else {
                include.push(term);
            }
        }
        Ok(Query { include, exclude, raw: input.to_string() })
    }
}

fn parse_term(body: &str) -> Result<Term> {
    if let Some((field, value)) = body.split_once(':') {
        let f = field.to_ascii_lowercase();
        const FIELDS: &[&str] = &[
            "ext", "size", "dm", "dc", "type", "hidden", "system", "readonly", "reparse",
            "parent", "path", "name",
        ];
        if !FIELDS.contains(&f.as_str()) {
            // `D:\proj`-style tokens: the colon is a drive letter, not a field.
            if body.contains('\\') || body.contains('/') {
                return Ok(name_term(body));
            }
            bail!(
                "unknown field '{field}:' (supported: ext size dm dc type hidden system readonly reparse parent path name)"
            );
        }
        if value.is_empty() {
            bail!("empty value for '{field}:'");
        }
        match f.as_str() {
            "ext" => {
                let exts: Vec<String> = value
                    .split(',')
                    .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect();
                if exts.is_empty() {
                    bail!("ext: needs at least one extension");
                }
                Ok(Term::Ext(exts))
            }
            "size" => parse_size(value).map(|(min, max)| Term::Size { min, max }),
            "dm" => parse_date(value).map(|(min, max)| Term::Mtime { min, max }),
            "dc" => parse_date(value).map(|(min, max)| Term::Ctime { min, max }),
            "type" => match value.to_ascii_lowercase().as_str() {
                "file" | "f" => Ok(Term::IsDir(false)),
                "dir" | "d" | "folder" => Ok(Term::IsDir(true)),
                other => bail!("type: expects file|dir, got '{other}'"),
            },
            "hidden" | "system" | "readonly" | "reparse" => {
                let bit = match field.to_ascii_lowercase().as_str() {
                    "hidden" => crate::EntryMeta::FLAG_HIDDEN,
                    "system" => crate::EntryMeta::FLAG_SYSTEM,
                    "readonly" => crate::EntryMeta::FLAG_READONLY,
                    _ => crate::EntryMeta::FLAG_REPARSE,
                };
                let on = match value.to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" => true,
                    "0" | "false" | "no" => false,
                    other => bail!("{field}: expects true|false, got '{other}'"),
                };
                Ok(Term::Flag { bit, on })
            }
            "parent" | "path" => Ok(Term::PathPrefix(value.to_lowercase())),
            "name" => Ok(name_term(value)),
            other => bail!("unknown field '{other}:' (supported: ext size dm dc type hidden system readonly reparse parent path name)"),
        }
    } else {
        Ok(name_term(body))
    }
}

/// Classify a bare pattern: pure suffix, wildcard, or plain substring.
/// A bare token containing `\`/`/` matches the full path (substring semantics,
/// like Everything); `parent:`/`path:` fields use prefix semantics instead.
fn name_term(pattern: &str) -> Term {
    let has_wc = pattern.contains('*') || pattern.contains('?');
    let has_sep = pattern.contains('\\') || pattern.contains('/');
    if has_sep {
        if has_wc {
            Term::PathWild(pattern.to_lowercase())
        } else {
            Term::PathSubstr(pattern.to_lowercase())
        }
    } else if let Some(suffix) = crate::store::try_suffix_literal(pattern) {
        Term::Suffix(suffix.to_lowercase())
    } else if has_wc {
        Term::NameWild(pattern.to_lowercase())
    } else {
        Term::Name(pattern.to_lowercase())
    }
}

fn parse_size(value: &str) -> Result<(Option<u64>, Option<u64>)> {
    let v = value.to_ascii_lowercase();
    if let Some((a, b)) = v.split_once('-') {
        return Ok((Some(parse_bytes(a)?), Some(parse_bytes(b)?)));
    }
    if let Some(rest) = v.strip_prefix('>') {
        return Ok((Some(parse_bytes(rest)?), None));
    }
    if let Some(rest) = v.strip_prefix('<') {
        return Ok((None, Some(parse_bytes(rest)?)));
    }
    let n = parse_bytes(&v)?;
    Ok((Some(n), Some(n.saturating_add(1))))
}

pub fn parse_bytes(s: &str) -> Result<u64> {    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("gb") {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix("mb") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix("kb") {
        (n, 1u64 << 10)
    } else if let Some(n) = s.strip_suffix('b') {
        (n, 1)
    } else {
        (s, 1)
    };
    let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("bad size '{s}' (use e.g. 1kb 500mb 2gb, with >, < or a-b)"))?;
    Ok(n.checked_mul(mult).unwrap_or(u64::MAX))
}

/// Parse a date expression into a half-open unix-seconds range.
fn parse_date(value: &str) -> Result<(Option<i64>, Option<i64>)> {
    let now = chrono::Local::now();
    let day_start = |d: chrono::NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .and_then(|t| t.and_local_timezone(chrono::Local).single())
            .map(|t| t.timestamp())
            .unwrap_or(0)
    };
    let today = now.date_naive();
    match value.to_ascii_lowercase().as_str() {
        "today" => Ok((Some(day_start(today)), Some(day_start(today.succ_opt().unwrap_or(today))))),
        "yesterday" => {
            let d = today.pred_opt().unwrap_or(today);
            Ok((Some(day_start(d)), Some(day_start(today))))
        }
        "thisweek" => Ok((Some(now.timestamp() - 7 * 86400), None)),
        "thismonth" => Ok((Some(now.timestamp() - 30 * 86400), None)),
        _ => {
            if let Some(rest) = value.strip_prefix('>') {
                return Ok((Some(day_start(parse_ymd(rest)?)), None));
            }
            if let Some(rest) = value.strip_prefix('<') {
                return Ok((None, Some(day_start(parse_ymd(rest)?))));
            }
            if let Some((a, b)) = value.split_once("..") {
                let (a, b) = (parse_ymd(a)?, parse_ymd(b)?);
                let end = day_start(b.succ_opt().unwrap_or(b));
                return Ok((Some(day_start(a)), Some(end)));
            }
            let d = parse_ymd(value)?;
            Ok((Some(day_start(d)), Some(day_start(d.succ_opt().unwrap_or(d)))))
        }
    }
}

fn parse_ymd(s: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("bad date '{s}' (expected YYYY-MM-DD)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_terms() {
        let q = Query::parse("foo *.rs ext:jpg,png size:>1mb type:file !temp").unwrap();
        assert_eq!(q.include.len(), 5);
        assert_eq!(q.exclude.len(), 1);
        assert!(matches!(q.include[0], Term::Name(ref s) if s == "foo"));
        assert!(matches!(q.include[1], Term::Suffix(ref s) if s == ".rs"));
        assert!(matches!(q.include[2], Term::Ext(ref e) if e == &vec!["jpg".to_string(), "png".to_string()]));
        assert!(matches!(q.include[4], Term::IsDir(false)));
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_bytes("1kb").unwrap(), 1024);
        assert_eq!(parse_bytes("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_bytes("2gb").unwrap(), 2 * 1024 * 1024 * 1024);
        let (min, max) = parse_size("1kb-2kb").unwrap();
        assert_eq!((min, max), (Some(1024), Some(2048)));
        let (min, max) = parse_size(">1mb").unwrap();
        assert_eq!((min, max), (Some(1024 * 1024), None));
    }

    #[test]
    fn parse_path_terms() {
        let q = Query::parse(r"parent:D:\proj path:D:\proj\src name:main*").unwrap();
        assert!(matches!(q.include[0], Term::PathPrefix(ref p) if p == r"d:\proj"));
        assert!(matches!(q.include[1], Term::PathPrefix(ref p) if p == r"d:\proj\src"));
        assert!(matches!(q.include[2], Term::NameWild(ref p) if p == "main*"));
        // bare token with a separator → full-path substring
        let q = Query::parse(r"D:\proj\src").unwrap();
        assert!(matches!(q.include[0], Term::PathSubstr(ref p) if p == r"d:\proj\src"));
    }

    #[test]
    fn unknown_field_rejected() {
        assert!(Query::parse("bogus:x").is_err());
    }

    #[test]
    fn date_keywords() {
        let q = Query::parse("dm:today dm:thisweek").unwrap();
        assert!(matches!(q.include[0], Term::Mtime { min: Some(_), max: Some(_) }));
        assert!(matches!(q.include[1], Term::Mtime { min: Some(_), max: None }));
        assert!(Query::parse("dm:2026-01-01..2026-01-31").is_ok());
        assert!(Query::parse("dm:garbage").is_err());
    }
}

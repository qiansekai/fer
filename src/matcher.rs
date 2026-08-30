//! Case-insensitive substring / wildcard pattern matching.
//!
//! This module defines the *matching semantics* of the engine (and is used by
//! the test suite as the reference implementation). The hot path in
//! [`crate::store`] implements the same semantics in SQL:
//!
//! * substring  → FTS5 trigram (>= 3 chars) or `instr` (1-2 chars)
//! * wildcards  → `LIKE` (glob `*`/`?` translated to `%`/`_`)
//! * pattern contains `\` or `/` (or `path_mode`) → match the full path,
//!   otherwise match the basename only.

/// Compiled search pattern.
#[derive(Debug, Clone)]
pub struct Matcher {
    kind: Kind,
    target_path: bool,
}

#[derive(Debug, Clone)]
enum Kind {
    /// Case-insensitive substring needle (pre-lowercased).
    Substring(String),
    /// Wildcard tokens; implicit leading/trailing `*` gives substring semantics.
    Glob(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(char),
    Any,  // ?
    Star, // *
}

impl Matcher {
    /// `path_mode=true` matches the full path; otherwise only the basename.
    /// A pattern containing `\` or `/` always switches to full-path mode.
    pub fn new(pattern: &str, path_mode: bool) -> Self {
        let has_sep = pattern.contains('\\') || pattern.contains('/');
        let kind = if has_wildcards(pattern) {
            let mut tokens = tokenize(pattern);
            // Leading `*` only: the glob must match a *suffix* of the target,
            // consistent with the SQL path (`LIKE '<glob>'`, unanchored left,
            // anchored right) and with Everything's shell-style semantics.
            if tokens.first() != Some(&Token::Star) {
                tokens.insert(0, Token::Star);
            }
            Kind::Glob(tokens)
        } else {
            Kind::Substring(pattern.to_lowercase())
        };
        Matcher {
            kind,
            target_path: path_mode || has_sep,
        }
    }

    pub fn matches(&self, full_path: &str) -> bool {
        let target = if self.target_path {
            full_path.to_lowercase()
        } else {
            crate::basename(full_path).to_lowercase()
        };
        match &self.kind {
            Kind::Substring(needle) => target.contains(needle.as_str()),
            Kind::Glob(tokens) => glob_match(tokens, &target),
        }
    }
}

/// Does the pattern use wildcards?
pub fn has_wildcards(p: &str) -> bool {
    p.contains('*') || p.contains('?')
}

fn tokenize(p: &str) -> Vec<Token> {
    let mut out = Vec::with_capacity(p.len());
    let mut last_star = false;
    for c in p.chars().flat_map(char::to_lowercase) {
        match c {
            '*' => {
                if !last_star {
                    out.push(Token::Star);
                }
                last_star = true;
            }
            '?' => {
                out.push(Token::Any);
                last_star = false;
            }
            c => {
                out.push(Token::Literal(c));
                last_star = false;
            }
        }
    }
    out
}

/// Iterative O(tokens × len) glob match — no recursion, no stack-overflow risk
/// on long patterns or long paths.
fn glob_match(tokens: &[Token], target: &str) -> bool {
    let chars: Vec<char> = target.chars().collect();
    let n = chars.len();
    let mut prev = vec![false; n + 1];
    let mut cur = vec![false; n + 1];
    prev[0] = true;
    for tok in tokens {
        match tok {
            Token::Star => {
                cur.fill(false);
                // cur[j] = prev[j] || cur[j-1]  (star eats zero or more chars)
                let mut run = prev[0];
                cur[0] = run;
                for j in 1..=n {
                    if prev[j] {
                        run = true;
                    }
                    cur[j] = run;
                }
            }
            _ => {
                cur.fill(false);
                for j in 1..=n {
                    if prev[j - 1] {
                        cur[j] = match tok {
                            Token::Any => true,
                            Token::Literal(c) => chars[j - 1] == *c,
                            Token::Star => unreachable!(),
                        };
                    }
                }
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_basic() {
        let m = Matcher::new("report", false);
        assert!(m.matches("D:\\docs\\AnnualReport2026.pdf"));
        assert!(!m.matches("D:\\docs\\notes.md"));
    }

    #[test]
    fn case_insensitive_ascii() {
        let m = Matcher::new("AGENTS", false);
        assert!(m.matches("D:\\x\\agents.md"));
        assert!(m.matches("D:\\x\\AGENTS.MD"));
    }

    #[test]
    fn case_insensitive_unicode() {
        let m = Matcher::new("ÉCOLE", false);
        assert!(m.matches("C:\\école.txt"));
    }

    #[test]
    fn cjk_substring() {
        let m = Matcher::new("报告", false);
        assert!(m.matches("D:\\docs\\年度报告.md"));
        assert!(!m.matches("D:\\docs\\readme.txt"));
    }

    #[test]
    fn glob_star() {
        let m = Matcher::new("*.rs", false);
        assert!(m.matches("D:\\proj\\main.rs"));
        assert!(!m.matches("D:\\proj\\main.rss"));
        assert!(!m.matches("D:\\proj\\main.txt"));
    }

    #[test]
    fn glob_question() {
        let m = Matcher::new("a?c.txt", false);
        assert!(m.matches("abc.txt"));
        assert!(!m.matches("ac.txt"));
        assert!(!m.matches("abbc.txt"));
    }

    #[test]
    fn glob_star_mid() {
        let m = Matcher::new("a*c", false);
        assert!(m.matches("ac"));
        assert!(m.matches("abc"));
        assert!(m.matches("aZZZc"));
        assert!(!m.matches("ab"));
        // suffix semantics: the pattern may sit at the end of the basename
        assert!(m.matches("xxabc"));
        assert!(!m.matches("axxbxx")); // 'c' must close the suffix
    }

    #[test]
    fn glob_star_only_matches_everything() {
        let m = Matcher::new("*", false);
        assert!(m.matches("anything at all"));
        assert!(m.matches(""));
    }

    #[test]
    fn empty_pattern_matches_everything() {
        let m = Matcher::new("", false);
        assert!(m.matches("whatever.txt"));
    }

    #[test]
    fn path_mode_auto_on_separator() {
        let m = Matcher::new("src\\main*", false); // separator auto-switches to path mode
        assert!(m.matches("D:\\proj\\src\\main.rs"));
        assert!(!m.matches("D:\\proj\\src2\\main.rs"));
    }

    #[test]
    fn path_mode_flag() {
        let m = Matcher::new("proj", true);
        assert!(m.matches("D:\\proj\\main.rs"));
        let m2 = Matcher::new("proj", false);
        assert!(!m2.matches("D:\\proj\\main.rs")); // basename is "main.rs"
    }
}

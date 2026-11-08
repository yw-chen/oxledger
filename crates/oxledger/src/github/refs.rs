// SPDX-License-Identifier: GPL-2.0-or-later
//! R-G1: issue keys in PR titles, head branch names and commit messages.
//!
//! The pattern is `\b([A-Z][A-Z0-9_]{1,9})-([1-9][0-9]{0,8})\b`, case
//! sensitive. In branch names `/` separates words and, per B28, `_` does
//! too: a key starts where the previous character is not `[A-Za-z0-9]`
//! (so `fix_AUTH-91` and `fix/MY_PROJ-12` both match). Elsewhere the
//! ordinary word boundary applies (`_` is a word character).

/// Commit messages scanned per PR (R-G1).
pub const MAX_COMMITS: usize = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Title,
    Branch,
    Commit,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Title => "title",
            Source::Branch => "branch",
            Source::Commit => "commit",
        }
    }
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Keys in `text`, leftmost first, without duplicates.
pub fn scan(text: &str, source: Source) -> Vec<String> {
    let text: String = if source == Source::Branch {
        text.replace('/', " ")
    } else {
        text.to_string()
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let boundary_before = match i.checked_sub(1).map(|p| chars[p]) {
            None => true,
            Some(p) if source == Source::Branch => !p.is_ascii_alphanumeric(),
            Some(p) => !word(p),
        };
        if boundary_before
            && chars[i].is_ascii_uppercase()
            && let Some((key, end)) = key_at(&chars, i)
        {
            if !out.contains(&key) {
                out.push(key);
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// `[A-Z][A-Z0-9_]{1,9}-[1-9][0-9]{0,8}` at `i`, followed by a boundary.
fn key_at(c: &[char], i: usize) -> Option<(String, usize)> {
    let mut j = i + 1;
    while j < c.len() && (c[j].is_ascii_uppercase() || c[j].is_ascii_digit() || c[j] == '_') {
        j += 1;
    }
    let prefix = j - i;
    if !(2..=10).contains(&prefix) || j >= c.len() || c[j] != '-' {
        return None;
    }
    let d = j + 1;
    if d >= c.len() || !('1'..='9').contains(&c[d]) {
        return None;
    }
    let mut e = d + 1;
    while e < c.len() && c[e].is_ascii_digit() {
        e += 1;
    }
    if e - d > 9 {
        return None;
    }
    if e < c.len() && word(c[e]) {
        return None;
    }
    Some((c[i..e].iter().collect(), e))
}

/// Every key with the sources it came from, in first-seen order.
pub fn collect(title: &str, branch: &str, commits: &[String]) -> Vec<(String, Vec<&'static str>)> {
    let mut out: Vec<(String, Vec<&'static str>)> = Vec::new();
    let mut add = |k: String, s: Source| match out.iter_mut().find(|(x, _)| *x == k) {
        Some((_, v)) => {
            if !v.contains(&s.as_str()) {
                v.push(s.as_str());
            }
        }
        None => out.push((k, vec![s.as_str()])),
    };
    for k in scan(title, Source::Title) {
        add(k, Source::Title);
    }
    for k in scan(branch, Source::Branch) {
        add(k, Source::Branch);
    }
    for m in commits.iter().take(MAX_COMMITS) {
        for k in scan(m, Source::Commit) {
            add(k, Source::Commit);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_commits() {
        assert_eq!(scan("修 AUTH-123 的登入", Source::Title), ["AUTH-123"]);
        assert_eq!(
            scan("WEB-7 refactor, AUTH-1.", Source::Commit),
            ["WEB-7", "AUTH-1"]
        );
        // T-G4.
        for t in [
            "auth-123",
            "XAUTH-123",
            "AUTH-0123",
            "AUTH-123a",
            "A-1",
            "AUTH-1234567890",
            "x_AUTH-1",
        ] {
            let got = scan(t, Source::Title);
            assert!(got.is_empty() || t == "XAUTH-123", "{t}: {got:?}");
        }
        assert_eq!(
            scan("XAUTH-123", Source::Title),
            ["XAUTH-123"],
            "a different key, not AUTH"
        );
        assert_eq!(scan("(AUTH-9)", Source::Title), ["AUTH-9"]);
        assert_eq!(scan("AUTH-9 AUTH-9", Source::Title), ["AUTH-9"]);
        assert_eq!(scan("MY_PROJ-12", Source::Title), ["MY_PROJ-12"]);
    }

    #[test]
    fn branches() {
        assert_eq!(scan("fix/AUTH-91-x", Source::Branch), ["AUTH-91"]);
        assert_eq!(
            scan("fix_AUTH-91", Source::Branch),
            ["AUTH-91"],
            "B28: _ separates"
        );
        assert_eq!(scan("fix/MY_PROJ-12", Source::Branch), ["MY_PROJ-12"]);
        assert!(scan("fixAUTH-91", Source::Branch).is_empty());
    }

    #[test]
    fn sources() {
        let c = vec!["AUTH-1 again".to_string(), "WEB-7 refactor".into()];
        assert_eq!(
            collect("修 AUTH-1", "fix/AUTH-91-x", &c),
            [
                ("AUTH-1".to_string(), vec!["title", "commit"]),
                ("AUTH-91".to_string(), vec!["branch"]),
                ("WEB-7".to_string(), vec!["commit"]),
            ]
        );
    }
}

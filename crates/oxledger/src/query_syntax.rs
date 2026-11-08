// SPDX-License-Identifier: GPL-2.0-or-later
//! The minimal text query syntax (design 03 §11, decided by the user for B2
//! on 2026-11-08): `project:`, `status:`, `tracker:`, `assignee:` and free
//! text, mapped onto the structured search parameters. Saved project tabs
//! store the normalized string (§18 R-T3); running one is an ordinary
//! search with the opener's visibility.
//!
//! Grammar: terms separated by Unicode white space. A term `key:value`
//! whose key (case-insensitive) is one of the four is a condition; the
//! value is a run of non-space characters or a `"…"` quoted string (no
//! escapes, no `"` inside). Any other term is free text, kept in order.
//! Each key at most once, except that `project:` may repeat only to be
//! rejected as a scope error by the caller.

use crate::service::SearchParams;
use serde::Serialize;

pub const KEYS: &[&str] = &["project", "status", "tracker", "assignee"];

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Query {
    /// Every `project:` value in order (normally zero or one).
    pub projects: Vec<String>,
    pub status: Option<String>,
    pub tracker: Option<String>,
    pub assignee: Option<String>,
    /// Free-text words in order.
    pub text: Vec<String>,
}

/// A syntax error at a character offset (0-based) of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub position: usize,
    pub message: &'static str,
}

/// Status values that select a category instead of a status name.
const CATEGORIES: &[&str] = &["open", "todo", "doing", "done"];

pub fn parse(input: &str) -> Result<Query, SyntaxError> {
    let chars: Vec<char> = input.chars().collect();
    let mut q = Query::default();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != ':' && chars[i] != '"' {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let key = word.to_lowercase();
        if i < chars.len() && chars[i] == ':' && KEYS.contains(&key.as_str()) {
            i += 1; // the colon
            let vstart = i;
            let value: String = if i < chars.len() && chars[i] == '"' {
                i += 1;
                let s = i;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                if i == chars.len() {
                    return Err(SyntaxError {
                        position: vstart,
                        message: "unterminated quote",
                    });
                }
                let v: String = chars[s..i].iter().collect();
                i += 1;
                if i < chars.len() && !chars[i].is_whitespace() {
                    return Err(SyntaxError {
                        position: i,
                        message: "a space must follow a quoted value",
                    });
                }
                v.trim().to_string()
            } else {
                let s = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    if chars[i] == '"' {
                        return Err(SyntaxError {
                            position: i,
                            message: "a quote may only start a value",
                        });
                    }
                    i += 1;
                }
                chars[s..i].iter().collect()
            };
            if value.is_empty() {
                return Err(SyntaxError {
                    position: vstart,
                    message: "the condition has no value",
                });
            }
            let slot = match key.as_str() {
                "project" => {
                    q.projects.push(value);
                    continue;
                }
                "status" => &mut q.status,
                "tracker" => &mut q.tracker,
                _ => &mut q.assignee,
            };
            if slot.is_some() {
                return Err(SyntaxError {
                    position: start,
                    message: "each condition may appear once",
                });
            }
            *slot = Some(value);
        } else {
            // Free text: the rest of the term, quotes and colons included
            // (e.g. a URL or `foo:bar`); a lone quote is an error.
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            let term: String = chars[start..i].iter().collect();
            if term.contains('"') {
                return Err(SyntaxError {
                    position: start + term.chars().position(|c| c == '"').unwrap_or(0),
                    message: "quotes are only for condition values",
                });
            }
            q.text.push(term);
        }
    }
    Ok(q)
}

fn quote(v: &str) -> String {
    if v.chars().any(char::is_whitespace) {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

impl Query {
    /// The stored form: conditions in a fixed order, then the free text,
    /// single spaces. Parsing it gives the same query back.
    pub fn normalized(&self) -> String {
        let mut parts: Vec<String> = self
            .projects
            .iter()
            .map(|p| format!("project:{}", quote(p)))
            .collect();
        for (k, v) in [
            ("status", &self.status),
            ("tracker", &self.tracker),
            ("assignee", &self.assignee),
        ] {
            if let Some(v) = v {
                parts.push(format!("{k}:{}", quote(v)));
            }
        }
        parts.extend(self.text.iter().cloned());
        parts.join(" ")
    }

    /// The search this query stands for (one project at most).
    pub fn to_params(&self) -> SearchParams {
        let (status, status_category) = match &self.status {
            Some(s) if CATEGORIES.contains(&s.to_lowercase().as_str()) => {
                (None, Some(s.to_lowercase()))
            }
            other => (other.clone(), None),
        };
        SearchParams {
            q: (!self.text.is_empty()).then(|| self.text.join(" ")),
            project: self.projects.first().cloned(),
            tracker: self.tracker.clone(),
            status,
            status_category,
            assignee: self.assignee.clone(),
            filter: None,
            limit: None,
            cursor: None,
            explain: None,
            fields: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_and_text() {
        let q =
            parse("  login  Status:open project:AUTH tracker:\"Feature request\" crash ").unwrap();
        assert_eq!(q.projects, ["AUTH"]);
        assert_eq!(q.status.as_deref(), Some("open"));
        assert_eq!(q.tracker.as_deref(), Some("Feature request"));
        assert_eq!(q.text, ["login", "crash"]);
        let n = q.normalized();
        assert_eq!(
            n,
            "project:AUTH status:open tracker:\"Feature request\" login crash"
        );
        assert_eq!(parse(&n).unwrap(), q, "normalizing is stable");
        let p = q.to_params();
        assert_eq!(p.status_category.as_deref(), Some("open"));
        assert!(p.status.is_none());
        assert_eq!(p.q.as_deref(), Some("login crash"));
    }

    #[test]
    fn free_text_keeps_colons_and_cjk() {
        let q = parse("https://x.test/a 登入 失敗 foo:bar").unwrap();
        assert_eq!(q.text, ["https://x.test/a", "登入", "失敗", "foo:bar"]);
        assert!(q.projects.is_empty());
        assert_eq!(parse("").unwrap(), Query::default());
    }

    #[test]
    fn errors_have_positions() {
        assert_eq!(parse("status:").unwrap_err().position, 7);
        assert_eq!(parse("a tracker:\"open").unwrap_err().position, 10);
        assert_eq!(parse("status:a status:b").unwrap_err().position, 9);
        assert_eq!(parse("say \"hi\"").unwrap_err().position, 4);
        assert_eq!(parse("tracker:\"a\"b").unwrap_err().position, 11);
        assert_eq!(parse("status:a\"b").unwrap_err().position, 8);
        // project may repeat (the caller rejects it as out of scope).
        assert_eq!(parse("project:A project:B").unwrap().projects, ["A", "B"]);
    }
}

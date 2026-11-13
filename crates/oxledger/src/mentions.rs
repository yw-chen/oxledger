// SPDX-License-Identifier: GPL-2.0-or-later
//! `@login` mentions (design 06 §3.4, M2.0): which users an event mentions.
//!
//! The command engine records them (`issue_event_mentions`, ids only) in the
//! transaction that writes the event; the `mentioned` notification is
//! dispatched from them in M2.1. Rendering (M2.1) uses the same rules, so a
//! mention shows as a chip exactly when it could be recorded.
//!
//! - Syntax (R-MN1): `@` at the start or after a character that is not
//!   `[A-Za-z0-9_.@]`; the login `[A-Za-z0-9][A-Za-z0-9_.-]{0,59}`, not
//!   followed by `@` or `[A-Za-z0-9_-]`. An agent handle such as
//!   `@claude-code@alice` is skipped as a whole (R-MN2); so is an email
//!   address. Trailing dots are sentence punctuation, not part of the login.
//! - Markdown: text inside code spans and blocks, links, images and raw HTML
//!   is not looked at (R-M5). `plain` text is scanned as it is (R-M8).
//! - Who (R-RF3, B-8): active users in the project's assignable list (the set
//!   `GET /projects/{id}/assignees` lists). An exact login first; otherwise a
//!   case-insensitive match counts only when it names one user of that list
//!   (R-MN1, B-39).

use crate::commands::TextFormat;
use anyhow::Result;
use comrak::nodes::NodeValue;
use comrak::{Arena, Options, parse_document};
use sqlx::PgConnection;
use std::collections::BTreeSet;
use uuid::Uuid;

/// Users one event may record (R-MN3); the rest are not recorded.
pub const MAX_MENTIONS: usize = 20;
const MAX_LOGIN: usize = 60;

/// The Markdown dialect of design 06 R-M1, as far as parsing goes.
pub fn markdown_options() -> Options<'static> {
    let mut o = Options::default();
    o.extension.strikethrough = true;
    o.extension.table = true;
    o.extension.autolink = true;
    o.extension.tasklist = true;
    o.extension.footnotes = true;
    o.extension.wikilinks_title_after_pipe = true;
    o
}

/// Candidate logins in `text`, in order, without repeats.
pub fn candidates(text: &str, format: TextFormat) -> Vec<String> {
    let mut out = Vec::new();
    match format {
        TextFormat::Plain => scan(text, None, &mut out),
        TextFormat::Markdown => {
            let arena = Arena::new();
            let root = parse_document(&arena, text, &markdown_options());
            // Runs of text between other inline nodes; code, links, images
            // and raw HTML are skipped with everything inside them. `prev`
            // is the visible character just before the run within the same
            // block, so R-MN1's "what precedes the @" holds across inline
            // boundaries (`**x**@bob`, `[x](u)@bob` are not mentions).
            let mut buf = String::new();
            let mut prev: Option<char> = None;
            let mut skip_until: Option<&comrak::nodes::AstNode> = None;
            let mut skipped_last: Option<char> = None;
            for edge in root.traverse() {
                match edge {
                    comrak::arena_tree::NodeEdge::Start(node) => {
                        let value = &node.data().value;
                        if skip_until.is_some() {
                            if let Some(c) = last_char(value) {
                                skipped_last = Some(c);
                            }
                            continue;
                        }
                        match value {
                            NodeValue::Text(t) => buf.push_str(t),
                            NodeValue::SoftBreak | NodeValue::LineBreak => buf.push('\n'),
                            NodeValue::Code(_) | NodeValue::HtmlInline(_) => {
                                flush(&mut buf, &mut prev, &mut out);
                                prev = last_char(value).or(prev);
                            }
                            NodeValue::Link(_) | NodeValue::Image(_) | NodeValue::WikiLink(_) => {
                                flush(&mut buf, &mut prev, &mut out);
                                skip_until = Some(node);
                                skipped_last = None;
                            }
                            v if v.block() => {
                                // Code and HTML blocks have no children: nothing scanned.
                                flush(&mut buf, &mut prev, &mut out);
                                prev = None;
                            }
                            _ => flush(&mut buf, &mut prev, &mut out),
                        }
                    }
                    comrak::arena_tree::NodeEdge::End(node) => {
                        if let Some(s) = skip_until {
                            if std::ptr::eq(s, node) {
                                skip_until = None;
                                prev = skipped_last.or(prev);
                            }
                            continue;
                        }
                        match &node.data().value {
                            NodeValue::Text(_) | NodeValue::SoftBreak | NodeValue::LineBreak => {}
                            v if v.block() => {
                                flush(&mut buf, &mut prev, &mut out);
                                prev = None;
                            }
                            _ => flush(&mut buf, &mut prev, &mut out),
                        }
                    }
                }
            }
            flush(&mut buf, &mut prev, &mut out);
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|l| seen.insert(l.clone()));
    out
}

fn login_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// The last character a leaf shows (text, code span, inline HTML).
fn last_char(v: &NodeValue) -> Option<char> {
    match v {
        NodeValue::Text(t) => t.chars().last(),
        NodeValue::Code(c) => c.literal.chars().last(),
        NodeValue::HtmlInline(h) => h.chars().last(),
        _ => None,
    }
}

/// Scans `buf` (preceded by `prev`) and starts a new run after it.
fn flush(buf: &mut String, prev: &mut Option<char>, out: &mut Vec<String>) {
    if let Some(c) = buf.chars().last() {
        scan(buf, *prev, out);
        *prev = Some(c);
    }
    buf.clear();
}

/// `http://` or `https://` (any case) at `i`.
fn url_at(chars: &[char], i: usize) -> bool {
    ["http://", "https://"].iter().any(|s| {
        let n = s.chars().count();
        i + n <= chars.len()
            && chars[i..i + n]
                .iter()
                .zip(s.chars())
                .all(|(a, b)| a.to_ascii_lowercase() == b)
    })
}

/// R-MN1 over one run of text; `prev` is the character before it (`None`
/// at the start of a block). A web address is skipped up to the next
/// whitespace (R-M8: `https://x.test/@bob` in plain text is not a mention;
/// Markdown turns it into an autolink, which is skipped anyway).
fn scan(text: &str, prev: Option<char>, out: &mut Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if url_at(&chars, i) {
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            continue;
        }
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        let before = if i == 0 { prev } else { Some(chars[i - 1]) };
        let before_ok =
            !before.is_some_and(|p| p.is_ascii_alphanumeric() || p == '_' || p == '.' || p == '@');
        let start = i + 1;
        let mut end = start;
        if before_ok && start < chars.len() && chars[start].is_ascii_alphanumeric() {
            end = start + 1;
            while end < chars.len() && end - start < MAX_LOGIN && login_char(chars[end]) {
                end += 1;
            }
            let next = chars.get(end).copied();
            let after_ok = !next.is_some_and(|c| {
                c == '@' || c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
            });
            if after_ok {
                let mut login: String = chars[start..end].iter().collect();
                while login.ends_with('.') {
                    login.pop();
                }
                if !login.is_empty() {
                    out.push(login);
                }
            }
        }
        // Skip the whole run (a handle's second `@` must not start a mention).
        i = end.max(start);
        while i < chars.len() && (login_char(chars[i]) || chars[i] == '@') {
            i += 1;
        }
    }
}

/// The users `logins` name in `project`'s assignable list (R-RF3, R-MN1),
/// in the order of `logins`.
pub async fn resolve(
    conn: &mut PgConnection,
    project: Uuid,
    logins: &[String],
) -> Result<Vec<Uuid>> {
    if logins.is_empty() {
        return Ok(Vec::new());
    }
    let lower: Vec<String> = logins.iter().map(|l| l.to_ascii_lowercase()).collect();
    // The same set as describe::assignable (users only, no author).
    let users: Vec<(Uuid, String)> = sqlx::query_as(
        "WITH assignable_members AS (
             SELECT m.principal_id FROM members m
             JOIN member_roles mr ON mr.member_id = m.id
             JOIN roles r ON r.id = mr.role_id AND r.assignable
             WHERE m.project_id = $1
         )
         SELECT p.id, p.login FROM principals p
         WHERE p.status = 'active' AND p.kind = 'user' AND p.login IS NOT NULL
           AND lower(p.login) = ANY($2)
           AND (p.id IN (SELECT principal_id FROM assignable_members)
                OR EXISTS (SELECT 1 FROM group_members gm
                           JOIN principals g ON g.id = gm.group_id AND g.status = 'active'
                           WHERE gm.user_id = p.id
                             AND gm.group_id IN (SELECT principal_id FROM assignable_members)))",
    )
    .bind(project)
    .bind(&lower)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: Vec<Uuid> = Vec::new();
    for (login, low) in logins.iter().zip(&lower) {
        let exact = users.iter().find(|(_, l)| l == login);
        let id = match exact {
            Some((id, _)) => Some(*id),
            None => {
                let mut ci = users.iter().filter(|(_, l)| l.to_ascii_lowercase() == *low);
                match (ci.next(), ci.next()) {
                    (Some((id, _)), None) => Some(*id),
                    _ => None, // none, or more than one: not a mention
                }
            }
        };
        if let Some(id) = id
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    Ok(out)
}

/// Records who `event` mentions (R-MN3): `ids` minus the author and minus
/// `before` (who the rewritten text already mentioned), the first
/// [`MAX_MENTIONS`].
pub async fn record(
    conn: &mut PgConnection,
    event: Uuid,
    author: Uuid,
    ids: &[Uuid],
    before: &[Uuid],
) -> Result<()> {
    let ids: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|i| *i != author && !before.contains(i))
        .take(MAX_MENTIONS)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO issue_event_mentions (event_id, user_id)
         SELECT $1, u FROM unnest($2::uuid[]) AS u ON CONFLICT DO NOTHING",
    )
    .bind(event)
    .bind(&ids)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(s: &str) -> Vec<String> {
        candidates(s, TextFormat::Markdown)
    }

    #[test]
    fn syntax() {
        assert_eq!(md("hi @bob and @carol."), ["bob", "carol"]);
        assert_eq!(md("@bob, @bob again"), ["bob"]);
        assert!(md("@claude-code@alice").is_empty());
        assert!(md("mail email@example.com now").is_empty());
        assert!(md("x@bob").is_empty());
        assert_eq!(md("(@john.doe)"), ["john.doe"]);
        assert!(md("@-x @_y").is_empty());
    }

    #[test]
    fn markdown_skips_code_and_links() {
        assert!(md("`@bob`").is_empty());
        assert!(md("```\n@bob\n```").is_empty());
        assert!(md("[@bob](https://example.com)").is_empty());
        // Raw HTML is shown escaped (R-M2): the tags themselves are not
        // scanned, text between inline tags is ordinary text (R-M5).
        assert!(md("<a title=\"@bob\">").is_empty());
        assert_eq!(md("<b>@bob</b>"), ["bob"]);
        assert_eq!(md("**@bob** and _@carol_"), ["bob", "carol"]);
        assert_eq!(md("- [ ] ask @bob\n"), ["bob"]);
    }

    #[test]
    fn markdown_looks_across_inline_boundaries() {
        // R-MN1: what precedes the @ counts even when it is in another node.
        assert!(md("**x**@bob").is_empty());
        assert!(md("[x](https://example.com)@bob").is_empty());
        assert!(md("`x`@bob").is_empty());
        assert_eq!(md("**x** @bob"), ["bob"]);
        assert_eq!(md("[x](https://example.com), @bob"), ["bob"]);
        // A new block starts fresh.
        assert_eq!(md("x\n\n@bob"), ["bob"]);
        assert_eq!(md("- x\n- @bob\n"), ["bob"]);
    }

    #[test]
    fn plain_scans_everything() {
        assert_eq!(candidates("`@bob`", TextFormat::Plain), ["bob"]);
        assert!(candidates("@claude-code@alice", TextFormat::Plain).is_empty());
    }

    #[test]
    fn plain_skips_web_addresses() {
        let p = |s: &str| candidates(s, TextFormat::Plain);
        assert!(p("see https://x.test/@bob").is_empty());
        assert!(p("see HTTP://x.test/u/@bob/x").is_empty());
        assert_eq!(p("https://x.test/@bob and @carol"), ["carol"]);
        assert!(md("see https://x.test/@bob").is_empty());
    }
}

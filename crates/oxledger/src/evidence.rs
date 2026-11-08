// SPDX-License-Identifier: GPL-2.0-or-later
//! Search evidence (design 03 §3.4, §3.5): which query terms occur in the
//! authorized raw text, how strongly, and which short window to show.
//!
//! Pure functions over one source text. Everything is computed from the text
//! read from SQL after authorization, never from the index, so a stale index
//! term can rank an issue but never produce a snippet. Offsets inside are
//! UTF-8 bytes (from the spanned lexer); everything that leaves this module is
//! in code points, half-open.

use crate::search::{lexemes, query_terms};
use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};

/// `primary`: at least one full-weight term matched. `fallback`: only
/// low-weight terms (CJK unigrams inside a run, identifier segments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Fallback,
    Primary,
}

/// A highlighted range of a returned `text`, in code points, half-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Highlight {
    pub start: usize,
    pub end: usize,
    pub strength: Strength,
}

/// The weighted terms of a query (`search::query_terms`).
pub struct Terms {
    weights: HashMap<String, f32>,
}

impl Terms {
    pub fn new(q: &str) -> Self {
        Terms {
            weights: query_terms(q).into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }
}

/// One occurrence of a query term, in code points of the full source.
#[derive(Debug, Clone)]
struct Occ {
    from: usize,
    to: usize,
    term: String,
    weight: f32,
}

/// The matches of a query in one source text.
#[derive(Debug, Clone)]
pub struct FieldMatch {
    pub strength: Strength,
    /// Highlight ranges in code points of the full source: the union of the
    /// occurrences that count (only primary ones when there is a primary).
    pub spans: Vec<(usize, usize)>,
    occs: Vec<Occ>,
}

/// Code point view of a text: its chars and each char's byte offset.
pub struct CpText {
    pub chars: Vec<char>,
    starts: Vec<usize>,
}

impl CpText {
    pub fn new(text: &str) -> Self {
        let (starts, chars) = text.char_indices().unzip();
        CpText { chars, starts }
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The code point index of a byte offset that is a char boundary (or the end).
    fn cp(&self, byte: usize) -> usize {
        self.starts.partition_point(|&b| b < byte)
    }

    pub fn slice(&self, from: usize, to: usize) -> String {
        self.chars[from..to].iter().collect()
    }

    /// Every start (code point) where `needle` occurs, overlapping allowed.
    pub fn find_all(&self, needle: &[char], within: (usize, usize)) -> Vec<usize> {
        let (lo, hi) = within;
        if needle.is_empty() || hi < lo || hi - lo < needle.len() {
            return Vec::new();
        }
        (lo..=hi - needle.len())
            .filter(|&i| self.chars[i..i + needle.len()] == *needle)
            .collect()
    }
}

/// Step 2–3 of §3.4 for one source: every occurrence of a query term, its
/// strength, and the highlight union (§3.5). `None` when nothing matches.
pub fn match_field(terms: &Terms, text: &str, cp: &CpText) -> Option<FieldMatch> {
    let mut occs: Vec<Occ> = lexemes(text)
        .into_iter()
        .filter_map(|l| {
            terms.weights.get(&l.term).map(|w| Occ {
                from: cp.cp(l.from),
                to: cp.cp(l.to),
                term: l.term,
                weight: *w,
            })
        })
        .collect();
    if occs.is_empty() {
        return None;
    }
    let strength = if occs.iter().any(|o| o.weight >= 1.0) {
        Strength::Primary
    } else {
        Strength::Fallback
    };
    if strength == Strength::Primary {
        occs.retain(|o| o.weight >= 1.0);
    }
    let mut ranges: Vec<(usize, usize)> = occs.iter().map(|o| (o.from, o.to)).collect();
    ranges.sort();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (f, t) in ranges {
        match spans.last_mut() {
            // Overlapping or touching: one highlight, no character twice.
            Some(last) if f <= last.1 => last.1 = last.1.max(t),
            _ => spans.push((f, t)),
        }
    }
    Some(FieldMatch {
        strength,
        spans,
        occs,
    })
}

/// A chosen window, in code points of the full source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: usize,
    pub end: usize,
    /// How good it is: (distinct terms, weight sum ×1000), higher first.
    pub score: (usize, i64),
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '。' | '！' | '？' | '；' | '，' | '、' | '.' | '!' | '?' | ';' | ',' | ':' | '：'
        )
}

/// Characters that attach to the previous one (combining marks, joiners,
/// variation selectors, emoji modifiers and tags). An approximation of
/// grapheme cluster boundaries without a segmentation crate.
fn is_extender(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200C | 0x200D | 0x20D0..=0x20FF
        | 0x302A..=0x302F | 0x3099..=0x309A | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
        | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0xE0100..=0xE01EF)
}

fn joined(chars: &[char], i: usize) -> bool {
    // Is there a grapheme boundary *inside* chars[i-1] | chars[i]?
    is_extender(chars[i])
        || chars[i - 1] == '\u{200D}'
        || (chars[i - 1] == '\r' && chars[i] == '\n')
}

/// Moves a window outwards so it does not cut a grapheme (§3.5 step 2).
fn grapheme_align(chars: &[char], mut s: usize, mut e: usize) -> (usize, usize) {
    while s > 0 && s < chars.len() && joined(chars, s) {
        s -= 1;
    }
    while e > 0 && e < chars.len() && joined(chars, e) {
        e += 1;
    }
    (s, e)
}

const SLACK: usize = 12;

/// Window of about `target` code points around `[from, to)`, nudged to
/// sentence / whitespace boundaries and then to grapheme boundaries.
fn window_around(chars: &[char], from: usize, to: usize, target: usize) -> (usize, usize) {
    let n = chars.len();
    let len = to - from;
    let (mut s, mut e) = if len >= target {
        (from, to)
    } else {
        let pad = (target - len) / 2;
        let s = from.saturating_sub(pad);
        let e = (s + target).min(n).max(to);
        (e.saturating_sub(target).min(s), e)
    };
    // Start right after a boundary, looking back first, then forward (never past the match).
    if s > 0 && !is_boundary(chars[s - 1]) {
        let back = (s.saturating_sub(SLACK)..s)
            .rev()
            .find(|&i| i == 0 || is_boundary(chars[i - 1]));
        let fwd = (s + 1..=from.min(s + SLACK)).find(|&i| is_boundary(chars[i - 1]));
        if let Some(i) = back.or(fwd) {
            s = i;
        }
    }
    // End on a boundary (a sentence end is kept), looking forward first.
    if e < n && !is_boundary(chars[e]) {
        let fwd = (e..(e + SLACK).min(n)).find(|&j| is_boundary(chars[j]));
        let back = (to.max(e.saturating_sub(SLACK))..e)
            .rev()
            .find(|&j| is_boundary(chars[j]));
        if let Some(j) = fwd.or(back) {
            e = j;
        }
    }
    if e < n && matches!(chars[e], '。' | '！' | '？' | '.' | '!' | '?') {
        e += 1;
    }
    let (mut s, mut e) = grapheme_align(chars, s, e);
    while s < from && chars[s].is_whitespace() {
        s += 1;
    }
    while e > to && chars[e - 1].is_whitespace() {
        e -= 1;
    }
    (s, e)
}

fn score(occs: &[Occ], s: usize, e: usize) -> (usize, i64) {
    let mut seen = BTreeSet::new();
    let mut sum = 0i64;
    for o in occs.iter().filter(|o| o.from >= s && o.to <= e) {
        if seen.insert(o.term.as_str()) {
            sum += (o.weight * 1000.0).round() as i64;
        }
    }
    (seen.len(), sum)
}

/// §3.5 step 1–3: up to `max` non-overlapping windows of about `target` code
/// points, best first (most distinct terms, then weight, then earliest).
/// Deterministic for a given query and text.
pub fn windows(cp: &CpText, m: &FieldMatch, target: usize, max: usize) -> Vec<Window> {
    let mut cands: Vec<Window> = m
        .occs
        .iter()
        .map(|o| {
            let (s, e) = window_around(&cp.chars, o.from, o.to, target);
            Window {
                start: s,
                end: e,
                score: score(&m.occs, s, e),
            }
        })
        .collect();
    cands.sort_by(|a, b| b.score.cmp(&a.score).then(a.start.cmp(&b.start)));
    let mut out: Vec<Window> = Vec::new();
    for c in cands {
        if out.len() >= max {
            break;
        }
        if out.iter().all(|w| c.end <= w.start || c.start >= w.end) {
            out.push(c);
        }
    }
    out
}

/// Highlights of `m` inside a window, relative to the window start.
pub fn highlights_in(m: &FieldMatch, w: (usize, usize)) -> Vec<(usize, usize)> {
    m.spans
        .iter()
        .filter(|(f, t)| *t > w.0 && *f < w.1)
        .map(|(f, t)| ((*f).max(w.0) - w.0, (*t).min(w.1) - w.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hl(q: &str, text: &str) -> (Strength, Vec<String>) {
        let cp = CpText::new(text);
        let m = match_field(&Terms::new(q), text, &cp).expect("match");
        (
            m.strength,
            m.spans.iter().map(|(f, t)| cp.slice(*f, *t)).collect(),
        )
    }

    #[test]
    fn overlapping_bigrams_are_one_highlight() {
        assert_eq!(
            hl("登入失敗", "使用者登入失敗了"),
            (Strength::Primary, vec!["登入失敗".to_string()])
        );
    }

    #[test]
    fn primary_suppresses_unigrams() {
        // 輸入's 入 must not light up when 登入 matches.
        assert_eq!(
            hl("登入", "輸入密碼後登入"),
            (Strength::Primary, vec!["登入".to_string()])
        );
    }

    #[test]
    fn colloquial_query_is_fallback() {
        assert_eq!(
            hl("付不了錢", "付款失敗，錢沒有扣到"),
            (Strength::Fallback, vec!["付".to_string(), "錢".to_string()])
        );
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            hl("invalid_grant", "回 invalid_grant 錯誤"),
            (Strength::Primary, vec!["invalid_grant".to_string()])
        );
        assert_eq!(
            hl("invalid_grant", "回 invalid request 錯誤"),
            (Strength::Fallback, vec!["invalid".to_string()])
        );
        assert_eq!(
            hl("invalid_grant invalid", "回 invalid request 錯誤").0,
            Strength::Primary
        );
        // Leading / trailing / doubled joiners are not part of the identifier.
        assert_eq!(
            hl("invalid_grant", "__invalid_grant__").1,
            vec!["invalid_grant".to_string()]
        );
        assert_eq!(hl("a_b", "a__b").1, vec!["a".to_string(), "b".to_string()]);
        // Over 128 bytes: only the segments exist, as plain words (primary).
        let long = format!("{}_x", "a".repeat(200));
        let (s, h) = hl(&long, &long);
        assert_eq!(s, Strength::Primary);
        assert_eq!(h, vec!["a".repeat(200), "x".to_string()]);
    }

    #[test]
    fn windows_do_not_cut_graphemes() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let text = format!("{}登入{}", family.repeat(30), family.repeat(30));
        let cp = CpText::new(&text);
        let m = match_field(&Terms::new("登入"), &text, &cp).unwrap();
        let w = windows(&cp, &m, 20, 1)[0];
        let snippet = cp.slice(w.start, w.end);
        assert!(snippet.contains("登入"));
        assert!(!snippet.starts_with('\u{200D}') && !snippet.ends_with('\u{200D}'));
        assert!(
            snippet.starts_with('👨') && snippet.ends_with('👧'),
            "{snippet}"
        );
        // Combining mark stays with its base; CRLF is not split.
        let text = format!("{}e\u{0301}登入\r\nx", "y".repeat(5));
        let cp = CpText::new(&text);
        let m = match_field(&Terms::new("登入"), &text, &cp).unwrap();
        assert_eq!(grapheme_align(&cp.chars, 6, 9), (5, 9));
        assert_eq!(grapheme_align(&cp.chars, 6, 10), (5, 11));
        let w = windows(&cp, &m, 3, 1)[0];
        assert!(cp.slice(w.start, w.end).contains("登入"));
    }
}

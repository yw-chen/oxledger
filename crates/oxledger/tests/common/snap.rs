// SPDX-License-Identifier: GPL-2.0-or-later
//! Golden (snapshot) test helpers (docs/research/09b G3, G4;
//! docs/impl/testing-playbook.md "Golden 測試").
//!
//! - [`settings`]: every snapshot of a topic lives in
//!   `tests/snapshots/<topic>/<name>.snap`, named explicitly, without the
//!   module prefix (so merging test binaries later does not move them).
//! - [`Normalizer`]: makes a value stable before it is snapshotted, keeping
//!   references intact: the same id is the same label everywhere
//!   (`<id1>`, `<id2>`… in order of first appearance, or a name given with
//!   [`Normalizer::name`]), timestamps become `<ts>`, xid-like keys are
//!   dropped, floats are rounded, set-like arrays are sorted and secrets
//!   are masked. [`Normalizer::assert_no_secrets`] then checks that no
//!   registered secret survived in the output.
//!
//! insta itself compares with `\r\n` → `\n` and trailing whitespace
//! trimmed, so line-ending behaviour is never asserted through a snapshot
//! (09b §2.2): assert it explicitly.
//!
//! The tests of this module are in `tests/t1_test_infra.rs` (one binary,
//! not once per test file that includes `common`).

use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

/// insta settings for one topic: `tests/snapshots/<topic>/`, no module
/// prefix, no expression in the header. Use as
/// `snap::settings("ai").bind(|| insta::assert_json_snapshot!("name", v))`.
pub fn settings(topic: &str) -> insta::Settings {
    assert!(
        !topic.is_empty()
            && topic
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
        "snapshot topic {topic:?}: lower-case ASCII, digits and _"
    );
    let mut s = insta::Settings::clone_current();
    s.set_snapshot_path(format!("snapshots/{topic}"));
    s.set_prepend_module_to_snapshot(false);
    s.set_omit_expression(true);
    s
}

/// See the module docs.
#[derive(Debug, Clone)]
pub struct Normalizer {
    /// Lower-case uuid → label.
    ids: HashMap<String, String>,
    next_id: usize,
    secrets: Vec<String>,
    drop_keys: BTreeSet<String>,
    set_keys: BTreeSet<String>,
    float_digits: i32,
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new()
    }
}

pub const TS: &str = "<ts>";
pub const SECRET: &str = "<secret>";

impl Normalizer {
    /// Drops `xid` and `*_xid` keys, rounds floats to 4 digits.
    pub fn new() -> Self {
        Normalizer {
            ids: HashMap::new(),
            next_id: 0,
            secrets: Vec::new(),
            drop_keys: ["xid".to_string()].into(),
            set_keys: BTreeSet::new(),
            float_digits: 4,
        }
    }

    /// Gives `id` a meaningful label, e.g. `name(alice, "user:alice")` →
    /// `<user:alice>`. Call before normalizing anything that contains it.
    pub fn name(&mut self, id: impl std::fmt::Display, label: &str) -> &mut Self {
        let key = id.to_string().to_ascii_lowercase();
        assert!(is_uuid(&key), "{key} is not a uuid");
        let label = format!("<{label}>");
        assert!(
            !self.ids.values().any(|l| *l == label),
            "label {label} used twice"
        );
        self.ids.insert(key, label);
        self
    }

    /// Masks `s` wherever it occurs (also inside longer strings).
    pub fn secret(&mut self, s: &str) -> &mut Self {
        assert!(s.len() >= 4, "secret {s:?} is too short to mask safely");
        self.secrets.push(s.to_string());
        // Longest first, so a secret containing another is masked whole.
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self
    }

    /// Removes object keys named `key` (besides `xid` and `*_xid`).
    pub fn drop_key(&mut self, key: &str) -> &mut Self {
        self.drop_keys.insert(key.to_string());
        self
    }

    /// Sorts the array under object key `key` (a set: order means nothing).
    pub fn set_key(&mut self, key: &str) -> &mut Self {
        self.set_keys.insert(key.to_string());
        self
    }

    /// Rounds non-integer numbers to `digits` decimals (default 4).
    pub fn float_digits(&mut self, digits: i32) -> &mut Self {
        self.float_digits = digits;
        self
    }

    /// The label of `id` (assigning the next `<idN>` if it has none yet).
    pub fn label(&mut self, id: &str) -> String {
        let key = id.to_ascii_lowercase();
        if let Some(l) = self.ids.get(&key) {
            return l.clone();
        }
        self.next_id += 1;
        let l = format!("<id{}>", self.next_id);
        self.ids.insert(key, l.clone());
        l
    }

    /// Normalizes a JSON value (see the module docs).
    pub fn json(&mut self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Number(n) => {
                if n.is_f64() {
                    let x = n.as_f64().unwrap();
                    let p = 10f64.powi(self.float_digits);
                    let r = (x * p).round() / p;
                    serde_json::Number::from_f64(if r == 0.0 { 0.0 } else { r })
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                } else {
                    v.clone()
                }
            }
            Value::Array(xs) => Value::Array(xs.iter().map(|x| self.json(x)).collect()),
            Value::Object(m) => {
                let mut out = Map::new();
                for (k, x) in m {
                    if self.drop_keys.contains(k) || k.ends_with("_xid") {
                        continue;
                    }
                    let mut x = self.json(x);
                    if self.set_keys.contains(k)
                        && let Value::Array(xs) = &mut x
                    {
                        xs.sort_by_key(|a| a.to_string());
                    }
                    out.insert(self.text(k), x);
                }
                Value::Object(out)
            }
            Value::Null | Value::Bool(_) => v.clone(),
        }
    }

    /// Normalizes text: secrets, uuids and timestamps anywhere in it.
    pub fn text(&mut self, s: &str) -> String {
        let mut s = s.to_string();
        for sec in &self.secrets {
            s = s.replace(sec.as_str(), SECRET);
        }
        let s = self.replace_uuids(&s);
        replace_timestamps(&s)
    }

    fn replace_uuids(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        let mut copied = 0;
        while i + 36 <= b.len() {
            let boundary_before = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            let boundary_after = i + 36 == b.len() || !b[i + 36].is_ascii_alphanumeric();
            if boundary_before
                && boundary_after
                && s.is_char_boundary(i)
                && s.is_char_boundary(i + 36)
                && is_uuid(&s[i..i + 36])
            {
                out.push_str(&s[copied..i]);
                out.push_str(&self.label(&s[i..i + 36]));
                i += 36;
                copied = i;
            } else {
                i += 1;
            }
        }
        out.push_str(&s[copied..]);
        out
    }

    /// Panics if a registered secret is in `out` (run it on the serialized
    /// snapshot: masking is not trusted on its own).
    #[track_caller]
    pub fn assert_no_secrets(&self, out: &str) {
        for sec in &self.secrets {
            assert!(!out.contains(sec.as_str()), "a secret is in the snapshot");
        }
    }

    /// Normalizes `v` and returns it pretty-printed, after
    /// [`Self::assert_no_secrets`] on the result.
    #[track_caller]
    pub fn pretty(&mut self, v: &Value) -> String {
        let s = serde_json::to_string_pretty(&self.json(v)).unwrap();
        self.assert_no_secrets(&s);
        s
    }
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, hex digits of either case.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Replaces RFC 3339 / ISO 8601 date-times (`2026-11-14T01:02:03`, `T` or a
/// space, optional fraction, optional `Z` or `±hh:mm`) with [`TS`]. A bare
/// date stays: dates in fixtures are usually meaningful.
pub fn replace_timestamps(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        if let Some(end) = timestamp_at(b, i)
            && (i == 0 || !b[i - 1].is_ascii_digit())
        {
            out.push_str(&s[copied..i]);
            out.push_str(TS);
            i = end;
            copied = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// The end of a date-time starting at `i`, if there is one.
fn timestamp_at(b: &[u8], i: usize) -> Option<usize> {
    // dddd-dd-ddXdd:dd:dd
    const SHAPE: &[u8] = b"dddd-dd-ddXdd:dd:dd";
    if i + SHAPE.len() > b.len() {
        return None;
    }
    for (k, want) in SHAPE.iter().enumerate() {
        let c = b[i + k];
        let ok = match want {
            b'd' => c.is_ascii_digit(),
            b'X' => c == b'T' || c == b' ',
            w => c == *w,
        };
        if !ok {
            return None;
        }
    }
    let mut j = i + SHAPE.len();
    if j < b.len() && b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit() {
        j += 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
    }
    if j < b.len() && b[j] == b'Z' {
        j += 1;
    } else if j + 3 <= b.len()
        && (b[j] == b'+' || b[j] == b'-')
        && b[j + 1].is_ascii_digit()
        && b[j + 2].is_ascii_digit()
    {
        // `+08:00`, `+0800` or PostgreSQL's text form `+08`.
        j += 3;
        let colon = usize::from(j < b.len() && b[j] == b':');
        if j + colon + 2 <= b.len()
            && b[j + colon].is_ascii_digit()
            && b[j + colon + 1].is_ascii_digit()
        {
            j += colon + 2;
        }
    }
    // Not part of a longer number.
    if j < b.len() && b[j].is_ascii_digit() {
        return None;
    }
    Some(j)
}

/// Lines sorted and joined: a stable text form of a set of rows.
pub fn sorted_lines<I: IntoIterator<Item = String>>(rows: I) -> String {
    let mut v: Vec<String> = rows.into_iter().collect();
    v.sort();
    v.join("\n")
}

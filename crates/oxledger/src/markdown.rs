// SPDX-License-Identifier: GPL-2.0-or-later
//! Markdown and plain text rendering (design 06 §3.2–§3.3, M2.1).
//!
//! Two stages (K1):
//!
//! 1. [`stage1`]: comrak parses the text (R-M1 dialect, [`mentions::markdown_options`]);
//!    the text runs ([`mentions::runs`], the same runs mentions are recorded
//!    from) are searched for issue keys (R-RF1) and `@login` (R-MN1), which
//!    become placeholders `<span data-ox-ref="issue|user|wiki"
//!    data-ox-v="…">original</span>`; links and images are rewritten (R-M3,
//!    R-M4); everything renders with raw HTML **escaped** (R-M2) and then goes
//!    through an ammonia allow list. The result depends only on the text, its
//!    format, whether an agent wrote it and the container, so it is cached
//!    (R-M6). A reader cannot write a placeholder: their `<span>` is shown as
//!    text, and ammonia lets `data-ox-ref` through on `span` with the three
//!    values only.
//! 2. [`Resolver::finish`]: per reader, every placeholder becomes a link or
//!    stays the original text, from fixed templates (R-RF2, R-RF3). An issue
//!    key the reader cannot see, a missing one and one of a missing project
//!    all give the original text byte for byte. Not cached.
//!
//! `plain` text (R-M8) is escaped, line breaks become `<br>`, web addresses
//! become links and keys and mentions placeholders, through the same
//! sanitizer. [`plain_text`] is the text search and notification excerpts
//! use (R-M11).
//!
//! Until M2.2 there is no wiki: a `[[…]]` link always shows its text. Until
//! M2.4 there are no attachments: no image is ever loaded (R-M4 "A2 不做附件
//! 時"), and `img` is not on the allow list at all.

use crate::auth::AuthContext;
use crate::commands::TextFormat;
use crate::error::{ApiError, ErrorCode, api};
use crate::mentions::{self, Item};
use crate::service::{App, read_tx};
use anyhow::Result;
use comrak::html::ChildRendering;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, create_formatter, try_parse_document};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// R-M1: bumped when the dialect or the rules change (a cache key part;
/// also returned to the SPA).
pub const FORMAT_VERSION: u32 = 1;
/// R-M7: the most a render takes; longer text is shown as plain text.
pub const MAX_INPUT_BYTES: usize = 512 * 1024;
/// R-M7: references (issue keys, mentions, wiki links) one text resolves;
/// later ones stay the original text and `refs_truncated` is set.
pub const MAX_REFS: usize = 200;
/// R-M3: in text an agent wrote, a URL longer than this (characters) or
/// with a longer query is not a link.
pub const AGENT_MAX_URL: usize = 200;
pub const AGENT_MAX_QUERY: usize = 100;
/// R-M6: stage-1 cache size and lifetime.
const CACHE_CAP: usize = 10_000;
const CACHE_TTL: Duration = Duration::from_secs(600);
/// A parse taking longer than this is given up and the text is shown as
/// plain text (T-M2-11).
const PARSE_BUDGET: Duration = Duration::from_millis(800);
/// `POST /markdown/preview` per person per minute (§3.1).
pub const PREVIEW_PER_MINUTE: u32 = 60;
/// R-M7 "預覽端點的 text 上限同各欄位上限": the description limit (the
/// largest issue text; notes are smaller).
pub const PREVIEW_MAX_BYTES: usize = crate::describe::MAX_DESCRIPTION_BYTES as usize;

const REL_EXTERNAL: &str = "noopener noreferrer nofollow ugc";
const REL_MAILTO: &str = "nofollow ugc";

// ---------------------------------------------------------------- stage 1

/// What stage 1 found to resolve per reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    /// Issue keys as written (current or old keys).
    pub issues: BTreeSet<String>,
    /// Logins as written (without `@`).
    pub logins: BTreeSet<String>,
}

/// The reader-independent HTML of one text (R-M5 step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage1 {
    pub html: String,
    pub refs: Refs,
    /// More than [`MAX_REFS`] references: the rest are plain text.
    pub truncated: bool,
}

/// What one rendering is for: the cache key (R-M6) without the text hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Source {
    pub format: TextFormat,
    /// The text is an agent's (R-M3): stricter link rules, `data-ox-agent`.
    pub agent: bool,
    /// The container (the issue for a description, the note for a note):
    /// heading and footnote ids get its short id (R-M1, B-54).
    pub container: Uuid,
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct CacheKey {
    hash: [u8; 32],
    format: TextFormat,
    version: u32,
    agent: bool,
    container: Uuid,
}

#[derive(Default)]
struct Cache {
    map: HashMap<CacheKey, (Arc<Stage1>, Instant, u64)>,
    tick: u64,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn cache_get(k: &CacheKey) -> Option<Arc<Stage1>> {
    let mut g = CACHE.lock().ok()?;
    let c = g.get_or_insert_with(Cache::default);
    c.tick += 1;
    let tick = c.tick;
    match c.map.get_mut(k) {
        Some((v, at, used)) if at.elapsed() < CACHE_TTL => {
            *used = tick;
            Some(v.clone())
        }
        Some(_) => {
            c.map.remove(k);
            None
        }
        None => None,
    }
}

fn cache_put(k: CacheKey, v: Arc<Stage1>) {
    let Ok(mut g) = CACHE.lock() else { return };
    let c = g.get_or_insert_with(Cache::default);
    c.tick += 1;
    let tick = c.tick;
    if c.map.len() >= CACHE_CAP {
        // Expired entries first, then the least recently used tenth.
        c.map.retain(|_, (_, at, _)| at.elapsed() < CACHE_TTL);
        if c.map.len() >= CACHE_CAP {
            let mut used: Vec<u64> = c.map.values().map(|e| e.2).collect();
            used.sort_unstable();
            let cut = used[CACHE_CAP / 10];
            c.map.retain(|_, e| e.2 > cut);
        }
    }
    c.map.insert(k, (v, Instant::now(), tick));
}

/// Stage-1 entries cached now (tests).
pub fn cache_len() -> usize {
    CACHE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|c| c.map.len()))
        .unwrap_or(0)
}

/// Stage 1 of `text` (cached, R-M6).
pub fn stage1(text: &str, src: Source) -> Arc<Stage1> {
    let key = CacheKey {
        hash: Sha256::digest(text.as_bytes()).into(),
        format: src.format,
        version: FORMAT_VERSION,
        agent: src.agent,
        container: src.container,
    };
    if let Some(v) = cache_get(&key) {
        return v;
    }
    let v = Arc::new(render_stage1(text, src));
    cache_put(key, v.clone());
    v
}

/// Stage 1 without the cache.
pub fn render_stage1(text: &str, src: Source) -> Stage1 {
    let short = short_id(src.container);
    // T-M2-11: one budget for all of stage 1 (parse, references, format,
    // sanitize); past it the text is shown escaped, nothing more.
    let deadline = Instant::now() + PARSE_BUDGET;
    let out = if text.len() > MAX_INPUT_BYTES {
        render_plain(text, src.agent, &short, deadline)
    } else {
        match src.format {
            TextFormat::Plain => render_plain(text, src.agent, &short, deadline),
            TextFormat::Markdown => render_markdown(text, src.agent, &short, deadline)
                .or_else(|| render_plain(text, src.agent, &short, deadline)),
        }
    };
    out.unwrap_or_else(|| render_escaped(text))
}

/// The last resort (over budget): the text escaped, line breaks kept, no
/// links and no references. Safe as built, so not sanitized again.
fn render_escaped(text: &str) -> Stage1 {
    let mut html = String::with_capacity(text.len() + 16);
    html.push_str("<p>");
    for c in text.chars() {
        match c {
            '\r' => {}
            '\n' => html.push_str("<br>\n"),
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '"' => html.push_str("&quot;"),
            '\'' => html.push_str("&#x27;"),
            c => html.push(c),
        }
    }
    html.push_str("</p>");
    Stage1 {
        html,
        refs: Refs::default(),
        // References were not looked for.
        truncated: true,
    }
}

/// Output larger than this many times the input (plus a floor) is not
/// rendered (T-M2-11: `[^1] ` repeated grows forty-fold).
const MAX_GROWTH: usize = 8;
const GROWTH_FLOOR: usize = 64 * 1024;
/// More footnote references than this: plain text (each one renders a
/// superscript link and a back reference).
const MAX_FOOTNOTE_REFS: usize = 500;
/// More headings than this: no heading ids (comrak's id deduplication is
/// quadratic in equal headings).
const MAX_HEADING_IDS: usize = 200;

fn too_big(html: &str, text: &str) -> bool {
    html.len() > MAX_GROWTH * text.len() + GROWTH_FLOOR
}

/// 8 hex characters naming a container in ids (`h-<short>-…`).
fn short_id(container: Uuid) -> String {
    hex::encode(&Sha256::digest(container.as_bytes())[..4])
}

/// HTML-escapes text or an attribute value.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#x27;"),
            c => o.push(c),
        }
    }
    o
}

/// One reference found in a run (character offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    start: usize,
    end: usize,
    kind: &'static str,
    value: String,
}

/// Web addresses in `chars` (R-M8, and the mention scan's rule): from
/// `http://` / `https://` to the next whitespace.
fn url_ranges(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if mentions::url_at(chars, i) {
            let s = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            out.push((s, i));
        } else {
            i += 1;
        }
    }
    out
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Issue keys (R-RF1) and mentions (R-MN1) in one run, leftmost first, not
/// overlapping; `prev` is the character before the run.
fn find_refs(text: &str, prev: Option<char>) -> Vec<Hit> {
    let chars: Vec<char> = text.chars().collect();
    let urls = url_ranges(&chars);
    // The ranges are sorted and disjoint, and `i` only grows: one pass.
    let mut ui = 0usize;
    let mut in_url = |i: usize| {
        while ui < urls.len() && urls[ui].1 <= i {
            ui += 1;
        }
        ui < urls.len() && urls[ui].0 <= i
    };
    let mut hits: Vec<Hit> = mentions::spans(text, prev)
        .into_iter()
        .map(|s| Hit {
            start: s.start,
            end: s.end,
            kind: "user",
            value: s.login,
        })
        .collect();
    let mut i = 0;
    while i < chars.len() {
        let before = if i == 0 { prev } else { Some(chars[i - 1]) };
        if !in_url(i)
            && !before.is_some_and(word)
            && chars[i].is_ascii_uppercase()
            && let Some((key, end)) = crate::github::refs::key_at(&chars, i)
        {
            hits.push(Hit {
                start: i,
                end,
                kind: "issue",
                value: key,
            });
            i = end;
            continue;
        }
        i += 1;
    }
    // A mention starts at its `@`, before the key it might contain.
    hits.sort_by_key(|h| (h.start, h.kind != "user"));
    let mut out: Vec<Hit> = Vec::new();
    for h in hits {
        if out.last().is_none_or(|l| l.end <= h.start) {
            out.push(h);
        }
    }
    out
}

fn open_ref(kind: &str, value: &str) -> String {
    format!(
        r#"<span data-ox-ref="{}" data-ox-v="{}">"#,
        kind,
        esc(value)
    )
}

/// Where a link goes (R-M2, R-M3), as a browser would resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dest {
    /// `#…` on the same page (footnotes, heading anchors).
    Fragment(String),
    /// A path on this site: one `/`, then not `/` or `\`.
    Internal(String),
    External {
        href: String,
        host: String,
    },
    Mailto(String),
}

/// `None`: not a link (`javascript:`, `data:`, other schemes, relative
/// paths without a leading `/`): the text stays, the link goes.
fn dest(raw: &str) -> Option<Dest> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('#') {
        return Some(Dest::Fragment(t.to_string()));
    }
    let external = |u: &url::Url| -> Option<Dest> {
        match u.scheme() {
            "http" | "https" => Some(Dest::External {
                href: u.as_str().to_string(),
                host: u.host_str().unwrap_or_default().to_string(),
            }),
            "mailto" => Some(Dest::Mailto(u.as_str().to_string())),
            _ => None,
        }
    };
    match url::Url::parse(t) {
        Ok(u) => external(&u),
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            // Resolve as a browser would: `//evil.example` and
            // `/\evil.example` are other hosts.
            const BASE: &str = "https://ox.invalid/";
            let base = url::Url::parse(BASE).ok()?;
            let u = base.join(t).ok()?;
            if u.host_str() != Some("ox.invalid") || u.port().is_some() {
                return external(&u);
            }
            if !t.starts_with('/') {
                return None;
            }
            let mut p = u.path().to_string();
            if let Some(q) = u.query() {
                p.push('?');
                p.push_str(q);
            }
            if let Some(f) = u.fragment() {
                p.push('#');
                p.push_str(f);
            }
            // Normalizing can leave `//host` or `/\host` (`/.//evil.example`,
            // `/a/..//x`): a browser reads that path as another host.
            if p.starts_with("//") || p.starts_with("/\\") {
                let u = base.join(&p).ok()?;
                if u.host_str() == Some("ox.invalid") && u.port().is_none() {
                    return None;
                }
                return external(&u);
            }
            Some(Dest::Internal(p))
        }
        Err(_) => None,
    }
}

/// R-M3: agent text with a long URL or query gets no link.
fn too_long_for_agent(raw: &str) -> bool {
    let t = raw.trim();
    if t.chars().count() > AGENT_MAX_URL {
        return true;
    }
    let q = match url::Url::parse(t) {
        Ok(u) => u.query().map(|q| q.chars().count()),
        Err(_) => url::Url::parse("https://ox.invalid/")
            .ok()
            .and_then(|b| b.join(t).ok())
            .and_then(|u| u.query().map(|q| q.chars().count())),
    };
    q.is_some_and(|n| n > AGENT_MAX_QUERY)
}

/// The opening `<a>` for a link to `raw` (`class`: `ox-ext` for text links,
/// `ox-img-ext` for an image shown as a link), or `None` when it must not be
/// a link.
fn link_open(raw: &str, title: &str, agent: bool, image: bool) -> Option<String> {
    let d = dest(raw)?;
    if agent && !matches!(d, Dest::Fragment(_)) && too_long_for_agent(raw) {
        return None;
    }
    let mut a = String::from("<a href=\"");
    let agent_attr = if agent { r#" data-ox-agent="""# } else { "" };
    match &d {
        Dest::Fragment(h) | Dest::Internal(h) => {
            a.push_str(&esc(h));
            a.push('"');
            if image {
                a.push_str(r#" class="ox-img-ext""#);
            }
        }
        Dest::External { href, host } => {
            let class = if image { "ox-img-ext" } else { "ox-ext" };
            let _ = write!(
                a,
                r#"{}" target="_blank" rel="{REL_EXTERNAL}" class="{class}" data-ox-host="{}"{agent_attr}"#,
                esc(href),
                esc(host)
            );
        }
        Dest::Mailto(href) => {
            let _ = write!(a, r#"{}" rel="{REL_MAILTO}"{agent_attr}"#, esc(href));
            if image {
                a.push_str(r#" class="ox-img-ext""#);
            }
        }
    }
    if !title.is_empty() {
        let _ = write!(a, r#" title="{}""#, esc(title));
    }
    a.push('>');
    Some(a)
}

/// The host an image link names (for the 「外部圖片：說明（主機）」 text).
fn image_host(raw: &str) -> Option<String> {
    match dest(raw)? {
        Dest::External { host, .. } => Some(host),
        _ => None,
    }
}

#[derive(Default)]
struct FmtCtx {
    agent: bool,
}

create_formatter!(Fmt<FmtCtx>, {
    NodeValue::Link(ref nl) => |context, entering| {
        let open = link_open(&nl.url, &nl.title, context.user.agent, false);
        if let Some(tag) = open {
            if entering {
                context.write_str(&tag)?;
            } else {
                context.write_str("</a>")?;
            }
        }
    },
    // R-M4: no image is loaded (no attachments before M2.4): a text link
    // 「外部圖片：說明（主機）」, or just the text when it cannot be a link.
    NodeValue::Image(ref nl) => |context, node, entering| {
        if entering {
            let alt = node.collect_text();
            let label = match image_host(&nl.url) {
                Some(h) if !h.is_empty() => format!("外部圖片：{alt}（{h}）"),
                _ => format!("外部圖片：{alt}"),
            };
            match link_open(&nl.url, &nl.title, context.user.agent, true) {
                Some(tag) => {
                    context.write_str(&tag)?;
                    context.write_str(&esc(&label))?;
                    context.write_str("</a>")?;
                }
                None => {
                    context.write_str(r#"<span class="ox-img-ext">"#)?;
                    context.write_str(&esc(&label))?;
                    context.write_str("</span>")?;
                }
            }
            return Ok(ChildRendering::Skip);
        }
    },
    // R-RF4: resolved per reader in stage 2.
    NodeValue::WikiLink(ref nwl) => |context, entering| {
        if entering {
            context.write_str(&open_ref("wiki", &nwl.url))?;
        } else {
            context.write_str("</span>")?;
        }
    },
});

/// Replaces `node` by its children (a wiki link past [`MAX_REFS`]).
fn unwrap<'a>(node: &'a AstNode<'a>) {
    let kids: Vec<&AstNode> = node.children().collect();
    for k in kids {
        node.insert_before(k);
    }
    node.detach();
}

/// Rewrites one run with placeholders for `hits`.
fn splice<'a>(arena: &'a Arena<'a>, run: &mentions::Run<'a>, hits: &[Hit]) {
    let Some(anchor) = run.nodes.first().copied() else {
        return;
    };
    // Each character of the run, with the break node it came from.
    let mut chars: Vec<(char, Option<NodeValue>)> = Vec::new();
    for n in &run.nodes {
        match &n.data().value {
            NodeValue::Text(t) => chars.extend(t.chars().map(|c| (c, None))),
            v => chars.push(('\n', Some(v.clone()))),
        }
    }
    let add = |v: NodeValue| anchor.insert_before(arena.alloc(v.into()));
    let mut text = String::new();
    let flush = |text: &mut String| {
        if !text.is_empty() {
            add(NodeValue::Text(Cow::Owned(std::mem::take(text))));
        }
    };
    let mut i = 0;
    let mut h = hits.iter().peekable();
    while i < chars.len() {
        if let Some(hit) = h.peek()
            && hit.start == i
        {
            flush(&mut text);
            add(NodeValue::Raw(open_ref(hit.kind, &hit.value)));
            let inner: String = chars[hit.start..hit.end].iter().map(|c| c.0).collect();
            add(NodeValue::Text(Cow::Owned(inner)));
            add(NodeValue::Raw("</span>".into()));
            i = hit.end;
            h.next();
            continue;
        }
        match &chars[i].1 {
            Some(v) => {
                flush(&mut text);
                add(v.clone());
            }
            None => text.push(chars[i].0),
        }
        i += 1;
    }
    flush(&mut text);
    for n in &run.nodes {
        n.detach();
    }
}

fn markdown_render_options(short: &str) -> comrak::Options<'static> {
    let mut o = mentions::markdown_options();
    o.extension.header_id_prefix = Some(format!("h-{short}-"));
    o.extension.header_id_prefix_in_href = true;
    // R-M2: raw HTML is shown as text.
    o.render.escape = true;
    o
}

fn render_markdown(text: &str, agent: bool, short: &str, deadline: Instant) -> Option<Stage1> {
    let late = || Instant::now() >= deadline;
    let arena = Arena::new();
    let mut opts = markdown_render_options(short);
    let root = try_parse_document(&arena, text, &opts, &late).ok()?;
    if late() || too_deep(root) {
        return None;
    }
    let (mut footnotes, mut headings) = (0usize, 0usize);
    for n in root.descendants() {
        match n.data().value {
            NodeValue::FootnoteReference(_) => footnotes += 1,
            NodeValue::Heading(_) => headings += 1,
            _ => {}
        }
    }
    if footnotes > MAX_FOOTNOTE_REFS {
        return None;
    }
    if headings > MAX_HEADING_IDS {
        opts.extension.header_id_prefix = None;
    }
    let mut refs = Refs::default();
    let mut count = 0usize;
    let mut truncated = false;
    for item in mentions::runs(root) {
        if late() {
            return None;
        }
        match item {
            Item::Wiki(node) => {
                count += 1;
                if count > MAX_REFS {
                    truncated = true;
                    unwrap(node);
                }
            }
            Item::Run(run) => {
                let hits = find_refs(&run.text(), run.prev);
                if hits.is_empty() {
                    continue;
                }
                let mut keep = Vec::new();
                for h in hits {
                    count += 1;
                    if count > MAX_REFS {
                        truncated = true;
                        continue;
                    }
                    match h.kind {
                        "issue" => refs.issues.insert(h.value.clone()),
                        _ => refs.logins.insert(h.value.clone()),
                    };
                    keep.push(h);
                }
                if !keep.is_empty() {
                    splice(&arena, &run, &keep);
                }
            }
        }
    }
    let mut html = String::new();
    // Not the validating `format_document`: placeholders are `Raw` nodes,
    // which comrak's tree check does not expect in table cells.
    Fmt::format_document_with_plugins(
        root,
        &opts,
        &mut html,
        &comrak::options::Plugins::default(),
        FmtCtx { agent },
    )
    .ok()?;
    if late() || too_big(&html, text) {
        return None;
    }
    let html = sanitize(&html, short);
    if late() {
        return None;
    }
    Some(Stage1 {
        html,
        refs,
        truncated,
    })
}

/// Nesting deeper than this renders as plain text (T-M2-11): rendering and
/// sanitizing cost grows with the square of the depth, and no real text
/// nests a hundred blocks deep.
const MAX_DEPTH: usize = 100;

fn too_deep<'a>(root: &'a AstNode<'a>) -> bool {
    let mut depth = 0usize;
    for edge in root.traverse() {
        match edge {
            comrak::arena_tree::NodeEdge::Start(_) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return true;
                }
            }
            comrak::arena_tree::NodeEdge::End(_) => depth = depth.saturating_sub(1),
        }
    }
    false
}

/// R-M8: a `plain` text.
fn render_plain(text: &str, agent: bool, short: &str, deadline: Instant) -> Option<Stage1> {
    let late = || Instant::now() >= deadline;
    if late() {
        return None;
    }
    let chars: Vec<char> = text.chars().filter(|c| *c != '\r').collect();
    let text: String = chars.iter().collect();
    let urls = url_ranges(&chars);
    let mut refs = Refs::default();
    let mut truncated = false;
    let mut hits: Vec<Hit> = Vec::new();
    let found = find_refs(&text, None);
    if late() {
        return None;
    }
    for h in found {
        if hits.len() >= MAX_REFS {
            truncated = true;
            break;
        }
        match h.kind {
            "issue" => refs.issues.insert(h.value.clone()),
            _ => refs.logins.insert(h.value.clone()),
        };
        hits.push(h);
    }
    let mut html = String::from("<p>");
    let push_char = |html: &mut String, c: char| match c {
        '\n' => html.push_str("<br>\n"),
        c => html.push_str(&esc(c.encode_utf8(&mut [0; 4]))),
    };
    let mut i = 0;
    let mut h = hits.iter().peekable();
    let mut u = urls.iter().peekable();
    let mut steps = 0usize;
    while i < chars.len() {
        steps += 1;
        if steps.is_multiple_of(4096) && late() {
            return None;
        }
        // A reference that ran over an address's start (`@bobhttp://x`)
        // leaves that address behind: skip it, keep the ones after.
        while let Some((s, _)) = u.peek()
            && *s < i
        {
            u.next();
        }
        while let Some(hit) = h.peek()
            && hit.start < i
        {
            h.next();
        }
        if let Some((s, e)) = u.peek().copied()
            && *s == i
        {
            // Trailing punctuation is the sentence's, not the address's.
            let mut end = *e;
            while end > i + 1
                && matches!(
                    chars[end - 1],
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '\'' | '"' | '>'
                )
            {
                end -= 1;
            }
            let url: String = chars[i..end].iter().collect();
            match link_open(&url, "", agent, false) {
                Some(tag) => {
                    html.push_str(&tag);
                    html.push_str(&esc(&url));
                    html.push_str("</a>");
                }
                None => html.push_str(&esc(&url)),
            }
            for c in &chars[end..*e] {
                push_char(&mut html, *c);
            }
            i = *e;
            u.next();
            continue;
        }
        if let Some(hit) = h.peek()
            && hit.start == i
        {
            let inner: String = chars[hit.start..hit.end].iter().collect();
            html.push_str(&open_ref(hit.kind, &hit.value));
            html.push_str(&esc(&inner));
            html.push_str("</span>");
            i = hit.end;
            h.next();
            continue;
        }
        push_char(&mut html, chars[i]);
        i += 1;
    }
    html.push_str("</p>");
    if late() || too_big(&html, &text) {
        return None;
    }
    let html = sanitize(&html, short);
    if late() {
        return None;
    }
    Some(Stage1 {
        html,
        refs,
        truncated,
    })
}

/// R-M2: the ammonia allow list (defence in depth after the escaping).
fn sanitize(html: &str, short: &str) -> String {
    let tags: HashSet<&str> = [
        "p",
        "br",
        "hr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "strong",
        "em",
        "del",
        "code",
        "pre",
        "blockquote",
        "ul",
        "ol",
        "li",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
        "a",
        "input",
        "sup",
        "section",
        "span",
        "div",
    ]
    .into_iter()
    .collect();
    let set = |v: &[&'static str]| -> HashSet<&'static str> { v.iter().copied().collect() };
    let mut attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
    attrs.insert(
        "a",
        set(&[
            "href",
            "title",
            "target",
            "rel",
            "id",
            "data-ox-host",
            "data-ox-agent",
        ]),
    );
    attrs.insert("span", set(&["title", "data-ox-ref", "data-ox-v"]));
    for h in ["h1", "h2", "h3", "h4", "h5", "h6", "li"] {
        attrs.insert(h, set(&["id"]));
    }
    attrs.insert("ol", set(&["start"]));
    attrs.insert("th", set(&["align"]));
    attrs.insert("td", set(&["align"]));
    attrs.insert("input", set(&["type", "checked", "disabled"]));
    let mut classes: HashMap<&str, HashSet<&str>> = HashMap::new();
    classes.insert(
        "a",
        set(&["ox-ext", "ox-img-ext", "anchor", "footnote-backref"]),
    );
    classes.insert("span", set(&["ox-img-ext"]));
    classes.insert("section", set(&["footnotes"]));
    classes.insert("sup", set(&["footnote-ref"]));
    let short = short.to_string();
    let mut b = ammonia::Builder::empty();
    b.tags(tags)
        .clean_content_tags(set(&["script", "style"]))
        .tag_attributes(attrs)
        .allowed_classes(classes)
        .url_schemes(set(&["http", "https", "mailto"]))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(None)
        .strip_comments(true)
        // R-M2: an input is only ever a disabled checkbox.
        .set_tag_attribute_value("input", "disabled", "")
        .attribute_filter(move |el, attr, value| filter_attr(&short, el, attr, value));
    b.clean(html).to_string()
}

/// R-M2's attribute rules beyond the allow list; also gives footnote ids
/// and links the container prefix (R-M1, B-54).
fn filter_attr<'u>(short: &str, el: &str, attr: &str, value: &'u str) -> Option<Cow<'u, str>> {
    let fn_prefix = |v: &str| -> Option<String> {
        for p in ["fnref-", "fn-"] {
            if let Some(rest) = v.strip_prefix(p) {
                return Some(format!("{p}{short}-{rest}"));
            }
        }
        None
    };
    match (el, attr) {
        (_, "id") => {
            if value.starts_with(&format!("h-{short}-")) {
                Some(value.into())
            } else {
                fn_prefix(value).map(Cow::Owned)
            }
        }
        ("a", "href") => match value.strip_prefix('#').and_then(fn_prefix) {
            Some(v) => Some(Cow::Owned(format!("#{v}"))),
            None => Some(value.into()),
        },
        ("a", "target") => (value == "_blank").then_some(value.into()),
        ("a", "rel") => (value == REL_EXTERNAL || value == REL_MAILTO).then_some(value.into()),
        ("a", "data-ox-agent") => Some("".into()),
        ("span", "data-ox-ref") => {
            matches!(value, "issue" | "user" | "wiki").then_some(value.into())
        }
        ("input", "type") => (value == "checkbox").then_some(value.into()),
        ("input", "checked") => Some("".into()),
        ("th" | "td", "align") => {
            matches!(value, "left" | "center" | "right").then_some(value.into())
        }
        ("ol", "start") => value.parse::<u32>().ok().map(|_| value.into()),
        _ => Some(value.into()),
    }
}

// ---------------------------------------------------------------- plain text

/// R-M11: the text search indexes and notification excerpts show: Markdown
/// syntax removed, link text and code kept, link targets dropped; `plain`
/// text as it is.
pub fn plain_text(text: &str, format: TextFormat) -> String {
    if format == TextFormat::Plain || text.len() > MAX_INPUT_BYTES {
        return text.to_string();
    }
    let arena = Arena::new();
    let opts = mentions::markdown_options();
    let deadline = Instant::now() + PARSE_BUDGET;
    let Ok(root) = try_parse_document(&arena, text, &opts, &|| Instant::now() >= deadline) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len());
    let block_break = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };
    for edge in root.traverse() {
        match edge {
            comrak::arena_tree::NodeEdge::Start(n) => match &n.data().value {
                NodeValue::Text(t) => out.push_str(t),
                NodeValue::Code(c) => out.push_str(&c.literal),
                NodeValue::CodeBlock(c) => {
                    block_break(&mut out);
                    out.push_str(&c.literal);
                }
                NodeValue::HtmlInline(h) => out.push_str(h),
                NodeValue::HtmlBlock(h) => {
                    block_break(&mut out);
                    out.push_str(&h.literal);
                }
                NodeValue::SoftBreak | NodeValue::LineBreak => out.push('\n'),
                NodeValue::TableCell => out.push(' '),
                v if v.block() => block_break(&mut out),
                _ => {}
            },
            comrak::arena_tree::NodeEdge::End(n) => {
                if n.data().value.block() {
                    block_break(&mut out);
                }
            }
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------- stage 2

/// What a reader may see of the references of some texts (R-RF2, R-RF3).
#[derive(Debug, Default)]
pub struct Resolver {
    /// Written key → (current key, subject, closed), for issues the reader
    /// can see only.
    issues: HashMap<String, (String, String, bool)>,
    /// Written login → (display name, login), for users in the project's
    /// assignable list.
    users: HashMap<String, (String, String)>,
}

impl Resolver {
    /// Looks up every reference of `docs` once. `project`: the project the
    /// texts belong to (mentions name its assignable list).
    pub async fn load(
        conn: &mut PgConnection,
        caller: AuthContext,
        project: Uuid,
        docs: &[&Stage1],
    ) -> Result<Resolver> {
        let mut keys: BTreeSet<String> = BTreeSet::new();
        let mut logins: BTreeSet<String> = BTreeSet::new();
        for d in docs {
            keys.extend(d.refs.issues.iter().cloned());
            logins.extend(d.refs.logins.iter().cloned());
        }
        let mut r = Resolver::default();
        if !keys.is_empty() {
            let scope = crate::db::load_scope(&mut *conn, caller).await?;
            let keys: Vec<String> = keys.into_iter().collect();
            #[allow(clippy::type_complexity)]
            let rows: Vec<(
                String,
                String,
                String,
                bool,
                Uuid,
                Uuid,
                bool,
                Uuid,
                Option<Uuid>,
            )> = sqlx::query_as(
                "SELECT rk.key, ck.key, i.subject, s.category = 'done',
                            i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
                     FROM issue_keys rk
                     JOIN issues i ON i.id = rk.issue_id AND i.archived_at IS NULL
                     JOIN issue_keys ck ON ck.issue_id = i.id AND ck.is_current
                     JOIN issue_statuses s ON s.id = i.status_id
                     WHERE rk.key = ANY($1)",
            )
            .bind(&keys)
            .fetch_all(&mut *conn)
            .await?;
            for (written, current, subject, closed, p, t, private, author, assignee) in rows {
                let facts = ox_core::IssueFacts {
                    project_id: p,
                    tracker_id: t,
                    is_private: private,
                    author_id: author,
                    assigned_to_id: assignee,
                };
                if scope.can_see(&facts) {
                    r.issues.insert(written, (current, subject, closed));
                }
            }
        }
        if !logins.is_empty() {
            let logins: Vec<String> = logins.into_iter().collect();
            for (written, u) in mentions::resolve_named(&mut *conn, project, &logins).await? {
                r.users.insert(written, (u.name, u.login));
            }
        }
        Ok(r)
    }

    /// Stage 2 of one text: every placeholder as this reader sees it.
    pub fn finish(&self, s: &Stage1) -> String {
        const OPEN: &str = "<span data-ox-ref=\"";
        let html = &s.html;
        let mut out = String::with_capacity(html.len());
        let mut rest = html.as_str();
        while let Some(at) = rest.find(OPEN) {
            out.push_str(&rest[..at]);
            let after = &rest[at + OPEN.len()..];
            let Some(parsed) = parse_placeholder(after) else {
                // Not ours after all: keep it as it is (it passed ammonia).
                out.push_str(OPEN);
                rest = after;
                continue;
            };
            let (kind, value, inner, len) = parsed;
            match kind {
                "issue" => match self.issues.get(value) {
                    Some((key, subject, closed)) => {
                        let class = if *closed {
                            "ox-issue ox-closed"
                        } else {
                            "ox-issue"
                        };
                        let _ = write!(
                            out,
                            r#"<a class="{class}" href="/issue/{}" title="{}">{inner}</a>"#,
                            esc(key),
                            esc(subject)
                        );
                    }
                    None => out.push_str(inner),
                },
                "user" => match self.users.get(value) {
                    Some((name, login)) => {
                        let _ = write!(
                            out,
                            r#"<span class="ox-mention" title="{}">@{}</span>"#,
                            esc(login),
                            esc(name)
                        );
                    }
                    None => out.push_str(inner),
                },
                // R-RF4: no wiki before M2.2: the link's text.
                _ => out.push_str(inner),
            }
            rest = &after[len..];
        }
        out.push_str(rest);
        out
    }
}

/// `kind" data-ox-v="value">inner</span>` → (kind, value, inner, length).
fn parse_placeholder(s: &str) -> Option<(&'static str, &str, &str, usize)> {
    let q = s.find('"')?;
    let kind = match &s[..q] {
        "issue" => "issue",
        "user" => "user",
        "wiki" => "wiki",
        _ => return None,
    };
    let s2 = s[q..].strip_prefix("\" data-ox-v=\"")?;
    let v_end = s2.find('"')?;
    let value = &s2[..v_end];
    let s3 = s2[v_end..].strip_prefix("\">")?;
    let close = s3.find("</span>")?;
    let inner = &s3[..close];
    let len = s.len() - s3.len() + close + "</span>".len();
    Some((kind, value, inner, len))
}

/// Whether an issue's description is an agent's (R-M3): the event that last
/// set it was an agent's write, or applied a proposal an agent made (even
/// when a person approved it). A person's rewrite makes it a person's.
///
/// `pp.kind = 'agent'` is the approved-proposal case. Migration 0032's
/// CHECK makes every `agent_direct` and external-agent event
/// `actor_kind = 'agent'`, so today the first test alone covers those; the
/// proposer test stays for approvals (and in case that CHECK changes).
pub async fn description_by_agent(conn: &mut PgConnection, issue: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM issue_field_origins o
             JOIN issue_events e ON e.id = o.origin_event_id
             LEFT JOIN principals pp ON pp.id = e.proposer_id
             WHERE o.issue_id = $1 AND o.field_key = 'description'
               AND (e.actor_kind = 'agent' OR pp.kind = 'agent'))",
    )
    .bind(issue)
    .fetch_one(&mut *conn)
    .await?)
}

/// One text to render for a reader.
#[derive(Debug, Clone, Copy)]
pub struct Text<'a> {
    pub text: &'a str,
    pub src: Source,
}

/// Both stages of `texts` (one lookup for all of them), for `caller`.
pub async fn render_for(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
    texts: &[Text<'_>],
) -> Result<Vec<String>> {
    let s1: Vec<Arc<Stage1>> = texts.iter().map(|t| stage1(t.text, t.src)).collect();
    let refs: Vec<&Stage1> = s1.iter().map(|s| s.as_ref()).collect();
    let r = Resolver::load(&mut *conn, caller, project, &refs).await?;
    Ok(s1.iter().map(|s| r.finish(s)).collect())
}

// ---------------------------------------------------------------- preview

/// `POST /markdown/preview` (§3.1, R-RF6).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkdownPreviewInput {
    /// What the text belongs to: `{"issue": "AUTH-1"}` (a description or
    /// note of that issue) or `{"project": "AUTH"}` (a new issue).
    /// `{"wiki_project": …}` answers `not_found` until the wiki exists (M2.2).
    pub context: MarkdownPreviewContext,
    pub text: String,
    /// `markdown` (default) or `plain`.
    pub format: Option<TextFormat>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkdownPreviewContext {
    /// An issue key.
    pub issue: Option<String>,
    /// A project id or key.
    pub project: Option<String>,
    /// A project id or key whose wiki the page is in.
    pub wiki_project: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MarkdownPreviewResult {
    /// Sanitized HTML, references resolved for the caller; put it in the
    /// page as it is (the only `{@html}` source, R-M10).
    pub html: String,
    /// The rendering rules' version (1 in M2.1); it changes when the same
    /// text would render differently.
    pub format_version: u32,
    /// More than 200 references: the rest are shown as text (R-M7).
    pub refs_truncated: bool,
}

/// Previews per person in the current minute.
static PREVIEWS: Mutex<Option<HashMap<Uuid, (Instant, u32)>>> = Mutex::new(None);

fn preview_allowed(who: Uuid) -> std::result::Result<(), u64> {
    let Ok(mut g) = PREVIEWS.lock() else {
        return Ok(());
    };
    let m = g.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    if m.len() > 10_000 {
        m.retain(|_, (at, _)| now.duration_since(*at) < Duration::from_secs(60));
    }
    let e = m.entry(who).or_insert((now, 0));
    if now.duration_since(e.0) >= Duration::from_secs(60) {
        *e = (now, 0);
    }
    if e.1 >= PREVIEW_PER_MINUTE {
        let left = Duration::from_secs(60).saturating_sub(now.duration_since(e.0));
        return Err(left.as_millis().max(1) as u64);
    }
    e.1 += 1;
    Ok(())
}

/// The same code as the stored texts' rendering (R-RF6): nothing is written
/// and nobody is notified. A context the caller cannot see is `not_found`
/// exactly like a missing one.
pub async fn preview(
    app: &App,
    caller: AuthContext,
    input: &MarkdownPreviewInput,
) -> Result<MarkdownPreviewResult> {
    let who = caller.principal_id().unwrap_or_default();
    if let Err(ms) = preview_allowed(who) {
        return Err(ApiError::new(
            ErrorCode::RateLimited,
            "too many previews right now; wait a moment",
        )
        .with_details(serde_json::json!({"kind": "markdown_preview", "retry_after_ms": ms}))
        .with_retry_after(ms)
        .into());
    }
    let c = &input.context;
    let mut tx = read_tx(app).await?;
    // Visibility first (02 §6.7), then the size.
    let (project, container) = match (&c.issue, &c.project, &c.wiki_project) {
        (Some(key), None, None) => {
            let (id, _) = crate::service::get_in(&mut tx, caller, key).await?;
            let p: Uuid = sqlx::query_scalar("SELECT project_id FROM issues WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            (p, id)
        }
        (None, Some(p), None) => {
            let id = crate::describe::visible_project(&mut tx, caller, p).await?;
            (id, id)
        }
        (None, None, Some(_)) => {
            return Err(api(ErrorCode::NotFound, "no such wiki"));
        }
        _ => {
            return Err(ApiError::new(
                ErrorCode::InvalidArgument,
                "context takes exactly one of issue, project, wiki_project",
            )
            .with_details(serde_json::json!({"path": "context"}))
            .into());
        }
    };
    if input.text.len() > PREVIEW_MAX_BYTES {
        return Err(ApiError::new(
            ErrorCode::PayloadTooLarge,
            format!("text is at most {PREVIEW_MAX_BYTES} bytes"),
        )
        .with_details(serde_json::json!({"limit": PREVIEW_MAX_BYTES, "path": "text"}))
        .into());
    }
    let src = Source {
        format: input.format.unwrap_or(TextFormat::Markdown),
        agent: false,
        container,
    };
    let s1 = stage1(&input.text, src);
    let r = Resolver::load(&mut tx, caller, project, &[s1.as_ref()]).await?;
    let html = r.finish(&s1);
    tx.commit().await?;
    Ok(MarkdownPreviewResult {
        html,
        format_version: FORMAT_VERSION,
        refs_truncated: s1.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(s: &str) -> Stage1 {
        render_stage1(
            s,
            Source {
                format: TextFormat::Markdown,
                agent: false,
                container: Uuid::nil(),
            },
        )
    }

    #[test]
    fn placeholders() {
        let s = md("see AUTH-1 and @bob.");
        assert!(
            s.html
                .contains(r#"<span data-ox-ref="issue" data-ox-v="AUTH-1">AUTH-1</span>"#),
            "{}",
            s.html
        );
        assert!(
            s.html
                .contains(r#"<span data-ox-ref="user" data-ox-v="bob">@bob</span>."#),
            "{}",
            s.html
        );
        assert_eq!(s.refs.issues.iter().collect::<Vec<_>>(), ["AUTH-1"]);
        assert_eq!(s.refs.logins.iter().collect::<Vec<_>>(), ["bob"]);
    }

    #[test]
    fn dest_rules() {
        assert_eq!(dest("javascript:alert(1)"), None);
        assert_eq!(dest("JaVaScRiPt:alert(1)"), None);
        assert_eq!(dest("java\tscript:alert(1)"), None);
        assert_eq!(dest("data:text/html,x"), None);
        assert_eq!(dest("foo/bar"), None);
        assert!(
            matches!(dest("//evil.example/x"), Some(Dest::External { host, .. }) if host == "evil.example")
        );
        assert!(
            matches!(dest("/\\evil.example/x"), Some(Dest::External { host, .. }) if host == "evil.example")
        );
        assert_eq!(
            dest("/issue/A-1"),
            Some(Dest::Internal("/issue/A-1".into()))
        );
        assert_eq!(
            dest("/attachments/x/content/../y"),
            Some(Dest::Internal("/attachments/x/y".into()))
        );
    }

    /// R-M2 defence in depth: the second layer alone keeps only the three
    /// reference kinds, whatever the first layer let through.
    #[test]
    fn sanitizer_limits_reference_kinds() {
        for kind in ["issue", "user", "wiki"] {
            let h = sanitize(
                &format!(r#"<span data-ox-ref="{kind}" data-ox-v="v">x</span>"#),
                "s",
            );
            assert!(
                h.contains(&format!(r#"data-ox-ref="{kind}""#)),
                "{kind}: {h}"
            );
        }
        for kind in ["script", "Issue", "user ", "", "html", "issue\"x"] {
            let h = sanitize(
                &format!(
                    r#"<span data-ox-ref="{}" data-ox-v="v">x</span>"#,
                    esc(kind)
                ),
                "s",
            );
            assert!(!h.contains("data-ox-ref"), "{kind:?}: {h}");
        }
        // Not on other elements.
        let h = sanitize(r#"<a href="/x" data-ox-ref="issue">x</a>"#, "s");
        assert!(!h.contains("data-ox-ref"), "{h}");
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.1 (Markdown): the two-stage rendering (R-M1–R-M8, R-RF1–
//! R-RF6), the `*_html` fields, `POST /markdown/preview`, mention delivery
//! (R-MN5) and plain-text search (R-M11). T-M2-01…03a, 05, 06, 08…14,
//! 15…19, 21. Wiki (T-M2-04), images (07, 07a) and import (20) come with
//! M2.2, M2.4 and M2.5.
//!
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use common::test_pool;
use http_body_util::BodyExt;
use ox_core::Pred;
use oxledger::agents::{self, PolicyInput};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::commands::{self, TextFormat};
use oxledger::error::{ErrorCode, classify};
use oxledger::issue_view;
use oxledger::markdown::{
    self, MarkdownPreviewContext as PreviewContext, MarkdownPreviewInput as PreviewInput, Source,
    Text,
};
use oxledger::notifications::{self, ListQuery, NotificationItem, PreferencesPut, ReadAllInput};
use oxledger::policy::PolicyMode;
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::search::{Layout, SearchIndex};
use oxledger::service::{self, App, SearchParams};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

/// Every database test resets the database (World::build): one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------- HTML parsing

/// One tag of serialized HTML.
#[derive(Debug, Clone)]
struct Tag {
    name: String,
    close: bool,
    attrs: Vec<(String, String)>,
}

impl Tag {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let r = &rest[at..];
        let Some(end) = r.find(';') else {
            out.push_str(r);
            return out;
        };
        let ent = &r[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "quot" => Some('"'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "nbsp" => Some('\u{a0}'),
            "#39" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &r[end + 1..];
            }
            None => {
                out.push('&');
                rest = &r[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The tags of `html` as the sanitizer serialized it: every `<` in text is
/// `&lt;`, every attribute value is double-quoted with `"` as `&quot;`. A
/// tag that does not follow those rules fails the test (it would mean the
/// output did not come from the serializer).
fn tags(html: &str) -> Vec<Tag> {
    let c: Vec<char> = html.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        if c[i] != '<' {
            i += 1;
            continue;
        }
        i += 1;
        let close = c.get(i) == Some(&'/');
        if close {
            i += 1;
        }
        let mut name = String::new();
        while i < c.len() && !c[i].is_whitespace() && c[i] != '>' && c[i] != '/' {
            name.push(c[i]);
            i += 1;
        }
        assert!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric()),
            "odd tag name {name:?} in {html}"
        );
        let mut attrs = Vec::new();
        loop {
            while i < c.len() && (c[i].is_whitespace() || c[i] == '/') {
                i += 1;
            }
            assert!(i < c.len(), "unterminated tag in {html}");
            if c[i] == '>' {
                i += 1;
                break;
            }
            let mut k = String::new();
            while i < c.len() && !c[i].is_whitespace() && c[i] != '=' && c[i] != '>' {
                k.push(c[i]);
                i += 1;
            }
            let mut v = String::new();
            if c.get(i) == Some(&'=') {
                i += 1;
                assert_eq!(c.get(i), Some(&'"'), "unquoted value of {k} in {html}");
                i += 1;
                while i < c.len() && c[i] != '"' {
                    v.push(c[i]);
                    i += 1;
                }
                assert!(i < c.len(), "unterminated value in {html}");
                i += 1;
            }
            attrs.push((k.to_ascii_lowercase(), unescape(&v)));
        }
        out.push(Tag {
            name: name.to_ascii_lowercase(),
            close,
            attrs,
        });
    }
    out
}

fn open_tags<'a>(t: &'a [Tag], name: &str) -> Vec<&'a Tag> {
    t.iter().filter(|t| !t.close && t.name == name).collect()
}

const ALLOWED_TAGS: &[&str] = &[
    "a",
    "p",
    "br",
    "hr",
    "em",
    "strong",
    "del",
    "code",
    "pre",
    "blockquote",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "span",
    "sup",
    "section",
    "input",
];

const ALLOWED_CLASSES: &[&str] = &[
    // Stage 2 (resolved references).
    "ox-issue",
    "ox-closed",
    "ox-mention",
    "ox-ext",
    "ox-img-ext",
    "anchor",
    "footnote-backref",
    "footnotes",
    "footnote-ref",
];

/// The stage-1 rules (R-M2, M4) on the parsed tags: nothing executable,
/// only http(s)/mailto/fragment/single-slash links, no images yet, inputs
/// only as disabled checkboxes.
fn assert_safe(sample: &str, html: &str) {
    let ts = tags(html);
    for t in &ts {
        assert!(
            ALLOWED_TAGS.contains(&t.name.as_str()),
            "tag {} from {sample:?}: {html}",
            t.name
        );
        for (k, v) in &t.attrs {
            assert!(
                !k.starts_with("on")
                    && ![
                        "style",
                        "src",
                        "srcdoc",
                        "srcset",
                        "formaction",
                        "action",
                        "background"
                    ]
                    .contains(&k.as_str()),
                "attribute {k} from {sample:?}: {html}"
            );
            let allowed = match (t.name.as_str(), k.as_str()) {
                (
                    "a",
                    "href" | "title" | "rel" | "class" | "id" | "data-ox-agent" | "aria-hidden",
                ) => true,
                ("a", "target") => v == "_blank",
                ("a", "data-ox-host") => true,
                ("a", "data-footnote-ref" | "data-footnote-backref" | "aria-label") => true,
                ("span", "class" | "title" | "data-ox-ref" | "data-ox-v") => true,
                ("input", "type" | "disabled" | "checked") => true,
                ("ol", "start") => true,
                ("th" | "td", "align") => true,
                (
                    "li" | "sup" | "section" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6",
                    "id" | "class",
                ) => true,
                ("section", "data-footnotes") => true,
                ("code" | "pre", "class" | "lang") => true,
                _ => false,
            };
            assert!(allowed, "{}[{k}={v:?}] from {sample:?}: {html}", t.name);
            if k == "class" {
                for c in v.split_whitespace() {
                    assert!(
                        ALLOWED_CLASSES.contains(&c) || c.starts_with("language-"),
                        "class {c} from {sample:?}: {html}"
                    );
                }
            }
            if k == "id" {
                assert!(
                    v.starts_with("h-") || v.starts_with("fn-") || v.starts_with("fnref-"),
                    "id {v} from {sample:?}: {html}"
                );
            }
            if k == "href" {
                let ok = if let Some(rest) = v.strip_prefix('/') {
                    !rest.starts_with('/') && !rest.starts_with('\\')
                } else if v.starts_with('#') {
                    true
                } else {
                    match url::Url::parse(v) {
                        Ok(u) => ["http", "https", "mailto"].contains(&u.scheme()),
                        Err(_) => false,
                    }
                };
                assert!(ok, "href {v:?} from {sample:?}: {html}");
                assert!(
                    !v.chars().any(|c| c.is_control() || c.is_whitespace()),
                    "href {v:?} from {sample:?}"
                );
            }
        }
        if t.name == "input" && !t.close {
            assert_eq!(t.attr("type"), Some("checkbox"), "{sample:?}: {html}");
            assert!(t.attr("disabled").is_some(), "{sample:?}: {html}");
        }
        if t.name == "a" && !t.close {
            let href = t.attr("href").unwrap_or("");
            if href.starts_with("http") {
                assert!(
                    t.attr("rel")
                        .is_some_and(|r| r.contains("noopener") && r.contains("nofollow")),
                    "external link without rel from {sample:?}: {html}"
                );
            }
        }
    }
}

fn src(format: TextFormat, agent: bool) -> Source {
    Source {
        format,
        agent,
        container: Uuid::nil(),
    }
}

fn md(text: &str) -> markdown::Stage1 {
    markdown::render_stage1(text, src(TextFormat::Markdown, false))
}

/// T-M2-01: at least 60 samples (OWASP XSS filter evasion variants among
/// them), in both formats, by a person and by an agent.
const XSS: &[&str] = &[
    "<script>alert(1)</script>",
    "<img src=x onerror=alert(1)>",
    "<svg onload=alert(1)>",
    "[x](javascript:alert(1))",
    "[x](JaVaScRiPt:alert(1))",
    "[x](&#106;avascript:alert(1))",
    "[x](&#x6A;avascript:alert(1))",
    "[x](java&#09;script:alert(1))",
    "[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)",
    "[x](https://ok.example \"a\\\" onmouseover=\\\"alert(1)\")",
    "[x](https://ok.example 'it''s \"q\"')",
    "<!-- <!-- nested --> <script>alert(1)</script> -->",
    "<style>body{display:none}</style>",
    "| a | b |\n|---|---|\n| <script>alert(1)</script> | <img src=x onerror=alert(1)> |",
    "[x](//evil.example/a)",
    "[x](/\\evil.example/a)",
    "![i](//evil.example/a.png)",
    "![i](/\\evil.example/a.png)",
    "[x](/attachments/00000000-0000-0000-0000-000000000000/content/../x)",
    "![i](/attachments/00000000-0000-0000-0000-000000000000/content/../x)",
    "<input type=\"text\" value=\"x\">",
    "- [ ] task\n- [x] done",
    "<a href=\"javascript:alert(1)\">x</a>",
    "<iframe src=\"https://evil.example\"></iframe>",
    "<math><mtext><table><mglyph><style><img src=x onerror=alert(1)>",
    "<form action=\"https://evil.example\"><button>go</button></form>",
    "[x](vbscript:msgbox(1))",
    "[x](file:///etc/passwd)",
    "<https://ok.example/a?b=<script>>",
    "<javascript:alert(1)>",
    "https://ok.example/\"onmouseover=\"alert(1)",
    "www.evil.example/<script>",
    "[x](  javascript:alert(1)  )",
    "[x](<javascript:alert(1)>)",
    "[x][r]\n\n[r]: javascript:alert(1)",
    "[x][r]\n\n[r]: https://ok.example \"t\\\" onclick=\\\"alert(1)\"",
    "![x](javascript:alert(1))",
    "![x](data:image/svg+xml;base64,PHN2Zy8+)",
    "<div style=\"background:url(javascript:alert(1))\">x</div>",
    "<a href=\"https://ok.example\" onclick=\"alert(1)\">x</a>",
    "<details open ontoggle=alert(1)>",
    "<body onload=alert(1)>",
    "<object data=\"javascript:alert(1)\">",
    "<embed src=\"javascript:alert(1)\">",
    "<base href=\"https://evil.example/\">",
    "<meta http-equiv=\"refresh\" content=\"0;url=javascript:alert(1)\">",
    "<link rel=stylesheet href=https://evil.example/x.css>",
    "`<script>alert(1)</script>`",
    "```html\n<script>alert(1)</script>\n```",
    "[<img src=x onerror=alert(1)>](https://ok.example)",
    "**<script>**alert(1)",
    "<scr<script>ipt>alert(1)</script>",
    "\"><script>alert(1)</script>",
    "[x](https://ok.example/\" onmouseover=\"alert(1))",
    "[x](https://ok.example/%22%3E%3Cscript%3E)",
    "<video><source onerror=alert(1)>",
    "<textarea><script>alert(1)</script></textarea>",
    "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\">",
    "<template><script>alert(1)</script></template>",
    "x[^1]\n\n[^1]: <script>alert(1)</script> [x](javascript:alert(1))",
    "# <img src=x onerror=alert(1)>",
    "> <script>alert(1)</script>",
    "[x](mailto:a@example.com?subject=<script>)",
    "[x](%6Aavascript:alert(1))",
    "<a href=\"&#x6A;avascript:alert(1)\">x</a>",
    "[[javascript:alert(1)]]",
    "[[AUTH:<script>alert(1)</script>]]",
    "[[x|<img src=x onerror=alert(1)>]]",
    "@<script>alert(1)</script>",
    "AUTH-1<script>",
    "<x onmouseover=alert(1)>hover</x>",
    "<span data-ox-ref=\"user\" data-ox-v=\"bob\">@bob</span>",
    "[x](https://ok.example \"<span data-ox-ref=&quot;user&quot; data-ox-v=&quot;bob&quot;>\")",
    "<IMG SRC=JaVaScRiPt:alert('XSS')>",
    "<IMG SRC=`javascript:alert(\"RSnake says, 'XSS'\")`>",
    "<IMG \"\"\"><SCRIPT>alert(\"XSS\")</SCRIPT>\"\\>",
    "<IMG SRC=\"jav&#x0A;ascript:alert('XSS');\">",
    "<BODY BACKGROUND=\"javascript:alert('XSS')\">",
    "<TABLE BACKGROUND=\"javascript:alert('XSS')\">",
    "[a](javascript&colon;alert(1))",
    "[a](\u{0001}javascript:alert(1))",
    "[a](https://ok.example/\u{202e}gpj.exe)",
];

#[test]
fn t_m2_01_xss_corpus() {
    assert!(XSS.len() >= 60, "{} samples", XSS.len());
    for s in XSS {
        for (format, agent) in [
            (TextFormat::Markdown, false),
            (TextFormat::Markdown, true),
            (TextFormat::Plain, false),
            (TextFormat::Plain, true),
        ] {
            let st = markdown::render_stage1(s, src(format, agent));
            assert_safe(s, &st.html);
            // Stage 2 with nothing resolvable adds no markup of its own.
            let none = markdown::render_stage1(s, src(format, agent));
            assert_eq!(st, none, "stage 1 is deterministic");
        }
    }
    // Raw tags are text.
    let h = md("<script>alert(1)</script> <b>x</b>").html;
    assert!(h.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{h}");
    assert!(h.contains("&lt;b&gt;x&lt;/b&gt;"), "{h}");
    assert!(open_tags(&tags(&h), "b").is_empty());
    // `//` and `/\` are other hosts: external links, never site paths.
    for s in ["[x](//evil.example/a)", "[x](/\\evil.example/a)"] {
        let ts = tags(&md(s).html);
        let a = open_tags(&ts, "a");
        assert_eq!(a.len(), 1, "{s}");
        assert_eq!(a[0].attr("href"), Some("https://evil.example/a"), "{s}");
        assert!(a[0].attr("class").unwrap_or("").contains("ox-ext"), "{s}");
    }
    // A site path stays one (normalized).
    let ts = tags(&md("[x](/issue/AUTH-1)").html);
    assert_eq!(open_tags(&ts, "a")[0].attr("href"), Some("/issue/AUTH-1"));
    assert!(open_tags(&ts, "a")[0].attr("class").is_none());
    // No image loads in M2.1: an image is an "external image" link or text.
    for s in [
        "![i](//evil.example/a.png)",
        "![i](https://evil.example/a.png)",
        "![i](data:image/png;base64,AAAA)",
    ] {
        let h = md(s).html;
        assert!(open_tags(&tags(&h), "img").is_empty(), "{h}");
        assert!(h.contains("外部圖片"), "{s}: {h}");
    }
    // Inputs: only the task list's disabled checkboxes.
    let ts = tags(&md("<input type=\"text\">\n\n- [ ] a\n- [x] b").html);
    let inputs = open_tags(&ts, "input");
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().all(|t| t.attr("type") == Some("checkbox")));
    // Disallowed schemes: the link goes, its text stays.
    for s in [
        "[keep](javascript:alert(1))",
        "[keep](data:text/html,x)",
        "[keep](vbscript:x)",
        "[keep](relative/path)",
    ] {
        let h = md(s).html;
        assert!(open_tags(&tags(&h), "a").is_empty(), "{s}: {h}");
        assert!(h.contains("keep"), "{s}: {h}");
    }
    // A forged placeholder is text, never a chip after stage 2.
    let forged = md("<span data-ox-ref=\"user\" data-ox-v=\"bob\">x</span>");
    assert!(
        !forged.html.contains("<span data-ox-ref"),
        "{}",
        forged.html
    );
    assert!(forged.refs.logins.is_empty());
}

/// R-M2, R-M3: a path that normalizes (WHATWG) to `//host` or `/\host` is
/// that host: an external link, never a site path. Inline, reference and
/// image links; a person's and an agent's.
#[test]
fn t_m2_01b_normalized_other_hosts() {
    let paths = [
        "/.//evil.example/a",
        "/a/..//evil.example/a",
        "/./\\evil.example/a",
        "/x/../\\evil.example/a",
        "/%2E//evil.example/a",
        "/%2e/%2E//evil.example/a",
    ];
    for p in paths {
        for text in [
            format!("[x]({p})"),
            format!("[x][r]\n\n[r]: {p}"),
            format!("![i]({p})"),
        ] {
            for agent in [false, true] {
                let h = markdown::render_stage1(&text, src(TextFormat::Markdown, agent)).html;
                assert_safe(&text, &h);
                let ts = tags(&h);
                let a = open_tags(&ts, "a");
                assert_eq!(a.len(), 1, "{text} agent={agent}: {h}");
                let href = a[0].attr("href").unwrap_or("");
                if let Some(rest) = href.strip_prefix('/') {
                    assert!(
                        !rest.starts_with('/') && !rest.starts_with('\\'),
                        "{text} agent={agent}: {h}"
                    );
                }
                assert_eq!(href, "https://evil.example/a", "{text} agent={agent}: {h}");
                let class = if text.starts_with('!') {
                    "ox-img-ext"
                } else {
                    "ox-ext"
                };
                assert_eq!(a[0].attr("class"), Some(class), "{text}: {h}");
                assert_eq!(a[0].attr("data-ox-host"), Some("evil.example"), "{text}");
                assert_eq!(a[0].attr("target"), Some("_blank"), "{text}");
                assert!(
                    a[0].attr("rel").is_some_and(|r| r.contains("noopener")),
                    "{text}: {h}"
                );
                assert_eq!(a[0].attr("data-ox-agent").is_some(), agent, "{text}: {h}");
            }
        }
    }
}

/// T-M2-06: an agent's long links are text, its other external links carry
/// `data-ox-agent`; a person's are all links.
#[test]
fn t_m2_06_agent_links() {
    let long = format!("https://x.example/{}", "a".repeat(250 - 18));
    assert_eq!(long.len(), 250);
    let query = format!("https://x.example/p?q={}", "b".repeat(118));
    assert_eq!(query.split_once('?').unwrap().1.len(), 120);
    let ok = "https://ok.example/p";
    for text in [
        format!("[long]({long}) [query]({query}) [ok]({ok})"),
        format!("{long}\n\n{query}\n\n{ok}"),
    ] {
        let agent = markdown::render_stage1(&text, src(TextFormat::Markdown, true));
        let ts = tags(&agent.html);
        let a = open_tags(&ts, "a");
        assert_eq!(a.len(), 1, "{}", agent.html);
        assert_eq!(a[0].attr("href"), Some(ok));
        assert!(a[0].attr("data-ox-agent").is_some(), "{}", agent.html);
        let human = markdown::render_stage1(&text, src(TextFormat::Markdown, false));
        let ts = tags(&human.html);
        let a = open_tags(&ts, "a");
        assert_eq!(a.len(), 3, "{}", human.html);
        assert!(a.iter().all(|t| t.attr("data-ox-agent").is_none()));
    }
    // Plain text by an agent: the same rules on autolinked URLs.
    let agent = markdown::render_stage1(&format!("{long} {ok}"), src(TextFormat::Plain, true));
    let ts = tags(&agent.html);
    assert_eq!(open_tags(&ts, "a").len(), 1, "{}", agent.html);
}

/// T-M2-05: code spans, code blocks and link text are not parsed.
#[test]
fn t_m2_05_code_and_links_are_not_references() {
    let s = md(
        "`AUTH-1` and `@bob`\n\n```\nAUTH-1 @bob\n```\n\n    AUTH-1 @bob\n\n[AUTH-1 @bob](https://x.example)",
    );
    assert!(s.refs.issues.is_empty(), "{:?}", s.refs);
    assert!(s.refs.logins.is_empty(), "{:?}", s.refs);
    assert!(!s.html.contains("data-ox-ref"), "{}", s.html);
    // Outside them they are.
    let s = md("AUTH-1 @bob");
    assert_eq!(s.refs.issues.len(), 1);
    assert_eq!(s.refs.logins.len(), 1);
}

/// T-M2-14 (no database): footnote and heading ids carry the container.
#[test]
fn t_m2_14_ids_per_container() {
    let text = "# 標題\n\n參考[^1]\n\n[^1]: 註";
    let a = markdown::render_stage1(
        text,
        Source {
            format: TextFormat::Markdown,
            agent: false,
            container: Uuid::now_v7(),
        },
    );
    let b = markdown::render_stage1(
        text,
        Source {
            format: TextFormat::Markdown,
            agent: false,
            container: Uuid::now_v7(),
        },
    );
    let ids = |h: &str| -> BTreeSet<String> {
        tags(h)
            .iter()
            .filter_map(|t| t.attr("id").map(String::from))
            .collect()
    };
    let (ia, ib) = (ids(&a.html), ids(&b.html));
    assert!(ia.iter().any(|i| i.starts_with("h-")), "{}", a.html);
    assert!(ia.iter().any(|i| i.starts_with("fn-")), "{}", a.html);
    assert!(ia.iter().any(|i| i.starts_with("fnref-")), "{}", a.html);
    assert!(ia.is_disjoint(&ib), "{ia:?} {ib:?}");
}

/// T-M2-11: pathological input finishes in time (on a 2 MiB stack); more
/// than 200 references are cut.
#[test]
fn t_m2_11_pathological() {
    let refs = (1..=1000)
        .map(|n| format!("AUTH-{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mentions = (1..=1000)
        .map(|n| format!("@u{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let samples: Vec<(&str, String)> = vec![
        ("10000 >", format!("{} x", ">".repeat(10_000))),
        ("50000 *", "*".repeat(50_000)),
        ("50000 [", "[".repeat(50_000)),
        ("200 KiB line", "a".repeat(200 * 1024)),
        ("200 KiB words", "ab cd ".repeat(200 * 1024 / 6)),
        ("nested lists", "- ".repeat(5_000) + "x"),
        ("1000 refs", refs),
        ("1000 mentions", mentions),
        (
            "over the input cap",
            "x ".repeat(markdown::MAX_INPUT_BYTES / 2 + 1),
        ),
        // 256 KiB each (the M2.1 review's slow inputs).
        ("http:// x256K", "http:// ".repeat(KIB256 / 8)),
        ("short links x256K", "http://a ".repeat(KIB256 / 9)),
        (
            "autolinks x256K",
            "https://x.example/a ".repeat(KIB256 / 20),
        ),
        (
            "footnote refs x256K",
            "[^1] ".repeat(KIB256 / 5) + "\n\n[^1]: n",
        ),
        ("equal headings x256K", "# x\n".repeat(KIB256 / 4)),
        (
            "mentions in addresses x256K",
            "@a http://x/@b AUTH-1 ".repeat(KIB256 / 22),
        ),
    ];
    // Release: within 1 s (the 800 ms budget, then the escaped fallback).
    // Debug builds run comrak and ammonia several times slower, and the last
    // uninterruptible step (sanitizing) can run past the budget: 5 s.
    let limit = if cfg!(debug_assertions) {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(1)
    };
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let mut slowest = (Duration::ZERO, String::new());
            for (name, text) in &samples {
                for format in [TextFormat::Markdown, TextFormat::Plain] {
                    let t = Instant::now();
                    let s = markdown::render_stage1(text, src(format, false));
                    let took = t.elapsed();
                    println!("{name} {format:?}: {took:?} ({} bytes out)", s.html.len());
                    if took > slowest.0 {
                        slowest = (took, format!("{name} {format:?}"));
                    }
                    assert!(took < limit, "{name} {format:?}: {took:?}");
                    assert!(
                        s.html.len() <= 8 * text.len() + 64 * 1024 + 16,
                        "{name} {format:?}: {} bytes",
                        s.html.len()
                    );
                    assert!(
                        s.refs.issues.len() + s.refs.logins.len() <= markdown::MAX_REFS,
                        "{name}"
                    );
                    let t = Instant::now();
                    let _ = markdown::plain_text(text, format);
                    assert!(t.elapsed() < Duration::from_secs(1), "{name} plain_text");
                    if *name == "1000 refs" || *name == "1000 mentions" {
                        assert!(s.truncated, "{name}");
                        assert_eq!(
                            s.refs.issues.len() + s.refs.logins.len(),
                            markdown::MAX_REFS,
                            "{name}"
                        );
                    }
                }
            }
            println!("slowest: {} {:?}", slowest.1, slowest.0);
        })
        .unwrap();
    h.join().unwrap();
}

const KIB256: usize = 256 * 1024;

/// R-M8: an address after a reference that ran over another address's
/// start still becomes a link (the address scan does not stall).
#[test]
fn plain_addresses_after_an_overlap() {
    let s = markdown::render_stage1(
        "@bobhttp://x.example https://y.example AUTH-1",
        src(TextFormat::Plain, false),
    );
    let ts = tags(&s.html);
    let hrefs: Vec<&str> = open_tags(&ts, "a")
        .iter()
        .filter_map(|t| t.attr("href"))
        .collect();
    assert!(hrefs.contains(&"https://y.example/"), "{}", s.html);
    assert!(
        s.html.contains(r#"data-ox-ref="issue" data-ox-v="AUTH-1""#),
        "{}",
        s.html
    );
}

/// T-M2-11: the heading-id and footnote caps.
#[test]
fn t_m2_11_caps() {
    // Equal headings: the ids are dropped, the headings stay.
    let s = md(&"# x\n".repeat(300));
    assert_eq!(open_tags(&tags(&s.html), "h1").len(), 300);
    assert!(!s.html.contains("id=\"h-"), "no ids past the cap");
    let s = md("# x\n# x");
    assert!(s.html.contains("id=\"h-"), "{}", s.html);
    // Footnote references past the cap: plain text (padded so the output
    // stays under the size cap: this is the count cap alone).
    let pad = "x".repeat(200);
    let s = md(&(format!("[^1] {pad} ").repeat(600) + "\n\n[^1]: n"));
    assert!(open_tags(&tags(&s.html), "sup").is_empty(), "plain");
    assert!(s.html.contains("[^1]"));
    let s = md("a[^1]\n\n[^1]: n");
    assert_eq!(open_tags(&tags(&s.html), "sup").len(), 1, "{}", s.html);
    // Output past 8× the input (+64 KiB): escaped text, in both formats.
    let short = "http://a ".repeat(4000);
    for format in [TextFormat::Markdown, TextFormat::Plain] {
        let s = markdown::render_stage1(&short, src(format, false));
        assert!(s.html.len() <= 8 * short.len() + 64 * 1024, "{format:?}");
        assert!(open_tags(&tags(&s.html), "a").is_empty(), "{format:?}");
    }
    let s = markdown::render_stage1("http://a http://b", src(TextFormat::Plain, false));
    assert_eq!(open_tags(&tags(&s.html), "a").len(), 2, "{}", s.html);
}

/// R-M11: the text search indexes.
#[test]
fn plain_text_extraction() {
    let p = markdown::plain_text(
        "# 標題\n\n**登入**失敗，請看[文件](https://docs.zzz.example/x)。\n\n- `code`\n- ~~刪~~",
        TextFormat::Markdown,
    );
    assert!(p.contains("登入失敗"), "{p}");
    assert!(p.contains("文件"), "{p}");
    assert!(!p.contains("https") && !p.contains("zzz"), "{p}");
    assert!(!p.contains('#') && !p.contains("**"), "{p}");
    assert!(p.contains("code") && p.contains("刪"), "{p}");
    // Plain text is as it is.
    let raw = "**x** [y](https://z.example)";
    assert_eq!(markdown::plain_text(raw, TextFormat::Plain), raw);
}

/// T-M2-08 (no database): `plain` text keeps its characters.
#[test]
fn t_m2_08_plain_is_text() {
    let s = markdown::render_stage1(
        "# 標題\n* 清單\nAUTH-1\n<b>x</b>",
        src(TextFormat::Plain, false),
    );
    let ts = tags(&s.html);
    for t in ["h1", "li", "ul", "b"] {
        assert!(open_tags(&ts, t).is_empty(), "{t}: {}", s.html);
    }
    assert!(s.html.contains("# 標題"), "{}", s.html);
    assert!(s.html.contains("* 清單"), "{}", s.html);
    assert!(s.html.contains("&lt;b&gt;x&lt;/b&gt;"), "{}", s.html);
    assert_eq!(s.refs.issues.iter().collect::<Vec<_>>(), ["AUTH-1"]);
}

// ---------------------------------------------------------------- database helpers

struct F {
    w: World,
    /// Role Viewer in WEB and AUTH (view_issues only, issues_visibility
    /// default): no private issues or notes.
    dave: Uuid,
    bob_ctx: AuthContext,
    carol_ctx: AuthContext,
    dave_ctx: AuthContext,
}

async fn setup(pool: &sqlx::PgPool) -> F {
    faults::clear();
    let w = World::build(pool).await;
    let viewer = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(pool, viewer, &["view_issues"]).await;
    let dave = auth::create_user(pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(pool, w.web, dave, viewer).await;
    member(pool, w.auth_p, dave, viewer).await;
    let bob_ctx = sign_in(pool, &w.policy, "bob").await;
    let carol_ctx = sign_in(pool, &w.policy, "carol").await;
    let dave_ctx = sign_in(pool, &w.policy, "dave").await;
    F {
        w,
        dave,
        bob_ctx,
        carol_ctx,
        dave_ctx,
    }
}

fn doc(ops: Vec<Value>, pre: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "m2.1", "ops": ops,
           "preconditions": pre.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn create(w: &World, who: AuthContext, project: Uuid, fields: Value) -> (Uuid, String) {
    let r = commands::submit(
        &w.app,
        who,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": project,
                        "tracker_id": w.bug, "fields": fields}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    dispatch(w).await;
    (r.effects[0].issue_id, r.effects[0].key.clone())
}

async fn run(w: &World, who: AuthContext, i: Uuid, ops: Vec<Value>) {
    commands::submit(
        &w.app,
        who,
        Uuid::now_v7(),
        &doc(ops, vec![(i, lv(w, i).await)]),
    )
    .await
    .unwrap();
}

fn set_desc(i: Uuid, text: &str) -> Value {
    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"description": text}})
}

fn note(i: Uuid, body: &str, private: bool) -> Value {
    json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body, "is_private": private})
}

async fn dispatch(w: &World) {
    notifications::dispatch_all(&w.pool).await.unwrap();
}

async fn item(w: &World, who: AuthContext, key: &str) -> Option<NotificationItem> {
    notifications::list(&w.app, who, &ListQuery::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.issue.as_ref().is_some_and(|x| x.key == key))
}

async fn unread(w: &World, who: AuthContext) -> u32 {
    notifications::unread_count(&w.app, who)
        .await
        .unwrap()
        .count
        .unwrap_or(0)
}

async fn rows(w: &World, user: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notifications WHERE recipient_id = $1")
        .bind(user)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn unmember(w: &World, project: Uuid, who: Uuid) {
    exec(
        &w.pool,
        &format!(
            "DELETE FROM member_roles WHERE member_id IN
               (SELECT id FROM members WHERE project_id = '{project}' AND principal_id = '{who}');
             DELETE FROM members WHERE project_id = '{project}' AND principal_id = '{who}';"
        ),
    )
    .await;
}

async fn prefs_put(w: &World, who: AuthContext, patch: Value) {
    let cur = notifications::preferences(&w.app, who).await.unwrap();
    let mut body = patch;
    body["lock_version"] = json!(cur.lock_version);
    let input: PreferencesPut = serde_json::from_value(body).unwrap();
    notifications::put_preferences(&w.app, who, Uuid::now_v7(), &input)
        .await
        .unwrap();
}

async fn render(w: &World, who: AuthContext, project: Uuid, text: &str, s: Source) -> String {
    let mut c = w.pool.acquire().await.unwrap();
    markdown::render_for(&mut c, who, project, &[Text { text, src: s }])
        .await
        .unwrap()
        .remove(0)
}

fn mdsrc(container: Uuid) -> Source {
    Source {
        format: TextFormat::Markdown,
        agent: false,
        container,
    }
}

async fn spa(w: &World, who: AuthContext, key: &str) -> issue_view::SpaIssue {
    issue_view::spa_view(
        &w.app,
        who,
        key,
        Some(vec!["recent_notes".into()]),
        Some(10),
        None,
    )
    .await
    .unwrap()
}

fn shape(e: &anyhow::Error) -> (ErrorCode, Value, String) {
    let a = classify(e).unwrap_or_else(|| panic!("{e:#}"));
    (
        a.code,
        a.details.clone().unwrap_or(Value::Null),
        a.message.clone(),
    )
}

// ---------------------------------------------------------------- references

/// T-M2-02, T-M2-08 (plain), T-M2-10: issue references by visibility,
/// byte for byte; the stage-1 cache is shared, stage 2 is per reader.
#[tokio::test]
async fn t_m2_02_08_10_issue_references() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_, k1) = create(w, f.bob_ctx, w.auth_p, json!({"subject": "pub \"<x>\""})).await;
    let (_, k2) = create(
        w,
        f.bob_ctx,
        w.auth_p,
        json!({"subject": "secret", "is_private": true}),
    )
    .await;
    let c = Uuid::now_v7();
    let text = format!("{k1} {k2} AUTH-999 ZZZ-1");
    // dave cannot see k2: it, a missing number and a missing project are
    // all just their text.
    let dave = render(w, f.dave_ctx, w.auth_p, &text, mdsrc(c)).await;
    assert_eq!(
        dave.trim(),
        format!(
            r#"<p><a class="ox-issue" href="/issue/{k1}" title="pub &quot;&lt;x&gt;&quot;">{k1}</a> {k2} AUTH-999 ZZZ-1</p>"#
        )
    );
    for (hidden, missing) in [(&k2, "AUTH-999"), (&k2, "ZZZ-1")] {
        let a = render(w, f.dave_ctx, w.auth_p, hidden, mdsrc(c)).await;
        let b = render(w, f.dave_ctx, w.auth_p, missing, mdsrc(c)).await;
        assert_eq!(a.replace(hidden.as_str(), "@@"), b.replace(missing, "@@"));
        assert_eq!(a.trim(), format!("<p>{hidden}</p>"));
    }
    // bob sees both.
    let bob = render(w, f.bob_ctx, w.auth_p, &text, mdsrc(c)).await;
    assert!(
        bob.contains(&format!(r#"href="/issue/{k2}" title="secret">{k2}</a>"#)),
        "{bob}"
    );
    // Closed issues get ox-closed.
    exec(&pool, &format!("UPDATE issues SET status_id = '{}' WHERE id IN (SELECT issue_id FROM issue_keys WHERE key = '{k1}')", w.done)).await;
    let closed = render(w, f.dave_ctx, w.auth_p, &k1, mdsrc(c)).await;
    assert!(closed.contains(r#"class="ox-issue ox-closed""#), "{closed}");

    // T-M2-10: one stage-1 entry for both readers, whatever the order.
    let t = format!("cache {k2} {}", Uuid::now_v7());
    let n0 = markdown::cache_len();
    let d1 = render(w, f.dave_ctx, w.auth_p, &t, mdsrc(c)).await;
    let b1 = render(w, f.bob_ctx, w.auth_p, &t, mdsrc(c)).await;
    assert_eq!(markdown::cache_len(), n0 + 1);
    assert_ne!(d1, b1);
    assert!(!d1.contains("ox-issue") && b1.contains("ox-issue"));
    let t2 = format!("cache {k2} {}", Uuid::now_v7());
    let b2 = render(w, f.bob_ctx, w.auth_p, &t2, mdsrc(c)).await;
    let d2 = render(w, f.dave_ctx, w.auth_p, &t2, mdsrc(c)).await;
    let strip = |s: &str, t: &str| s.replace(t.rsplit(' ').next().unwrap(), "");
    assert_eq!(strip(&d1, &t), strip(&d2, &t2));
    assert_eq!(strip(&b1, &t), strip(&b2, &t2));
    let s1 = markdown::stage1(&t, mdsrc(c));
    assert!(Arc::ptr_eq(&s1, &markdown::stage1(&t, mdsrc(c))));
    // Another container, format or author is another entry.
    assert!(!Arc::ptr_eq(
        &s1,
        &markdown::stage1(&t, mdsrc(Uuid::now_v7()))
    ));
    let mut agent = mdsrc(c);
    agent.agent = true;
    assert!(!Arc::ptr_eq(&s1, &markdown::stage1(&t, agent)));

    // T-M2-08: a plain text keeps `#`, `*` and tags as text; keys resolve.
    let plain = render(
        w,
        f.dave_ctx,
        w.auth_p,
        &format!("# 標題\n* 清單\n{k1}\n<b>x</b>"),
        Source {
            format: TextFormat::Plain,
            agent: false,
            container: c,
        },
    )
    .await;
    assert!(
        plain.contains("# 標題") && plain.contains("* 清單"),
        "{plain}"
    );
    assert!(plain.contains("&lt;b&gt;x&lt;/b&gt;"), "{plain}");
    assert!(plain.contains(&format!(r#"href="/issue/{k1}""#)), "{plain}");
    assert_safe("plain", &plain);
}

/// T-M2-03, T-M2-03a: mention chips only for the assignable list, outputs
/// for strangers and nobody alike.
#[tokio::test]
async fn t_m2_03_mentions() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    exec(
        &pool,
        "UPDATE principals SET name = 'Bob <Lin>' WHERE login = 'bob'",
    )
    .await;
    auth::create_user(&pool, "eve", "eve", PW, false)
        .await
        .unwrap();
    let ann = auth::create_user(&pool, "ann", "ann", PW, false)
        .await
        .unwrap();
    let ann2 = auth::create_user(&pool, "ANN", "ANN", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, ann, w.dev).await;
    member(&pool, w.web, ann2, w.dev).await;
    let cora = auth::create_user(&pool, "cora", "cora", PW, false)
        .await
        .unwrap();
    let watcher = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility, assignable) VALUES ('Watcher','all', false) RETURNING id",
    )
    .await;
    grant(&pool, watcher, &["view_issues"]).await;
    member(&pool, w.web, cora, watcher).await;
    let c = Uuid::now_v7();
    let r = |t: &'static str| render(w, w.alice_ctx, w.web, t, mdsrc(c));

    let h = r("@bob @eve @nobody @claude-code@alice email@example.com").await;
    let chip = r#"<span class="ox-mention" title="bob">@Bob &lt;Lin&gt;</span>"#;
    assert!(
        h.starts_with(&format!("<p>{chip} @eve @nobody @claude-code@alice ")),
        "{h}"
    );
    assert_eq!(h.matches("ox-mention").count(), 1, "{h}");
    assert_eq!(
        r("@eve").await.replace("eve", "x"),
        r("@nobody").await.replace("nobody", "x")
    );
    // T-M2-03a: case-insensitive when one login matches; ambiguous and
    // not-assignable stay text.
    let h = r("@Bob").await;
    assert!(h.contains(chip), "{h}");
    assert_eq!(r("@Ann").await.trim(), "<p>@Ann</p>");
    assert!(r("@ann").await.contains(r#"title="ann""#));
    assert_eq!(r("@cora").await.trim(), "<p>@cora</p>");
    assert_eq!(
        r("@cora").await.replace("cora", "x"),
        r("@nobody").await.replace("nobody", "x")
    );
}

/// T-M2-05 against the database: nothing in code or link text resolves.
#[tokio::test]
async fn t_m2_05_code_spans_resolve_nothing() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_, k1) = create(w, f.bob_ctx, w.web, json!({"subject": "x"})).await;
    let t = format!("`{k1}` `@bob`\n\n```\n{k1} @bob\n```\n\n[{k1} @bob](https://x.example)");
    let h = render(w, w.alice_ctx, w.web, &t, mdsrc(Uuid::now_v7())).await;
    assert!(!h.contains("ox-issue") && !h.contains("ox-mention"), "{h}");
}

// ---------------------------------------------------------------- *_html and preview

/// T-M2-13, T-M2-14 and the `*_html` fields of `GET /issues/{key}` and
/// `GET /issues/{key}/view`.
#[tokio::test]
async fn t_m2_13_14_html_fields() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let a = w.alice_ctx;
    let foot = "# 標題\n\n參考[^1]\n\n[^1]: 註";
    let (i, key) = create(w, a, w.web, json!({"subject": "html", "description": foot})).await;
    run(w, a, i, vec![note(i, foot, false), note(i, foot, false)]).await;
    let v = spa(w, a, &key).await;
    let mut htmls = vec![v.description_html.clone().unwrap()];
    let notes = &v.view.recent_notes.as_ref().unwrap().items;
    assert_eq!(notes.len(), 2);
    htmls.extend(notes.iter().map(|n| n.body_html.clone().unwrap()));
    let mut all = BTreeSet::new();
    for h in &htmls {
        assert_safe("footnotes", h);
        let ts = tags(h);
        let ids: BTreeSet<String> = ts
            .iter()
            .filter_map(|t| t.attr("id").map(String::from))
            .collect();
        assert!(ids.len() >= 3, "{h}");
        for t in &ts {
            if let Some(target) = t.attr("href").and_then(|x| x.strip_prefix('#')) {
                assert!(
                    ids.contains(target),
                    "#{target} not in its own container: {h}"
                );
            }
        }
        for id in ids {
            assert!(all.insert(id.clone()), "{id} repeats across containers");
        }
    }
    // REST detail: the same description HTML; MCP has none.
    let d = service::get(&w.app, a, &key).await.unwrap();
    assert_eq!(d.description_html, v.description_html);
    assert_eq!(d.description_by_agent, Some(false));
    let j = serde_json::to_value(
        issue_view::get(
            &w.app,
            a,
            issue_view::GetParams {
                key: key.clone(),
                fields: None,
                citations: None,
                include: Some(vec!["recent_notes".into()]),
                note_limit: None,
                event_limit: None,
            },
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(!j.to_string().contains("_html"), "MCP view: {j}");

    // T-M2-13: an agent proposal a person approved is still agent text.
    let deleg = w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(deleg.context_id).await;
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        deleg.id,
        PolicyInput {
            commands: BTreeMap::new(),
            fields: [("description".to_string(), PolicyMode::Review)].into(),
        },
    )
    .await
    .unwrap();
    let long = format!("https://x.example/{}", "a".repeat(232));
    let text = format!("[long]({long}) [ok](https://ok.example/)");
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(vec![set_desc(i, &text)], vec![(i, lv(w, i).await)]),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    let SubmitResult::ReviewRequired { proposal, .. } = r else {
        panic!("expected review: {r:?}")
    };
    changesets::approve_and_apply(
        &w.app,
        a,
        Uuid::now_v7(),
        proposal.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: proposal.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    let v = spa(w, a, &key).await;
    assert_eq!(v.description_by_agent, Some(true));
    let h = v.description_html.clone().unwrap();
    let ts = tags(&h);
    let links = open_tags(&ts, "a");
    assert_eq!(links.len(), 1, "{h}");
    assert!(links[0].attr("data-ox-agent").is_some(), "{h}");
    assert_eq!(
        service::get(&w.app, a, &key)
            .await
            .unwrap()
            .description_by_agent,
        Some(true)
    );
    // A person rewrites it: a person's text again.
    run(w, a, i, vec![set_desc(i, &format!("{text} "))]).await;
    let v = spa(w, a, &key).await;
    assert_eq!(v.description_by_agent, Some(false));
    let h = v.description_html.clone().unwrap();
    let ts = tags(&h);
    let links = open_tags(&ts, "a");
    assert_eq!(links.len(), 2, "{h}");
    assert!(links.iter().all(|t| t.attr("data-ox-agent").is_none()));
    // An agent's note is agent text too.
    let agent_note = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(vec![note(i, &text, false)], vec![(i, lv(w, i).await)]),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(
        matches!(agent_note, SubmitResult::Applied { .. }),
        "{agent_note:?}"
    );
    let v = spa(w, a, &key).await;
    let n = v
        .view
        .recent_notes
        .as_ref()
        .unwrap()
        .items
        .iter()
        .find(|n| n.body == text)
        .unwrap();
    let ts = tags(n.body_html.as_deref().unwrap());
    assert_eq!(open_tags(&ts, "a").len(), 1);
}

/// T-M2-09 and the rest of `POST /markdown/preview`: not_found alike,
/// 413 by size, 429 by rate, nothing written.
#[tokio::test]
async fn t_m2_09_preview() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_, k1) = create(w, f.bob_ctx, w.auth_p, json!({"subject": "pub"})).await;
    let (_, k2) = create(
        w,
        f.bob_ctx,
        w.auth_p,
        json!({"subject": "secret", "is_private": true}),
    )
    .await;
    let pv = |ctx: PreviewContext, text: &str| PreviewInput {
        context: ctx,
        text: text.into(),
        format: None,
    };
    let issue = |k: &str| PreviewContext {
        issue: Some(k.into()),
        project: None,
        wiki_project: None,
    };
    let project = |k: &str| PreviewContext {
        issue: None,
        project: Some(k.into()),
        wiki_project: None,
    };
    let text = format!("**b** {k1} {k2}");
    let r = markdown::preview(&w.app, f.dave_ctx, &pv(issue(&k1), &text))
        .await
        .unwrap();
    assert_eq!(r.format_version, markdown::FORMAT_VERSION);
    assert!(!r.refs_truncated);
    assert!(r.html.contains("<strong>b</strong>"), "{}", r.html);
    assert!(r.html.contains(&format!("/issue/{k1}")), "{}", r.html);
    assert!(!r.html.contains(&format!("/issue/{k2}")), "{}", r.html);
    // Invisible and missing contexts: one not_found.
    // (The message names the key the caller sent, nothing more.)
    let hidden = markdown::preview(&w.app, f.dave_ctx, &pv(issue(&k2), "x"))
        .await
        .unwrap_err();
    let missing = markdown::preview(&w.app, f.dave_ctx, &pv(issue("AUTH-9999"), "x"))
        .await
        .unwrap_err();
    assert_eq!(shape(&hidden).0, ErrorCode::NotFound);
    let same = |e: &anyhow::Error, k: &str| {
        let (c, d, m) = shape(e);
        (c, d, m.replace(k, "KEY"))
    };
    assert_eq!(same(&hidden, &k2), same(&missing, "AUTH-9999"));
    // carol is not in AUTH: the project is not_found like a missing one.
    let hidden = markdown::preview(&w.app, f.carol_ctx, &pv(project("AUTH"), "x"))
        .await
        .unwrap_err();
    let missing = markdown::preview(&w.app, f.carol_ctx, &pv(project("NOPE"), "x"))
        .await
        .unwrap_err();
    assert_eq!(shape(&hidden).0, ErrorCode::NotFound);
    assert_eq!(shape(&hidden).0, shape(&missing).0);
    assert_eq!(shape(&hidden).1, shape(&missing).1);
    // Visibility before size: a large text for a hidden issue is not_found.
    let big = "x".repeat(markdown::PREVIEW_MAX_BYTES + 1);
    let e = markdown::preview(&w.app, f.dave_ctx, &pv(issue(&k2), &big))
        .await
        .unwrap_err();
    assert_eq!(
        same(&e, &k2),
        same(&missing_issue(w, f.dave_ctx).await, "AUTH-9999")
    );
    let e = markdown::preview(&w.app, f.dave_ctx, &pv(issue(&k1), &big))
        .await
        .unwrap_err();
    assert_eq!(shape(&e).0, ErrorCode::PayloadTooLarge);
    assert_eq!(shape(&e).1["limit"], json!(markdown::PREVIEW_MAX_BYTES));
    // The wiki does not exist before M2.2; a context needs exactly one key.
    let e = markdown::preview(
        &w.app,
        f.dave_ctx,
        &pv(
            PreviewContext {
                issue: None,
                project: None,
                wiki_project: Some("AUTH".into()),
            },
            "x",
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(shape(&e).0, ErrorCode::NotFound);
    let e = markdown::preview(
        &w.app,
        f.dave_ctx,
        &pv(
            PreviewContext {
                issue: Some(k1.clone()),
                project: Some("AUTH".into()),
                wiki_project: None,
            },
            "x",
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(shape(&e).0, ErrorCode::InvalidArgument);
    // Nothing is written: no event, no mention row.
    let before = count(&pool, "SELECT count(*) FROM issue_events").await;
    markdown::preview(&w.app, w.alice_ctx, &pv(project("AUTH"), "@bob"))
        .await
        .unwrap();
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_events").await,
        before
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_event_mentions").await,
        0
    );
    // 60 a minute per person.
    let mut limited = None;
    for n in 0..=markdown::PREVIEW_PER_MINUTE {
        if let Err(e) = markdown::preview(&w.app, f.bob_ctx, &pv(project("AUTH"), "x")).await {
            limited = Some((n, e));
            break;
        }
    }
    let (n, e) = limited.expect("rate limited");
    assert_eq!(n, markdown::PREVIEW_PER_MINUTE);
    assert_eq!(shape(&e).0, ErrorCode::RateLimited);
    assert_eq!(shape(&e).1["kind"], "markdown_preview");
    // Someone else is not limited by bob.
    markdown::preview(&w.app, w.alice_ctx, &pv(project("AUTH"), "x"))
        .await
        .unwrap();
}

async fn missing_issue(w: &World, who: AuthContext) -> anyhow::Error {
    markdown::preview(
        &w.app,
        who,
        &PreviewInput {
            context: PreviewContext {
                issue: Some("AUTH-9999".into()),
                project: None,
                wiki_project: None,
            },
            text: "x".into(),
            format: None,
        },
    )
    .await
    .unwrap_err()
}

// ---------------------------------------------------------------- mentions delivered

/// T-M2-15…19, 21 and `on_mentioned` (R-MN5: the reader's own rights).
#[tokio::test]
async fn t_m2_15_21_mention_delivery() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let a = w.alice_ctx;
    let mentioned = |i: &Option<NotificationItem>| {
        i.as_ref()
            .is_some_and(|i| i.reasons.iter().any(|r| r == "mentioned"))
    };

    // T-M2-15: a note naming bob reaches bob, who does not watch.
    let (i, key) = create(w, a, w.web, json!({"subject": "m15"})).await;
    run(w, a, i, vec![note(i, "@bob 看一下", false)]).await;
    dispatch(w).await;
    let it = item(w, f.bob_ctx, &key).await;
    assert!(mentioned(&it), "{it:?}");
    assert_eq!(it.as_ref().unwrap().reasons, ["mentioned"]);
    let watching: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issue_watchers WHERE issue_id = $1 AND user_id = $2",
    )
    .bind(i)
    .bind(w.bob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(watching, 0, "a mention does not make a watcher");

    // T-M2-16: a private note naming dave, who may not read private notes:
    // nothing, not even an unread count change.
    let dave_unread = unread(w, f.dave_ctx).await;
    let dave_rows = rows(w, f.dave).await;
    run(w, a, i, vec![note(i, "@dave 私下", true)]).await;
    dispatch(w).await;
    assert_eq!(rows(w, f.dave).await, dave_rows);
    assert_eq!(unread(w, f.dave_ctx).await, dave_unread);
    // A private issue dave cannot see, its description naming him: nothing.
    let (_, _) = create(
        w,
        a,
        w.web,
        json!({"subject": "m16", "is_private": true, "description": "@dave"}),
    )
    .await;
    dispatch(w).await;
    assert_eq!(rows(w, f.dave).await, dave_rows);
    // dave can read a public note naming him.
    run(w, a, i, vec![note(i, "@dave 公開", false)]).await;
    dispatch(w).await;
    assert!(mentioned(&item(w, f.dave_ctx, &key).await));

    // T-M2-17: an agent's note naming carol: a chip, no row, no notice.
    let deleg = w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(deleg.context_id).await;
    let (j, jkey) = create(w, a, w.web, json!({"subject": "m17"})).await;
    let carol_rows = rows(w, w.carol).await;
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(
            vec![note(j, "@carol ping", false)],
            vec![(j, lv(w, j).await)],
        ),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
    dispatch(w).await;
    assert_eq!(rows(w, w.carol).await, carol_rows);
    let v = spa(w, a, &jkey).await;
    let n = &v.view.recent_notes.as_ref().unwrap().items[0];
    assert!(
        n.body_html.as_deref().unwrap().contains(r#"title="carol""#),
        "{:?}",
        n.body_html
    );

    // T-M2-18: @bob, then @bob @carol: only carol hears of the edit.
    let (k, kkey) = create(w, a, w.web, json!({"subject": "m18"})).await;
    run(w, a, k, vec![set_desc(k, "@bob")]).await;
    dispatch(w).await;
    assert!(mentioned(&item(w, f.bob_ctx, &kkey).await));
    notifications::read_all(&w.app, f.bob_ctx, &ReadAllInput { before: Utc::now() })
        .await
        .unwrap();
    assert_eq!(unread(w, f.bob_ctx).await, 0);
    run(w, a, k, vec![set_desc(k, "@bob @carol")]).await;
    dispatch(w).await;
    assert_eq!(unread(w, f.bob_ctx).await, 0);
    assert!(mentioned(&item(w, f.carol_ctx, &kkey).await));

    // T-M2-19: carol leaves WEB: the notice is gone when read.
    unmember(w, w.web, w.carol).await;
    assert!(item(w, f.carol_ctx, &kkey).await.is_none());
    member(&pool, w.web, w.carol, w.dev).await;

    // T-M2-21: redacting the note hides the event from bob.
    grant(&pool, w.dev, &["manage_project"]).await;
    let (m, mkey) = create(w, a, w.web, json!({"subject": "m21"})).await;
    run(w, a, m, vec![note(m, "@bob 這段要遮蔽", false)]).await;
    dispatch(w).await;
    assert!(mentioned(&item(w, f.bob_ctx, &mkey).await));
    let note_id: Uuid = sqlx::query_scalar("SELECT id FROM issue_notes WHERE issue_id = $1")
        .bind(m)
        .fetch_one(&pool)
        .await
        .unwrap();
    redaction::redact(
        &w.app,
        a,
        &w.policy,
        Uuid::now_v7(),
        &Target::Note {
            key: mkey.clone(),
            note: note_id,
        },
        &RedactInput {
            reason_code: "personal_data".into(),
            reason: "test".into(),
        },
    )
    .await
    .unwrap();
    let it = item(w, f.bob_ctx, &mkey).await;
    assert!(it.is_none(), "{it:?}");

    // A retracted note (undo, §16.7) is hidden the same way. Today only an
    // agent's note is retracted, and an agent's note records no mention
    // (R-MN4); the read check still drops it, set here by hand.
    let (r, rkey) = create(w, a, w.web, json!({"subject": "retract"})).await;
    run(w, a, r, vec![note(r, "@bob 會撤回", false)]).await;
    dispatch(w).await;
    assert!(mentioned(&item(w, f.bob_ctx, &rkey).await));
    let by: Uuid =
        sqlx::query_scalar("INSERT INTO execution_refs (kind) VALUES ('command') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE issue_notes SET retracted_at = now(), retracted_by_execution_id = $2
         WHERE issue_id = $1",
    )
    .bind(r)
    .bind(by)
    .execute(&pool)
    .await
    .unwrap();
    let it = item(w, f.bob_ctx, &rkey).await;
    assert!(it.is_none(), "{it:?}");

    // on_mentioned off: no notice; on again: notices.
    prefs_put(w, f.bob_ctx, json!({"on_mentioned": false})).await;
    let p = notifications::preferences(&w.app, f.bob_ctx).await.unwrap();
    assert!(!p.on_mentioned);
    let (o, okey) = create(w, a, w.web, json!({"subject": "off"})).await;
    run(w, a, o, vec![note(o, "@bob off", false)]).await;
    dispatch(w).await;
    assert!(item(w, f.bob_ctx, &okey).await.is_none());
    prefs_put(w, f.bob_ctx, json!({"on_mentioned": true})).await;
    run(w, a, o, vec![note(o, "@bob on", false)]).await;
    dispatch(w).await;
    assert!(mentioned(&item(w, f.bob_ctx, &okey).await));
}

// ---------------------------------------------------------------- search

async fn keys_as(app: &App, who: AuthContext, q: &str) -> Vec<String> {
    let mut k: Vec<String> = service::search(
        app,
        who,
        SearchParams {
            q: Some(q.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .items
    .into_iter()
    .map(|i| i.key)
    .collect();
    k.sort();
    k
}

/// T-M2-12: descriptions and notes are searched as their plain text; a
/// schema-3 index (Markdown source) serves until the upgrade rebuild.
#[tokio::test]
async fn t_m2_12_plain_text_search() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let a = w.alice_ctx;
    let (i, key) = create(
        w,
        a,
        w.web,
        json!({"subject": "s1", "description": "**登入**失敗，請看[文件](https://docs.zzz.example/x)"}),
    )
    .await;
    run(
        w,
        a,
        i,
        vec![note(i, "見[鏈結](https://qqq.example/p)", false)],
    )
    .await;
    let (p, pkey) = create(w, a, w.web, json!({"subject": "s2"})).await;
    // A plain description (pre-M2) is indexed as it is.
    exec(
        &pool,
        &format!(
            "UPDATE issues SET description = 'see https://ppp.example', description_format = 'plain' WHERE id = '{p}'"
        ),
    )
    .await;

    // An index written by M1.7 (schema 3): the Markdown source.
    let index = SearchIndex::open(None).unwrap();
    index.set_first_build(Some(Layout::RawText));
    let app = Arc::new(App {
        pool: pool.clone(),
        index,
    });
    app.index.sync_all(&pool).await.unwrap();
    assert_eq!(app.index.view().unwrap().layout(), Layout::RawText);
    assert!(app.index.needs_upgrade());
    assert_eq!(keys_as(&app, a, "zzz").await, [key.as_str()]);
    assert_eq!(keys_as(&app, a, "qqq").await, [key.as_str()]);
    // The index itself (before the SQL re-check, which would hide a stale
    // hit): which terms it holds.
    let indexed = |ix: &SearchIndex, q: &str| ix.search(q, &Pred::True, 10).unwrap().len();
    assert_eq!(indexed(&app.index, "zzz"), 1);
    assert!(app.index.claim_upgrade());
    app.index.rebuild(&pool).await.unwrap();
    assert!(!app.index.needs_upgrade());
    assert_eq!(app.index.view().unwrap().layout(), Layout::Current);
    for q in ["zzz", "qqq", "docs"] {
        assert_eq!(indexed(&app.index, q), 0, "{q} is not in the plain text");
    }
    assert_eq!(indexed(&app.index, "文件"), 1);
    assert_eq!(indexed(&app.index, "鏈結"), 1);
    for (q, want) in [
        ("登入失敗", vec![key.clone()]),
        ("文件", vec![key.clone()]),
        ("鏈結", vec![key.clone()]),
        ("zzz", vec![]),
        ("qqq", vec![]),
        // Only the plain description still has its URL.
        ("https", vec![pkey.clone()]),
        ("ppp", vec![pkey.clone()]),
    ] {
        assert_eq!(keys_as(&app, a, q).await, want, "{q}");
    }
    // Matching is on the plain text; evidence quotes the stored text as it
    // is (citations must match the source verbatim).
    let r = service::search(
        &app,
        a,
        SearchParams {
            q: Some("登入失敗".into()),
            explain: Some("full".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let r = serde_json::to_value(r).unwrap();
    let ev = &r["items"][0]["evidence"];
    let stored = "**登入**失敗，請看[文件](https://docs.zzz.example/x)";
    let quoted: Vec<&str> = ev
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["field"] == "description")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert!(!quoted.is_empty(), "{r}");
    assert!(quoted.iter().all(|t| stored.contains(t)), "{quoted:?}");
    assert!(quoted.iter().any(|t| t.contains("**")), "{quoted:?}");
    // The world's own index (built Current) agrees, and keeps agreeing
    // after edits sync.
    w.app.index.sync_all(&pool).await.unwrap();
    assert_eq!(keys_as(&w.app, a, "zzz").await, Vec::<String>::new());
    run(
        w,
        a,
        i,
        vec![set_desc(i, "改成[新連結](https://yyy.example)")],
    )
    .await;
    w.app.index.sync_all(&pool).await.unwrap();
    assert_eq!(keys_as(&w.app, a, "新連結").await, [key.as_str()]);
    assert_eq!(keys_as(&w.app, a, "yyy").await, Vec::<String>::new());
    assert_eq!(keys_as(&w.app, a, "文件").await, Vec::<String>::new());
    assert_eq!(indexed(&w.app.index, "yyy"), 0);
    assert_eq!(indexed(&w.app.index, "新連結"), 1);
    // M10: the index lags (still "kkk" in the text) while the stored
    // Markdown has kkk only in a link target: the SQL re-check reads the
    // plain text too, so the stale hit is dropped.
    run(w, a, i, vec![set_desc(i, "kkk 在文字裡")]).await;
    w.app.index.sync_all(&pool).await.unwrap();
    assert_eq!(keys_as(&w.app, a, "kkk").await, [key.as_str()]);
    run(w, a, i, vec![set_desc(i, "[別處](https://kkk.example/)")]).await;
    assert_eq!(
        indexed(&w.app.index, "kkk"),
        1,
        "the index has not caught up"
    );
    assert_eq!(keys_as(&w.app, a, "kkk").await, Vec::<String>::new());
}

// ---------------------------------------------------------------- HTTP

const ORIGIN_SPA: &str = "http://localhost:5173";

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    )
}

struct Browser {
    session: String,
    csrf: String,
}

fn cookie_value(resp: &axum::response::Response, name: &str) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{name}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap()
}

async fn login(r: &Router, login: &str) -> Browser {
    let lc = r
        .clone()
        .oneshot(
            Request::get("/api/v1/auth/login-context")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = cookie_value(&lc, LOGIN_COOKIE);
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::ORIGIN, ORIGIN_SPA)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={cookie}"))
                .header("x-csrf-token", &token)
                .body(Body::from(
                    json!({"login": login, "password": PW}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session = cookie_value(&resp, SESSION_COOKIE);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

async fn call(
    r: &Router,
    b: &Browser,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::ORIGIN, ORIGIN_SPA)
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
        .header("x-csrf-token", &b.csrf);
    if method != "GET" {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    let resp = r.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The M2.1 routes over HTTP: preview (200, 404 alike, 413, unknown
/// field), the `*_html` fields, `on_mentioned`.
#[tokio::test]
async fn m2_1_http() {
    let _s = SERIAL.lock().await;
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(
        w,
        w.alice_ctx,
        w.web,
        json!({"subject": "h", "description": "**粗** @bob"}),
    )
    .await;
    run(w, w.alice_ctx, i, vec![note(i, "_斜_", false)]).await;
    let (_, secret) = create(
        w,
        f.bob_ctx,
        w.auth_p,
        json!({"subject": "s", "is_private": true}),
    )
    .await;
    let r = router(w);
    let dave = login(&r, "dave").await;
    let alice = login(&r, "alice").await;

    let (s, body) = call(
        &r,
        &alice,
        "POST",
        "/markdown/preview",
        Some(json!({"context": {"project": "WEB"}, "text": "**x** @bob"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(
        body["data"]["html"]
            .as_str()
            .unwrap()
            .contains("<strong>x</strong>"),
        "{body}"
    );
    assert!(
        body["data"]["html"]
            .as_str()
            .unwrap()
            .contains("ox-mention"),
        "{body}"
    );
    assert_eq!(body["data"]["format_version"], 1);
    let (s1, b1) = call(
        &r,
        &dave,
        "POST",
        "/markdown/preview",
        Some(json!({"context": {"issue": secret}, "text": "x"})),
    )
    .await;
    let (s2, b2) = call(
        &r,
        &dave,
        "POST",
        "/markdown/preview",
        Some(json!({"context": {"issue": "AUTH-9999"}, "text": "x"})),
    )
    .await;
    assert_eq!(s1, StatusCode::NOT_FOUND);
    assert_eq!(
        (s1, &b1["error"]["code"], &b1["error"]["details"]),
        (s2, &b2["error"]["code"], &b2["error"]["details"])
    );
    let (s, b) = call(&r, &alice, "POST", "/markdown/preview",
        Some(json!({"context": {"project": "WEB"}, "text": "x".repeat(markdown::PREVIEW_MAX_BYTES + 1)}))).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert_eq!(b["error"]["code"], "payload_too_large");
    let (s, _) = call(
        &r,
        &alice,
        "POST",
        "/markdown/preview",
        Some(json!({"context": {"project": "WEB"}, "text": "x", "extra": 1})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, v) = call(
        &r,
        &alice,
        "GET",
        &format!("/issues/{key}/view?include=recent_notes"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let d = &v["data"];
    assert!(
        d["description_html"]
            .as_str()
            .unwrap()
            .contains("<strong>粗</strong>"),
        "{d}"
    );
    assert!(
        d["description_html"]
            .as_str()
            .unwrap()
            .contains("ox-mention"),
        "{d}"
    );
    assert_eq!(d["description_by_agent"], false);
    assert!(
        d["recent_notes"]["items"][0]["body_html"]
            .as_str()
            .unwrap()
            .contains("<em>斜</em>"),
        "{d}"
    );
    let (s, v) = call(&r, &alice, "GET", &format!("/issues/{key}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v["data"]["description_html"]
            .as_str()
            .unwrap()
            .contains("<strong>粗</strong>"),
        "{v}"
    );

    let (s, p) = call(&r, &alice, "GET", "/me/notification-preferences", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p["data"]["on_mentioned"], true, "{p}");
}

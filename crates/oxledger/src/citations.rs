// SPDX-License-Identifier: GPL-2.0-or-later
//! Citations (design 03 §3.6): `cit1.` + base64url JSON naming one source
//! unit, its `source_rev` and a code point range, plus the shared verifier
//! behind `verify_citations`, `get_source` and the evidence check of a
//! proposal (contract 02 §6.7 step 7).
//!
//! Unsigned on purpose: a citation is valid when the current text at that
//! range is the quote and the caller can read the source right now. Every
//! check reloads the caller's read scope (people: their visibility; agents:
//! the delegated read scope, §3.8), and "missing" and "not allowed" give the
//! same answer.

use crate::auth::{self, AuthContext};
use crate::db;
use crate::error::{ApiError, ErrorCode, NextAction, api};
use crate::evidence::{CpText, Highlight, Strength};
use crate::perm::{self, Actor, ProjectPerms};
use crate::service::{App, read_tx};
use anyhow::Result;
use ox_core::{IssueFacts, Scope};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

pub const PREFIX: &str = "cit1.";
/// Longest quote that can be verified directly (code points).
pub const MAX_QUOTE: usize = 2000;
/// `verify_citations` batch limit.
pub const MAX_BATCH: usize = 50;
const MAX_ID_LEN: usize = 1024;
/// Upper bound for a range end; real sources are far shorter.
const MAX_OFFSET: u64 = 1 << 24;

/// The text field of an issue a citation can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextField {
    Subject,
    Description,
}

impl TextField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::Description => "description",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "subject" => Some(Self::Subject),
            "description" => Some(Self::Description),
            _ => None,
        }
    }
}

/// A source unit (§3.2). A closed set: an unknown kind is `invalid_citation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    IssueField { issue_id: Uuid, field: TextField },
    IssueNote { issue_id: Uuid, note_id: Uuid },
}

impl Source {
    fn to_json(self) -> Value {
        match self {
            Source::IssueField { issue_id, field } => {
                json!({"kind": "issue_field", "issue_id": issue_id, "field": field})
            }
            Source::IssueNote { issue_id, note_id } => {
                json!({"kind": "issue_note", "issue_id": issue_id, "note_id": note_id})
            }
        }
    }

    fn from_json(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let uuid = |k: &str| o.get(k)?.as_str()?.parse::<Uuid>().ok();
        match o.get("kind")?.as_str()? {
            "issue_field" if o.len() == 3 => Some(Source::IssueField {
                issue_id: uuid("issue_id")?,
                field: TextField::parse(o.get("field")?.as_str()?)?,
            }),
            "issue_note" if o.len() == 3 => Some(Source::IssueNote {
                issue_id: uuid("issue_id")?,
                note_id: uuid("note_id")?,
            }),
            _ => None,
        }
    }
}

/// A decoded citation: `[start, end)` in code points of the whole source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Citation {
    pub src: Source,
    pub rev: [u8; 32],
    pub start: usize,
    pub end: usize,
}

/// `source_rev`: SHA-256 of the raw UTF-8 text (§3.2).
pub fn source_rev(text: &str) -> [u8; 32] {
    auth::sha256(text.as_bytes())
}

pub fn encode(c: &Citation) -> String {
    let body = json!({
        "v": 1, "src": c.src.to_json(), "rev": auth::encode(&c.rev),
        "start": c.start, "end": c.end,
    });
    format!("{PREFIX}{}", auth::encode(body.to_string().as_bytes()))
}

pub fn cite(src: Source, text: &str, start: usize, end: usize) -> String {
    encode(&Citation {
        src,
        rev: source_rev(text),
        start,
        end,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    v: u64,
    src: Value,
    rev: String,
    start: u64,
    end: u64,
}

/// Decodes and checks the shape, the version, the kind, the rev length and
/// the range bounds. `None` = `invalid_citation`.
pub fn decode(id: &str) -> Option<Citation> {
    if id.len() > MAX_ID_LEN {
        return None;
    }
    let bytes = auth::decode_bytes(id.strip_prefix(PREFIX)?)?;
    let raw: Raw = serde_json::from_slice(&bytes).ok()?;
    if raw.v != 1 || raw.start >= raw.end || raw.end > MAX_OFFSET {
        return None;
    }
    let rev: [u8; 32] = auth::decode_bytes(&raw.rev)?.try_into().ok()?;
    Some(Citation {
        src: Source::from_json(&raw.src)?,
        rev,
        start: raw.start as usize,
        end: raw.end as usize,
    })
}

// ---------------------------------------------------------------- reading sources

/// A source read under the caller's scope.
pub(crate) struct Loaded {
    pub text: String,
    pub key: String,
}

/// The caller's read scope, loaded once per request (§3.8: every tool uses
/// the same authorization context). Building it re-checks the credential,
/// the agent's work context and R's epochs.
pub(crate) struct Reader {
    scope: Scope,
    actor: Actor,
    perms: HashMap<Uuid, Option<ProjectPerms>>,
}

impl Reader {
    pub async fn new(conn: &mut PgConnection, caller: AuthContext) -> Result<Self> {
        // An agent whose context has no usable delegation left reads nothing:
        // the whole request fails instead of answering not_found item by item
        // (design 03 §3.8, §3.9).
        let me = db::effective(&mut *conn, caller).await?;
        if me.agent.as_ref().is_some_and(|a| a.projects.is_empty()) {
            return Err(api(
                ErrorCode::DelegationUnavailable,
                "this agent has no active delegation; ask the delegator to delegate a project",
            ));
        }
        let scope = db::load_scope(&mut *conn, caller).await?;
        let actor = perm::actor(&mut *conn, caller).await?;
        Ok(Reader {
            scope,
            actor,
            perms: HashMap::new(),
        })
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn is_agent(&self) -> bool {
        self.actor.is_agent()
    }

    /// Private note rule (Redmine `Journal.visible`): readable with
    /// `view_private_notes` on the issue's tracker, or by its author. Never
    /// for agents (§3.5), whatever the delegator may read.
    pub async fn note_visible(
        &mut self,
        conn: &mut PgConnection,
        project: Uuid,
        tracker: Uuid,
        is_private: bool,
        author: Uuid,
        on_behalf_of: Option<Uuid>,
    ) -> Result<bool> {
        if !is_private {
            return Ok(true);
        }
        if self.actor.is_agent() {
            return Ok(false);
        }
        if perm::is_own(self.actor.user_id, author, on_behalf_of) {
            return Ok(true);
        }
        if !self.perms.contains_key(&project) {
            let pp = perm::project_perms(&mut *conn, &self.actor, project).await?;
            self.perms.insert(project, pp);
        }
        Ok(self.perms[&project]
            .as_ref()
            .is_some_and(|pp| pp.allowed(perm::VIEW_PRIVATE_NOTES, tracker)))
    }

    /// Reads a source if the caller may read it now; `None` for missing and
    /// not readable alike. A note is looked up by `note.id AND note.issue_id`
    /// together, so a readable issue cannot pull another issue's note.
    pub async fn source(
        &mut self,
        conn: &mut PgConnection,
        src: &Source,
    ) -> Result<Option<Loaded>> {
        type Row = (Uuid, Uuid, bool, Uuid, Option<Uuid>, String, String);
        type NoteRow = (bool, Uuid, Option<Uuid>);
        let (row, note): (Option<Row>, Option<NoteRow>) = match *src {
            Source::IssueField { issue_id, field } => {
                let col = match field {
                    TextField::Subject => "i.subject",
                    TextField::Description => "i.description",
                };
                let sql = format!(
                    "SELECT i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                            k.key, {col}
                     FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
                     WHERE i.id = $1 AND i.archived_at IS NULL"
                );
                let r: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(issue_id)
                    .fetch_optional(&mut *conn)
                    .await?;
                (r, None)
            }
            Source::IssueNote { issue_id, note_id } => {
                type NRow = (
                    Uuid,
                    Uuid,
                    bool,
                    Uuid,
                    Option<Uuid>,
                    String,
                    String,
                    bool,
                    Uuid,
                    Option<Uuid>,
                );
                let r: Option<NRow> = sqlx::query_as(
                    "SELECT i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                            k.key, n.body, n.is_private, n.author_id, n.created_on_behalf_of_id
                     FROM issue_notes n
                     JOIN issues i ON i.id = n.issue_id
                     JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
                     WHERE n.id = $1 AND n.issue_id = $2 AND i.archived_at IS NULL",
                )
                .bind(note_id)
                .bind(issue_id)
                .fetch_optional(&mut *conn)
                .await?;
                match r {
                    None => (None, None),
                    Some((p, t, ip, a, asg, k, body, np, na, nb)) => {
                        (Some((p, t, ip, a, asg, k, body)), Some((np, na, nb)))
                    }
                }
            }
        };
        let Some((project_id, tracker_id, is_private, author_id, assigned_to_id, key, text)) = row
        else {
            return Ok(None);
        };
        let facts = IssueFacts {
            project_id,
            tracker_id,
            is_private,
            author_id,
            assigned_to_id,
        };
        if !self.scope.can_see(&facts) {
            return Ok(None);
        }
        if let Some((np, na, nb)) = note
            && !self
                .note_visible(conn, project_id, tracker_id, np, na, nb)
                .await?
        {
            return Ok(None);
        }
        Ok(Some(Loaded { text, key }))
    }
}

// ---------------------------------------------------------------- statuses

/// Per-citation outcome (§3.7 table). Not an error: no error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CitationStatus {
    Valid,
    Moved,
    Ambiguous,
    SourceChanged,
    CitationMismatch,
    NotFound,
    InvalidCitation,
}

impl CitationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Moved => "moved",
            Self::Ambiguous => "ambiguous",
            Self::SourceChanged => "source_changed",
            Self::CitationMismatch => "citation_mismatch",
            Self::NotFound => "not_found",
            Self::InvalidCitation => "invalid_citation",
        }
    }

    /// §3.7 per-item table; `not_found` and `invalid_citation` as their error codes.
    pub fn next_actions(self) -> &'static [NextAction] {
        use NextAction::*;
        match self {
            Self::Valid => &[],
            Self::Moved | Self::SourceChanged | Self::CitationMismatch => &[FixInput],
            Self::Ambiguous => &[FixInput, AskHuman],
            Self::NotFound => ErrorCode::NotFound.next_actions(),
            Self::InvalidCitation => ErrorCode::InvalidCitation.next_actions(),
        }
    }
}

/// `exact` (a changed source is `source_changed`) or `reanchor` (find the
/// quote in the current text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Exact,
    Reanchor,
}

fn parse_mode(m: Option<&str>, path: &str) -> Result<Mode> {
    match m {
        None | Some("exact") => Ok(Mode::Exact),
        Some("reanchor") => Ok(Mode::Reanchor),
        Some(_) => Err(invalid_arg(path, &["exact", "reanchor"])),
    }
}

fn invalid_arg(path: &str, allowed: &[&str]) -> anyhow::Error {
    ApiError::new(
        ErrorCode::InvalidArgument,
        format!("{path} must be one of {}", allowed.join(", ")),
    )
    .with_details(json!({"path": path, "allowed": allowed}))
    .into()
}

fn quote_err(path: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::InvalidArgument,
        format!("{path} must be 1–{MAX_QUOTE} characters"),
    )
    .with_details(json!({"path": path}))
    .into()
}

fn field_name(src: &Source) -> &'static str {
    match src {
        Source::IssueField { field, .. } => field.as_str(),
        Source::IssueNote { .. } => "note",
    }
}

fn note_id(src: &Source) -> Option<Uuid> {
    match src {
        Source::IssueNote { note_id, .. } => Some(*note_id),
        Source::IssueField { .. } => None,
    }
}

/// Where a check landed: the status and, when the source was readable, the
/// current range the quote refers to.
struct Outcome {
    status: CitationStatus,
    count: usize,
    found: Option<(Citation, Loaded, usize, usize)>,
}

impl Outcome {
    fn only(status: CitationStatus) -> Self {
        Outcome {
            status,
            count: 0,
            found: None,
        }
    }
}

/// The verification order of §3.6 for one item. `subquote`: the quote is
/// looked for inside the cited range instead of having to equal it.
async fn check(
    reader: &mut Reader,
    conn: &mut PgConnection,
    citation_id: &str,
    quote: &str,
    mode: Mode,
    subquote: bool,
) -> Result<Outcome> {
    // 1. decode (and a range that cannot be verified directly).
    let Some(c) = decode(citation_id) else {
        return Ok(Outcome::only(CitationStatus::InvalidCitation));
    };
    if !subquote && c.end - c.start > MAX_QUOTE {
        return Ok(Outcome::only(CitationStatus::InvalidCitation));
    }
    // 2. read under the caller's scope.
    let Some(loaded) = reader.source(&mut *conn, &c.src).await? else {
        return Ok(Outcome::only(CitationStatus::NotFound));
    };
    let cp = CpText::new(&loaded.text);
    let q: Vec<char> = quote.chars().collect();
    let found = |status, s: usize, loaded: Loaded| Outcome {
        status,
        count: 1,
        found: Some((c, loaded, s, s + q.len())),
    };
    if source_rev(&loaded.text) == c.rev {
        // 3. same text: compare the range.
        if c.end > cp.len() {
            return Ok(Outcome::only(CitationStatus::InvalidCitation));
        }
        if !subquote {
            return Ok(if cp.chars[c.start..c.end] == q[..] {
                found(CitationStatus::Valid, c.start, loaded)
            } else {
                Outcome::only(CitationStatus::CitationMismatch)
            });
        }
        let at = cp.find_all(&q, (c.start, c.end));
        return Ok(match at.as_slice() {
            [one] => found(CitationStatus::Valid, *one, loaded),
            [] => Outcome::only(CitationStatus::CitationMismatch),
            many => Outcome {
                status: CitationStatus::Ambiguous,
                count: many.len(),
                found: None,
            },
        });
    }
    // 4. the source changed.
    if mode == Mode::Exact {
        return Ok(Outcome::only(CitationStatus::SourceChanged));
    }
    let at = cp.find_all(&q, (0, cp.len()));
    Ok(match at.as_slice() {
        [one] => found(CitationStatus::Moved, *one, loaded),
        [] => Outcome::only(CitationStatus::SourceChanged),
        many => Outcome {
            status: CitationStatus::Ambiguous,
            count: many.len(),
            found: None,
        },
    })
}

fn quote_ok(quote: &str) -> bool {
    let n = quote.chars().count();
    (1..=MAX_QUOTE).contains(&n)
}

/// Evidence check of a proposal (contract 02 §6.7 step 7): `mode: exact`,
/// under the caller's current read scope (an agent token: the delegated read
/// scope), on the caller's connection and transaction.
///
/// Returns `Valid`, `CitationMismatch`, `SourceChanged`, `NotFound` or
/// `InvalidCitation`. An empty quote or one over [`MAX_QUOTE`] code points is
/// `CitationMismatch` (it cannot equal a verifiable range). Credential or work
/// context failures are errors (`unauthenticated`, `context_unavailable`),
/// never a per-item `NotFound`.
pub async fn verify_one(
    conn: &mut PgConnection,
    caller: AuthContext,
    citation_id: &str,
    quote: &str,
) -> Result<CitationStatus> {
    let mut reader = Reader::new(&mut *conn, caller).await?;
    reader.verify_exact(conn, citation_id, quote).await
}

impl Reader {
    /// [`verify_one`] with an already loaded scope (several citations in one
    /// transaction).
    pub(crate) async fn verify_exact(
        &mut self,
        conn: &mut PgConnection,
        citation_id: &str,
        quote: &str,
    ) -> Result<CitationStatus> {
        if !quote_ok(quote) {
            return Ok(if decode(citation_id).is_none() {
                CitationStatus::InvalidCitation
            } else {
                CitationStatus::CitationMismatch
            });
        }
        Ok(check(self, conn, citation_id, quote, Mode::Exact, false)
            .await?
            .status)
    }
}

// ---------------------------------------------------------------- verify_citations

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CitationItem {
    /// A `citation_id` returned by `search`, `get` or `get_source`.
    pub citation_id: String,
    /// The cited text exactly as returned (1–2000 characters).
    pub quote: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifyCitationsParams {
    /// 1–50 citations.
    pub items: Vec<CitationItem>,
    /// "exact" (default): a changed source is `source_changed`. "reanchor": find the quote in the current text.
    pub mode: Option<String>,
}

/// One citation's result. Fields other than `status` and `next_actions`
/// appear only after the source was read under the caller's scope.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CitationResult {
    pub status: CitationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// subject | description | note
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// `moved`: the citation for the quote's new position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation_id: Option<String>,
    pub next_actions: &'static [NextAction],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct VerifyCitationsResult {
    /// In the order of `items`.
    pub results: Vec<CitationResult>,
}

/// `cite_valid`: also give a citation for a `valid` quote (`get_source`).
fn result_of(o: Outcome, path: &str, cite_valid: bool) -> CitationResult {
    let mut r = CitationResult {
        status: o.status,
        key: None,
        field: None,
        note_id: None,
        quote: None,
        citation_id: None,
        next_actions: o.status.next_actions(),
        details: None,
    };
    match o.status {
        CitationStatus::SourceChanged => {
            r.details = Some(json!({"suggested_tool": "get_source", "mode": "reanchor"}));
        }
        CitationStatus::Ambiguous => r.details = Some(json!({"count": o.count})),
        CitationStatus::InvalidCitation => r.details = Some(json!({"path": path})),
        _ => {}
    }
    if let Some((c, loaded, s, e)) = o.found {
        let cp = CpText::new(&loaded.text);
        r.key = Some(loaded.key.clone());
        r.field = Some(field_name(&c.src).into());
        r.note_id = note_id(&c.src);
        r.quote = Some(cp.slice(s, e));
        if o.status == CitationStatus::Moved || cite_valid {
            r.citation_id = Some(cite(c.src, &loaded.text, s, e));
        }
    }
    r
}

/// `verify_citations` (§3.6). Whole-request errors only for bad input, an
/// over-long batch and credential / context failures; everything about one
/// citation is reported in its own result.
pub async fn verify_citations(
    app: &App,
    caller: AuthContext,
    p: VerifyCitationsParams,
) -> Result<VerifyCitationsResult> {
    let mode = parse_mode(p.mode.as_deref(), "mode")?;
    if p.items.is_empty() || p.items.len() > MAX_BATCH {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!("items must hold 1–{MAX_BATCH} citations"),
        )
        .with_details(json!({"path": "items"}))
        .into());
    }
    for (i, it) in p.items.iter().enumerate() {
        if !quote_ok(&it.quote) {
            return Err(quote_err(&format!("items[{i}].quote")));
        }
    }
    let mut tx = read_tx(app).await?;
    let mut reader = Reader::new(&mut tx, caller).await?;
    let mut results = Vec::with_capacity(p.items.len());
    for (i, it) in p.items.iter().enumerate() {
        let o = check(
            &mut reader,
            &mut tx,
            &it.citation_id,
            &it.quote,
            mode,
            false,
        )
        .await?;
        results.push(result_of(o, &format!("items[{i}].citation_id"), false));
    }
    tx.commit().await?;
    Ok(VerifyCitationsResult { results })
}

// ---------------------------------------------------------------- get_source

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetSourceParams {
    pub citation_id: String,
    /// 1–2000 characters: the whole cited range (`selection: range`) or a part of it (`subquote`).
    pub quote: String,
    /// "range" (default): `quote` must equal the cited range. "subquote": find `quote` inside it (must occur once).
    pub selection: Option<String>,
    /// "exact" (default) or "reanchor" (when the source changed, find the quote in the current text).
    pub mode: Option<String>,
    /// Context on each side, 0–2000 characters (default 200). Context is for reading, not citing.
    pub context_chars: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SourceContext {
    pub text: String,
    /// Where the quote sits in `text` (code points, half-open).
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GetSourceResult {
    #[serde(flatten)]
    pub result: CitationResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<SourceContext>,
}

const DEFAULT_CONTEXT: u32 = 200;

/// `get_source` (§3.6): authorize, check the version, then return the exact
/// quote, a citation for it and some context from the same source unit.
pub async fn get_source(
    app: &App,
    caller: AuthContext,
    p: GetSourceParams,
) -> Result<GetSourceResult> {
    let mode = parse_mode(p.mode.as_deref(), "mode")?;
    let subquote = match p.selection.as_deref() {
        None | Some("range") => false,
        Some("subquote") => true,
        Some(_) => return Err(invalid_arg("selection", &["range", "subquote"])),
    };
    let ctx = p.context_chars.unwrap_or(DEFAULT_CONTEXT);
    if ctx > MAX_QUOTE as u32 {
        return Err(
            ApiError::new(ErrorCode::InvalidArgument, "context_chars must be 0–2000")
                .with_details(json!({"path": "context_chars"}))
                .into(),
        );
    }
    if !quote_ok(&p.quote) {
        return Err(quote_err("quote"));
    }
    let mut tx = read_tx(app).await?;
    let mut reader = Reader::new(&mut tx, caller).await?;
    let o = check(
        &mut reader,
        &mut tx,
        &p.citation_id,
        &p.quote,
        mode,
        subquote,
    )
    .await?;
    tx.commit().await?;
    let context = o.found.as_ref().map(|(_, loaded, s, e)| {
        let cp = CpText::new(&loaded.text);
        let from = s.saturating_sub(ctx as usize);
        let to = (e + ctx as usize).min(cp.len());
        SourceContext {
            text: cp.slice(from, to),
            highlights: vec![Highlight {
                start: s - from,
                end: e - from,
                strength: Strength::Primary,
            }],
        }
    });
    let result = result_of(o, "citation_id", true);
    Ok(GetSourceResult { result, context })
}

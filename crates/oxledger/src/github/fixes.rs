// SPDX-License-Identifier: GPL-2.0-or-later
//! `fixed_by` (design 06 §3.8, R-F1–R-F11): fix keywords in merged pull
//! requests and pushed commits, the project's "keyword → target status"
//! rules, and the transition the integration makes for a rule's sponsor.
//!
//! Decision A3 (a) + (i): a rule runs with the permissions and workflow of
//! the person who last enabled or changed it (the sponsor), and only a pull
//! request merged into the default branch moves an issue. A commit pushed
//! straight to the default branch leaves a hint (`direct_push`).
//!
//! Nothing in a webhook payload is trusted to move an issue: the worker
//! asks the GitHub API once per pull request whether it really is merged
//! into the default branch (R-F3), and each (issue, pull request) is
//! handled once (unique index; T-M2-69).

use super::client::GithubApi;
use super::jobs::Target;
use super::links::{self, ProjectAccess};
use super::refs;
use crate::auth::AuthContext;
use crate::commands::{self, Attribution, EvalCtx, Mode, SYSTEM_PRINCIPAL};
use crate::describe;
use crate::error::{ApiError, ErrorCode, classify};
use crate::perm::{self, MANAGE_PROJECT};
use crate::service::{App, read_tx};
use crate::time_reports::Named;
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

/// R-F2: GitHub's own closing keywords, prefilled in a new rule and what a
/// project without rules is hinted about.
pub const DEFAULT_KEYWORDS: [&str; 9] = [
    "fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved",
];
pub const MAX_RULES: usize = 5;
pub const MAX_KEYWORDS: usize = 10;
pub const MAX_KEYWORD_CHARS: usize = 30;
/// R-F3: fix links one pull request or one push may create.
pub const MAX_LINKS_PER_SOURCE: usize = 200;
/// Distinct issue keys looked up for one pull request or push.
pub const MAX_KEYS: usize = 1000;
/// R-F7: issues one pull request may move.
pub const MAX_AUTO: usize = 50;
/// Text scanned per title, body or commit message.
pub const MAX_SCAN_CHARS: usize = 65_536;
/// R-F1: commit messages of a pull request that are scanned.
pub const MAX_PR_COMMITS: usize = 250;
pub const MAX_ATTEMPTS: i16 = 5;
/// Pull requests confirmed in one pass of the worker.
const PASS_LIMIT: i64 = 20;

pub const ACTION_RULES_PUT: &str = "fix_rules_put";
pub const ACTION_FIXED_BY: &str = "fixed_by";
pub const ACTION_AUTO_DISABLE: &str = "fix_rule_auto_disable";

const NS_FIXED_BY: &[u8] = b"oxledger/fixed_by/v1\n";
const NS_AUTO_DISABLE: &[u8] = b"oxledger/fix_rule_auto_disable/v1\n";

/// The operation id of an integration write: derived from what it is about,
/// so the same fix link (or the same version of a rule) is one request.
/// (The contract says UUIDv5; that needs SHA-1, which is not a dependency:
/// this is the same idea over SHA-256, marked as a version 8 UUID.)
fn derived_id(ns: &[u8], parts: &[&[u8]]) -> Uuid {
    let mut h = Sha256::new();
    h.update(ns);
    for p in parts {
        h.update(p);
    }
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

// ---------------------------------------------------------------- scanning (R-F2)

fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A letter or digit of any script: `préfix AUTH-1` is not `fix`.
fn wordish(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `<keyword>[:]? <key>(, <key> | and <key>)*`: every (keyword, key) in
/// `text`, in order. Keywords (lower case) match whatever the case, as
/// whole words where they begin or end with an ASCII letter or digit; keys
/// are R-G1's.
pub fn scan(text: &str, keywords: &[String]) -> Vec<(String, String)> {
    let c: Vec<char> = text.chars().take(MAX_SCAN_CHARS).collect();
    let mut kws: Vec<Vec<char>> = keywords.iter().map(|k| k.chars().collect()).collect();
    kws.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let blank = |i: usize| i < c.len() && (c[i] == ' ' || c[i] == '\t');
    let key = |i: usize| {
        (i < c.len() && c[i].is_ascii_uppercase())
            .then(|| refs::key_at(&c, i))
            .flatten()
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let hit = kws.iter().find(|k| {
            let n = k.len();
            n > 0
                && i + n <= c.len()
                && c[i..i + n]
                    .iter()
                    .zip(k.iter())
                    .all(|(a, b)| a.to_lowercase().eq(std::iter::once(*b)))
                && !(word(k[0]) && i > 0 && wordish(c[i - 1]))
                && !(word(k[n - 1]) && i + n < c.len() && wordish(c[i + n]))
        });
        let Some(k) = hit else {
            i += 1;
            continue;
        };
        let mut j = i + k.len();
        if j < c.len() && c[j] == ':' {
            j += 1;
        }
        let gap = j;
        while blank(j) {
            j += 1;
        }
        let first = if j > gap { key(j) } else { None };
        let Some((found, mut end)) = first else {
            i += 1;
            continue;
        };
        let kw: String = k.iter().collect();
        out.push((kw.clone(), found));
        loop {
            let mut j = end;
            while blank(j) {
                j += 1;
            }
            if j < c.len() && c[j] == ',' {
                j += 1;
            } else if j > end
                && j + 3 <= c.len()
                && c[j..j + 3]
                    .iter()
                    .collect::<String>()
                    .eq_ignore_ascii_case("and")
                && blank(j + 3)
            {
                j += 3;
            } else {
                break;
            }
            while blank(j) {
                j += 1;
            }
            match key(j) {
                Some((more, e)) => {
                    out.push((kw.clone(), more));
                    end = e;
                }
                None => break,
            }
        }
        i = end;
    }
    out
}

// ---------------------------------------------------------------- rules: settings (§6.9)

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FixRule {
    pub id: Uuid,
    pub enabled: bool,
    /// Lower case, 1 to 10.
    pub keywords: Vec<String>,
    pub target_status_id: Uuid,
    pub target_status: String,
    /// null: every tracker.
    pub tracker_ids: Option<Vec<Uuid>>,
    /// Whose permissions and workflow the rule runs with: who last enabled
    /// or changed it.
    pub sponsor: Named,
    /// Why the rule is off: `sponsor_inactive`, `sponsor_lost_permission`,
    /// `status_removed`, `manual`.
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusChoice {
    pub id: Uuid,
    pub name: String,
    pub closed: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FixRules {
    pub rules: Vec<FixRule>,
    pub lock_version: i64,
    /// The project links a repository now (rules do nothing without one).
    pub has_linked_repo: bool,
    /// What a new rule starts with (GitHub's own closing keywords).
    pub default_keywords: Vec<String>,
    /// Statuses the project's trackers can reach.
    pub statuses: Vec<StatusChoice>,
    pub trackers: Vec<Named>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FixRuleInput {
    /// An existing rule; absent for a new one.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub enabled: bool,
    pub keywords: Vec<String>,
    pub target_status_id: Uuid,
    /// null or absent: every tracker.
    #[serde(default)]
    pub tracker_ids: Option<Vec<Uuid>>,
}

/// `PUT /projects/{id}/fix-rules`: the whole list.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FixRulesPut {
    pub rules: Vec<FixRuleInput>,
    pub lock_version: i64,
}

fn invalid(reason: &str, index: usize, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::FixRuleInvalid, msg)
        .with_details(json!({"reason": reason, "index": index}))
        .into()
}

type RuleRow = (
    Uuid,
    bool,
    Vec<String>,
    Uuid,
    String,
    Option<Vec<Uuid>>,
    Uuid,
    String,
    Option<String>,
);

async fn rule_rows(conn: &mut PgConnection, project: Uuid, lock: bool) -> Result<Vec<FixRule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT r.id, r.enabled, r.keywords, r.target_status_id, s.name, r.tracker_ids,
                r.sponsor_id, p.name, r.disabled_reason
         FROM project_fix_rules r
         JOIN issue_statuses s ON s.id = r.target_status_id
         JOIN principals p ON p.id = r.sponsor_id
         WHERE r.project_id = $1 ORDER BY r.position, r.id{}",
        if lock { " FOR UPDATE OF r" } else { "" }
    )))
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FixRule {
            id: r.0,
            enabled: r.1,
            keywords: r.2,
            target_status_id: r.3,
            target_status: r.4,
            tracker_ids: r.5,
            sponsor: Named { id: r.6, name: r.7 },
            disabled_reason: r.8,
        })
        .collect())
}

async fn view(conn: &mut PgConnection, project: Uuid) -> Result<FixRules> {
    let rules = rule_rows(conn, project, false).await?;
    let lock_version: i64 = sqlx::query_scalar(
        "SELECT fix_rules_version FROM project_tab_settings WHERE project_id = $1",
    )
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    let has_linked_repo: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM project_repo_links WHERE project_id = $1 AND unlinked_at IS NULL)",
    )
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    let statuses: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT s.id, s.name, s.category = 'done' FROM issue_statuses s
         WHERE EXISTS (SELECT 1 FROM workflow_transitions w
                       JOIN project_trackers t ON t.tracker_id = w.tracker_id AND t.project_id = $1
                       WHERE w.new_status_id = s.id)
         ORDER BY s.position, s.name",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    let trackers: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT t.id, t.name FROM trackers t JOIN project_trackers pt ON pt.tracker_id = t.id
         WHERE pt.project_id = $1 ORDER BY t.position, t.name",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(FixRules {
        rules,
        lock_version,
        has_linked_repo,
        default_keywords: DEFAULT_KEYWORDS.iter().map(|k| k.to_string()).collect(),
        statuses: statuses
            .into_iter()
            .map(|(id, name, closed)| StatusChoice { id, name, closed })
            .collect(),
        trackers: trackers
            .into_iter()
            .map(|(id, name)| Named { id, name })
            .collect(),
    })
}

fn must_manage(a: &ProjectAccess) -> Result<()> {
    if a.can_manage {
        Ok(())
    } else {
        Err(links::forbidden(
            "fix rules need manage_project in an active project",
        ))
    }
}

/// `GET /projects/{id}/fix-rules` (`manage_project`).
pub async fn get(app: &App, caller: AuthContext, project: &str) -> Result<FixRules> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let a = links::project_access(&mut tx, caller, project).await?;
    must_manage(&a)?;
    let v = view(&mut tx, a.project).await?;
    tx.commit().await?;
    Ok(v)
}

/// A keyword as stored: trimmed, lower case; `None` when it cannot be one.
fn keyword(raw: &str) -> Option<String> {
    let k = raw.trim().to_lowercase();
    let n = k.chars().count();
    let ok = (1..=MAX_KEYWORD_CHARS).contains(&n)
        && k.chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, ':' | ',' | '#'));
    ok.then_some(k)
}

struct Clean {
    id: Option<Uuid>,
    enabled: bool,
    keywords: Vec<String>,
    target: Uuid,
    trackers: Option<Vec<Uuid>>,
}

async fn validate(
    conn: &mut PgConnection,
    project: Uuid,
    input: &FixRulesPut,
) -> Result<Vec<Clean>> {
    if input.rules.len() > MAX_RULES {
        return Err(invalid(
            "too_many_rules",
            MAX_RULES,
            "a project has at most 5 fix rules",
        ));
    }
    let trackers: Vec<Uuid> =
        sqlx::query_scalar("SELECT tracker_id FROM project_trackers WHERE project_id = $1")
            .bind(project)
            .fetch_all(&mut *conn)
            .await?;
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (i, r) in input.rules.iter().enumerate() {
        if r.id.is_some() && input.rules[..i].iter().any(|x| x.id == r.id) {
            return Err(invalid("rule_duplicate", i, "the same rule id twice"));
        }
        if r.keywords.is_empty() {
            return Err(invalid("keywords_empty", i, "a rule needs a keyword"));
        }
        if r.keywords.len() > MAX_KEYWORDS {
            return Err(invalid(
                "too_many_keywords",
                i,
                "a rule has at most 10 keywords",
            ));
        }
        let mut kws = Vec::new();
        for raw in &r.keywords {
            let Some(k) = keyword(raw) else {
                return Err(invalid(
                    "keyword_invalid",
                    i,
                    "a keyword is 1 to 30 characters without spaces, ':', ',' or '#'",
                ));
            };
            if seen.contains(&k) {
                return Err(invalid(
                    "keyword_duplicate",
                    i,
                    "a keyword is used once in a project",
                ));
            }
            seen.push(k.clone());
            kws.push(k);
        }
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM issue_statuses WHERE id = $1)")
                .bind(r.target_status_id)
                .fetch_one(&mut *conn)
                .await?;
        if !known {
            return Err(invalid("status_unknown", i, "no such status"));
        }
        let mut tr = r.tracker_ids.clone().filter(|t| !t.is_empty());
        if let Some(t) = tr.as_mut() {
            t.sort();
            t.dedup();
            if t.iter().any(|x| !trackers.contains(x)) {
                return Err(invalid(
                    "tracker_not_in_project",
                    i,
                    "a tracker is not used by this project",
                ));
            }
        }
        out.push(Clean {
            id: r.id,
            enabled: r.enabled,
            keywords: kws,
            target: r.target_status_id,
            trackers: tr,
        });
    }
    Ok(out)
}

fn summary(rules: &[FixRule]) -> Json {
    Json::Array(
        rules
            .iter()
            .map(|r| {
                json!({"id": r.id, "enabled": r.enabled, "keywords": r.keywords,
                       "target_status_id": r.target_status_id, "tracker_ids": r.tracker_ids,
                       "sponsor_id": r.sponsor.id, "disabled_reason": r.disabled_reason})
            })
            .collect(),
    )
}

/// `PUT /projects/{id}/fix-rules` (Idempotency-Key): replaces the list.
/// The caller becomes the sponsor of every rule that changed or was turned
/// on (R-F8).
pub async fn put(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &FixRulesPut,
) -> Result<FixRules> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| links::not_found())?;
    let body = json!({"project": project, "put": input});
    let request = match links::open(&mut tx, &who, operation_id, ACTION_RULES_PUT, &body).await? {
        Ok(r) => r,
        Err(_) => {
            let a = links::access_of(&mut tx, caller, project).await?;
            must_manage(&a)?;
            let v = view(&mut tx, project).await?;
            tx.commit().await?;
            return Ok(v);
        }
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = links::access_of(&mut tx, caller, project).await?;
    must_manage(&a)?;
    // The project settings lock (02 §19.7).
    let version: i64 = sqlx::query_scalar(
        "SELECT fix_rules_version FROM project_tab_settings WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await?;
    if version != input.lock_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the fix rules changed; load them again",
        )
        .with_details(json!({"current_lock_version": version}))
        .into());
    }
    let clean = validate(&mut tx, project, input).await?;
    let before = rule_rows(&mut tx, project, true).await?;
    let kept: Vec<Uuid> = clean
        .iter()
        .filter_map(|c| c.id)
        .filter(|id| before.iter().any(|b| b.id == *id))
        .collect();
    sqlx::query("DELETE FROM project_fix_rules WHERE project_id = $1 AND NOT (id = ANY($2))")
        .bind(project)
        .bind(&kept)
        .execute(&mut *tx)
        .await?;
    for (pos, c) in clean.iter().enumerate() {
        let old = c.id.and_then(|id| before.iter().find(|b| b.id == id));
        match old {
            Some(o) => {
                let content = o.keywords != c.keywords
                    || o.target_status_id != c.target
                    || o.tracker_ids != c.trackers;
                let turned_on = c.enabled && !o.enabled;
                let turned_off = !c.enabled && o.enabled;
                let sponsor = if content || turned_on {
                    who.user_id
                } else {
                    o.sponsor.id
                };
                let reason = if c.enabled {
                    None
                } else if turned_off {
                    Some("manual".to_string())
                } else {
                    o.disabled_reason.clone()
                };
                sqlx::query(
                    "UPDATE project_fix_rules SET enabled = $2, keywords = $3, target_status_id = $4,
                            tracker_ids = $5, sponsor_id = $6, disabled_reason = $7, position = $8,
                            lock_version = lock_version + $9::int, updated_at = now()
                     WHERE id = $1",
                )
                .bind(o.id)
                .bind(c.enabled)
                .bind(&c.keywords)
                .bind(c.target)
                .bind(&c.trackers)
                .bind(sponsor)
                .bind(reason)
                .bind(pos as i16)
                .bind(i32::from(content || turned_on || turned_off))
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO project_fix_rules (project_id, enabled, sponsor_id, keywords, target_status_id,
                                                    tracker_ids, position)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(project)
                .bind(c.enabled)
                .bind(who.user_id)
                .bind(&c.keywords)
                .bind(c.target)
                .bind(&c.trackers)
                .bind(pos as i16)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    let after = rule_rows(&mut tx, project, false).await?;
    let changed = summary(&before) != summary(&after);
    if changed {
        sqlx::query(
            "UPDATE project_tab_settings SET fix_rules_version = fix_rules_version + 1 WHERE project_id = $1",
        )
        .bind(project)
        .execute(&mut *tx)
        .await?;
        links::event(
            &mut tx,
            project,
            "fix_rule_changed",
            who.user_id,
            request,
            json!({"before": summary(&before), "after": summary(&after)}),
        )
        .await?;
    }
    commands::complete_request(&mut tx, request, &json!({"changed": changed})).await?;
    let v = view(&mut tx, project).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- detection (R-F1, R-F3)

struct RuleLite {
    id: Uuid,
    project: Uuid,
    enabled: bool,
    keywords: Vec<String>,
    trackers: Option<Vec<Uuid>>,
}

/// Where a keyword was found.
#[derive(Debug, Clone, Copy)]
pub enum Found<'a> {
    /// A pull request merged into the default branch.
    Pr(Uuid),
    /// A commit pushed to the default branch; `head` is the push's `after`.
    Push { sha: &'a str, head: &'a str },
}

/// The rules of the projects that link one repository, loaded once per
/// event.
pub struct Detector {
    rules: Vec<RuleLite>,
    keywords: Vec<String>,
    /// Fix links this event may still create (R-F3).
    left: usize,
    pub truncated: bool,
}

impl Detector {
    pub async fn load(conn: &mut PgConnection, t: &Target) -> Result<Detector> {
        let projects: Vec<Uuid> = t.links.iter().map(|l| l.1).collect();
        type Row = (Uuid, Uuid, bool, Vec<String>, Option<Vec<Uuid>>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, project_id, enabled, keywords, tracker_ids FROM project_fix_rules
             WHERE project_id = ANY($1) ORDER BY project_id, position, id",
        )
        .bind(&projects)
        .fetch_all(&mut *conn)
        .await?;
        let rules: Vec<RuleLite> = rows
            .into_iter()
            .map(|r| RuleLite {
                id: r.0,
                project: r.1,
                enabled: r.2,
                keywords: r.3,
                trackers: r.4,
            })
            .collect();
        let mut keywords: Vec<String> = DEFAULT_KEYWORDS.iter().map(|k| k.to_string()).collect();
        for k in rules.iter().flat_map(|r| r.keywords.iter()) {
            if !keywords.contains(k) {
                keywords.push(k.clone());
            }
        }
        Ok(Detector {
            rules,
            keywords,
            left: MAX_LINKS_PER_SOURCE,
            truncated: false,
        })
    }

    /// Records the fix links `texts` ask for. With an enabled rule that
    /// matches: `pending` for a pull request, a `direct_push` hint for a
    /// push (A3 (i)). Without one: a `no_rule` hint, and the API is never
    /// asked (R-F3).
    pub async fn record(
        &mut self,
        conn: &mut PgConnection,
        t: &Target,
        found: Found<'_>,
        texts: &[&str],
    ) -> Result<()> {
        // (issue, project, link, rule, keyword), one per issue: an enabled
        // rule's keyword wins over a hint.
        let mut hits: Vec<(Uuid, Uuid, Uuid, Option<Uuid>, String)> = Vec::new();
        // The text may be an outsider's: one lookup per distinct key, and
        // no more than MAX_KEYS of them for one source.
        let mut known: HashMap<String, Option<(Uuid, Uuid, Uuid)>> = HashMap::new();
        for text in texts {
            for (kw, key) in scan(text, &self.keywords) {
                let row = match known.get(&key) {
                    Some(r) => *r,
                    None if known.len() >= MAX_KEYS => {
                        self.truncated = true;
                        continue;
                    }
                    None => {
                        let r: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                            "SELECT i.id, i.project_id, i.tracker_id FROM issue_keys k JOIN issues i ON i.id = k.issue_id
                             WHERE k.key = $1 LIMIT 1",
                        )
                        .bind(&key)
                        .fetch_optional(&mut *conn)
                        .await?;
                        known.insert(key, r);
                        r
                    }
                };
                let Some((issue, project, tracker)) = row else {
                    continue;
                };
                let Some((link, _)) = t.links.iter().find(|(_, p)| *p == project) else {
                    continue;
                };
                let of_project = |r: &&RuleLite| r.project == project && r.keywords.contains(&kw);
                let rule = self.rules.iter().find(of_project);
                if rule.is_none() && !DEFAULT_KEYWORDS.contains(&kw.as_str()) {
                    // Another project's keyword says nothing here.
                    continue;
                }
                let active = rule
                    .filter(|r| {
                        r.enabled && r.trackers.as_ref().is_none_or(|ts| ts.contains(&tracker))
                    })
                    .map(|r| r.id);
                match hits.iter_mut().find(|h| h.0 == issue) {
                    Some(h) if h.3.is_none() && active.is_some() => {
                        h.3 = active;
                        h.4 = kw;
                    }
                    Some(_) => {}
                    None => hits.push((issue, project, *link, active, kw)),
                }
            }
        }
        for (issue, project, link, rule, kw) in hits {
            if self.left == 0 {
                self.truncated = true;
                break;
            }
            let (state, reason) = match (&found, rule) {
                (Found::Pr(_), Some(_)) => ("pending", None),
                (Found::Push { .. }, Some(_)) => ("suggested", Some("direct_push")),
                (_, None) => ("suggested", Some("no_rule")),
            };
            let n = match found {
                Found::Pr(pr) => sqlx::query(
                    "INSERT INTO issue_fix_links (issue_id, project_id, repo_id, pr_id, link_id, rule_id, keyword,
                                                  state, reason, decided_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, CASE WHEN $8 = 'pending' THEN NULL ELSE now() END)
                     ON CONFLICT (issue_id, pr_id) WHERE pr_id IS NOT NULL DO NOTHING",
                )
                .bind(issue)
                .bind(project)
                .bind(t.repo)
                .bind(pr)
                .bind(link)
                .bind(rule)
                .bind(&kw)
                .bind(state)
                .bind(reason)
                .execute(&mut *conn)
                .await?
                .rows_affected(),
                // R-C7: a revoked (project, repo, sha) gets no fix link.
                Found::Push { sha, head } => sqlx::query(
                    "INSERT INTO issue_fix_links (issue_id, project_id, repo_id, sha, link_id, rule_id, keyword,
                                                  confirm_head, state, reason, decided_at)
                     SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now()
                     WHERE NOT EXISTS (SELECT 1 FROM project_commit_revocations v
                                       WHERE v.project_id = $2 AND v.repo_id = $3 AND v.sha = $4)
                     ON CONFLICT (issue_id, repo_id, sha) WHERE sha IS NOT NULL DO NOTHING",
                )
                .bind(issue)
                .bind(project)
                .bind(t.repo)
                .bind(sha)
                .bind(link)
                .bind(rule)
                .bind(&kw)
                .bind(head)
                .bind(state)
                .bind(reason)
                .execute(&mut *conn)
                .await?
                .rows_affected(),
            };
            if n > 0 {
                self.left -= 1;
            }
        }
        Ok(())
    }
}

/// R-F1 (a): a `pull_request` event that says the PR was merged into the
/// repository's default branch. Returns whether links were cut at 200.
pub async fn pr_merged(
    conn: &mut PgConnection,
    t: &Target,
    pr: Uuid,
    payload: &Json,
    commits: &[String],
) -> Result<bool> {
    let v = &payload["pull_request"];
    if payload["action"].as_str() != Some("closed") || v["merged"].as_bool() != Some(true) {
        return Ok(false);
    }
    let default: Option<String> =
        sqlx::query_scalar("SELECT default_branch FROM github_repos WHERE id = $1")
            .bind(t.repo)
            .fetch_one(&mut *conn)
            .await?;
    if default.is_none() || default.as_deref() != v["base"]["ref"].as_str() {
        return Ok(false);
    }
    let mut d = Detector::load(conn, t).await?;
    let mut texts: Vec<&str> = vec![
        v["title"].as_str().unwrap_or(""),
        // Only scanned for keywords: never stored, no ordinary ref (R-G1).
        v["body"].as_str().unwrap_or(""),
    ];
    texts.extend(commits.iter().take(MAX_PR_COMMITS).map(String::as_str));
    d.record(conn, t, Found::Pr(pr), &texts).await?;
    Ok(d.truncated)
}

/// Unlinking and revoking: the development data of a link goes with it.
pub async fn drop_link(conn: &mut PgConnection, link: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM issue_fix_links WHERE link_id = $1")
        .bind(link)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn drop_pr(conn: &mut PgConnection, project: Uuid, pr: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM issue_fix_links WHERE project_id = $1 AND pr_id = $2")
        .bind(project)
        .bind(pr)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn drop_commit(
    conn: &mut PgConnection,
    project: Uuid,
    repo: Uuid,
    sha: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM issue_fix_links WHERE project_id = $1 AND repo_id = $2 AND sha = $3")
        .bind(project)
        .bind(repo)
        .bind(sha)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- confirmation and execution

async fn decide_all(app: &App, pr: Uuid, state: &str, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE issue_fix_links SET state = $2, reason = $3, decided_at = now()
         WHERE pr_id = $1 AND state = 'pending'",
    )
    .bind(pr)
    .bind(state)
    .bind(reason)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// One more failed attempt for the rows in `ids`; after the fifth they are
/// `skipped` (`unconfirmed`).
async fn retry_later(app: &App, ids: &[Uuid], now: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "UPDATE issue_fix_links
         SET attempts = attempts + 1,
             next_at = $2 + make_interval(secs => 30 * power(2, attempts)::double precision),
             state = CASE WHEN attempts + 1 >= $3 THEN 'skipped' ELSE state END,
             reason = CASE WHEN attempts + 1 >= $3 THEN 'unconfirmed' ELSE reason END,
             decided_at = CASE WHEN attempts + 1 >= $3 THEN now() ELSE decided_at END
         WHERE id = ANY($1) AND state = 'pending'",
    )
    .bind(ids)
    .bind(now)
    .bind(MAX_ATTEMPTS)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// The worker's pass over due `pending` fix links: one API confirmation per
/// pull request, then one transaction per issue. Returns how many pull
/// requests it looked at.
pub async fn run_pending_once(app: &App, api: &dyn GithubApi, now: DateTime<Utc>) -> Result<usize> {
    let prs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT pr_id FROM issue_fix_links
         WHERE state = 'pending' AND next_at <= $1 AND pr_id IS NOT NULL
         GROUP BY pr_id ORDER BY min(next_at) LIMIT $2",
    )
    .bind(now)
    .bind(PASS_LIMIT)
    .fetch_all(&app.pool)
    .await?;
    for pr in &prs {
        if let Err(e) = confirm(app, api, *pr, now).await {
            tracing::warn!(pr = %pr, "fixed_by: {}", e.root_cause());
        }
    }
    Ok(prs.len())
}

async fn confirm(app: &App, api: &dyn GithubApi, pr: Uuid, now: DateTime<Utc>) -> Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM issue_fix_links WHERE pr_id = $1 AND state = 'pending' AND next_at <= $2 ORDER BY id",
    )
    .bind(pr)
    .bind(now)
    .fetch_all(&app.pool)
    .await?;
    if ids.is_empty() {
        return Ok(());
    }
    let src: Option<(String, i64, Option<String>, i32, bool)> = sqlx::query_as(
        "SELECT r.full_name, i.gh_installation_id, r.default_branch, p.number,
                r.state = 'accessible' AND i.state = 'active'
         FROM github_pull_requests p JOIN github_repos r ON r.id = p.repo_id
         JOIN github_installations i ON i.id = r.installation_id WHERE p.id = $1",
    )
    .bind(pr)
    .fetch_optional(&app.pool)
    .await?;
    let Some((full_name, inst, default, number, reachable)) = src else {
        return Ok(());
    };
    if !reachable {
        return retry_later(app, &ids, now).await;
    }
    // R-F3: the payload is not trusted. One call per pull request.
    let got = match api.get_pull(inst, &full_name, number).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(pr = %pr, "fixed_by: GitHub did not confirm: {}", e.root_cause());
            return retry_later(app, &ids, now).await;
        }
    };
    let Some(v) = got else {
        return decide_all(app, pr, "skipped", "unconfirmed").await;
    };
    let merged = v["merged"].as_bool() == Some(true) || !v["merged_at"].is_null();
    // The default branch is the API's too: `github_repos.default_branch`
    // is whatever the last payload said, and a payload can be an old one.
    let _ = default;
    let api_default = v["base"]["repo"]["default_branch"].as_str();
    let on_default = api_default.is_some() && v["base"]["ref"].as_str() == api_default;
    if !(merged && on_default) {
        return decide_all(app, pr, "skipped", "not_on_default_branch").await;
    }
    // R-F7: more than 50 → none moves by itself.
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issue_fix_links WHERE pr_id = $1 AND state = 'pending'",
    )
    .bind(pr)
    .fetch_one(&app.pool)
    .await?;
    if pending as usize > MAX_AUTO {
        return decide_all(app, pr, "suggested", "too_many").await;
    }
    for id in ids {
        if let Err(e) = apply_one(app, id).await {
            tracing::warn!(fix_link = %id, "fixed_by: not applied: {}", e.root_cause());
            retry_later(app, &[id], now).await?;
        }
    }
    Ok(())
}

enum Decision {
    Applied(Uuid),
    Suggested(&'static str),
    Skipped(&'static str),
    /// The link is gone: so is the fix link.
    Gone,
}

struct Pending {
    id: Uuid,
    issue: Uuid,
    project: Uuid,
    repo: Uuid,
    pr: Uuid,
    link: Uuid,
    rule: Option<Uuid>,
    keyword: String,
}

/// One fix link, one transaction (R-F4, R-F5): eligibility is checked
/// again on locked rows, then the transition is written by the command
/// engine as the integration acting for the sponsor.
async fn apply_one(app: &App, id: Uuid) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    // Lock order (unlink: link → fix links; PUT: settings → rules → fix
    // links): the link, the project's settings row, the rule, and only
    // then the fix link.
    let ahead: Option<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT link_id, project_id, rule_id FROM issue_fix_links WHERE id = $1 AND state = 'pending'",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((link0, project0, rule0)) = ahead else {
        return Ok(());
    };
    sqlx::query("SELECT 1 FROM project_repo_links WHERE id = $1 FOR SHARE")
        .bind(link0)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT 1 FROM project_tab_settings WHERE project_id = $1 FOR UPDATE")
        .bind(project0)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT 1 FROM project_fix_rules WHERE id = $1 FOR UPDATE")
        .bind(rule0)
        .execute(&mut *tx)
        .await?;
    type Row = (Uuid, Uuid, Uuid, Option<Uuid>, Uuid, Option<Uuid>, String);
    let row: Option<Row> = sqlx::query_as(
        "SELECT issue_id, project_id, repo_id, pr_id, link_id, rule_id, keyword
         FROM issue_fix_links WHERE id = $1 AND state = 'pending' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((issue, project, repo, Some(pr), link, rule, keyword)) = row else {
        return Ok(());
    };
    let p = Pending {
        id,
        issue,
        project,
        repo,
        pr,
        link,
        rule,
        keyword,
    };
    match decide(&mut tx, &p).await? {
        Decision::Applied(execution) => {
            sqlx::query(
                "UPDATE issue_fix_links SET state = 'applied', reason = NULL, execution_id = $2, decided_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(execution)
            .execute(&mut *tx)
            .await?;
        }
        Decision::Suggested(reason) => set_state(&mut tx, id, "suggested", reason).await?,
        Decision::Skipped(reason) => set_state(&mut tx, id, "skipped", reason).await?,
        Decision::Gone => {
            sqlx::query("DELETE FROM issue_fix_links WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn set_state(conn: &mut PgConnection, id: Uuid, state: &str, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE issue_fix_links SET state = $2, reason = $3, decided_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(state)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// R-F8: the sponsor can no longer back the rule. The rule goes off, with
/// a setting event by the system principal under its own operation request
/// (one per rule version).
async fn disable_rule(
    conn: &mut PgConnection,
    project: Uuid,
    rule: Uuid,
    version: i64,
    reason: &'static str,
) -> Result<()> {
    let op = derived_id(NS_AUTO_DISABLE, &[rule.as_bytes(), &version.to_be_bytes()]);
    let body = Sha256::digest(format!("{rule}\n{version}\n{reason}").as_bytes());
    let fp = commands::fingerprint(ACTION_AUTO_DISABLE, &body);
    let Some(request) =
        commands::open_request(&mut *conn, SYSTEM_PRINCIPAL, op, ACTION_AUTO_DISABLE, &fp).await?
    else {
        // This version of the rule was disabled before.
        return Ok(());
    };
    sqlx::query(
        "UPDATE project_fix_rules SET enabled = false, disabled_reason = $2, lock_version = lock_version + 1,
                updated_at = now() WHERE id = $1",
    )
    .bind(rule)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    // A settings page opened before this must not save the rule back on
    // (and under a new sponsor) without having seen why it went off.
    sqlx::query(
        "UPDATE project_tab_settings SET fix_rules_version = fix_rules_version + 1 WHERE project_id = $1",
    )
    .bind(project)
    .execute(&mut *conn)
    .await?;
    links::event(
        conn,
        project,
        "fix_rule_changed",
        SYSTEM_PRINCIPAL,
        request,
        json!({"rule_id": rule, "reason": reason}),
    )
    .await?;
    commands::complete_request(conn, request, &json!({"rule_id": rule, "reason": reason})).await?;
    Ok(())
}

async fn decide(conn: &mut PgConnection, p: &Pending) -> Result<Decision> {
    // §19.5: the link is held as the worker holds it; unlinking waits.
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM project_repo_links WHERE id = $1 AND unlinked_at IS NULL FOR SHARE",
    )
    .bind(p.link)
    .fetch_optional(&mut *conn)
    .await?;
    if live.is_none() {
        return Ok(Decision::Gone);
    }
    // Hidden in this project: its number is not written into the history.
    let hidden: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM project_pr_hidden WHERE project_id = $1 AND pr_id = $2)",
    )
    .bind(p.project)
    .bind(p.pr)
    .fetch_one(&mut *conn)
    .await?;
    if hidden {
        return Ok(Decision::Skipped("pr_hidden"));
    }
    type Rule = (bool, Uuid, Vec<String>, Uuid, Option<Vec<Uuid>>, i64);
    let rule: Option<Rule> = sqlx::query_as(
        "SELECT enabled, sponsor_id, keywords, target_status_id, tracker_ids, lock_version
         FROM project_fix_rules WHERE id = $1 AND project_id = $2 FOR UPDATE",
    )
    .bind(p.rule)
    .bind(p.project)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((true, sponsor, keywords, target, trackers, version)) = rule else {
        return Ok(Decision::Suggested("no_rule"));
    };
    if !keywords.contains(&p.keyword) {
        return Ok(Decision::Suggested("no_rule"));
    }
    let issue: Option<(Uuid, Uuid, i32, bool, bool)> = sqlx::query_as(
        "SELECT i.tracker_id, i.status_id, i.lock_version, s.category = 'done',
                EXISTS (SELECT 1 FROM issue_triage t WHERE t.issue_id = i.id AND t.state = 'pending')
         FROM issues i JOIN issue_statuses s ON s.id = i.status_id
         WHERE i.id = $1 AND i.project_id = $2 FOR UPDATE OF i",
    )
    .bind(p.issue)
    .bind(p.project)
    .fetch_optional(&mut *conn)
    .await?;
    // Moved to another project since: its own rules did not ask for this.
    let Some((tracker, status, lock_version, closed, triage)) = issue else {
        return Ok(Decision::Suggested("no_rule"));
    };
    if trackers.is_some_and(|ts| !ts.contains(&tracker)) {
        return Ok(Decision::Suggested("no_rule"));
    }
    if closed {
        return Ok(Decision::Suggested("already_closed"));
    }
    if triage {
        return Ok(Decision::Suggested("in_triage"));
    }
    if status == target {
        return Ok(Decision::Skipped("already_in_target"));
    }
    // A closed or archived project takes no writes; the rule stays as it is.
    let open: bool = sqlx::query_scalar("SELECT status = 'active' FROM projects WHERE id = $1")
        .bind(p.project)
        .fetch_one(&mut *conn)
        .await?;
    if !open {
        return Ok(Decision::Suggested("project_inactive"));
    }
    // R-F8: the sponsor still is an active user who manages the project.
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active')",
    )
    .bind(sponsor)
    .fetch_one(&mut *conn)
    .await?;
    if !active {
        disable_rule(
            conn,
            p.project,
            p.rule.unwrap_or_default(),
            version,
            "sponsor_inactive",
        )
        .await?;
        return Ok(Decision::Suggested("sponsor_inactive"));
    }
    let as_sponsor = AuthContext::rule_sponsor(sponsor);
    let actor = perm::actor(&mut *conn, as_sponsor).await?;
    let manages = perm::project_perms(&mut *conn, &actor, p.project)
        .await?
        .is_some_and(|pp| pp.allowed_any(MANAGE_PROJECT));
    if !manages {
        disable_rule(
            conn,
            p.project,
            p.rule.unwrap_or_default(),
            version,
            "sponsor_lost_permission",
        )
        .await?;
        return Ok(Decision::Suggested("sponsor_lost_permission"));
    }
    sqlx::query("SAVEPOINT fixed_by")
        .execute(&mut *conn)
        .await?;
    match transition(conn, p, as_sponsor, sponsor, target, lock_version).await {
        Ok(execution) => {
            sqlx::query("RELEASE SAVEPOINT fixed_by")
                .execute(&mut *conn)
                .await?;
            Ok(Decision::Applied(execution))
        }
        Err(e) => {
            let reason = match classify(&e).map(|a| a.code) {
                Some(ErrorCode::RequiredFieldMissing | ErrorCode::ReadonlyField) => "form_rule",
                // The sponsor cannot see the issue, or has no such
                // transition: one answer for both (R-F5, T-M2-70a).
                Some(
                    ErrorCode::NotFound
                    | ErrorCode::ForbiddenOp
                    | ErrorCode::InvalidTransition
                    | ErrorCode::InvalidValue,
                ) => "workflow_disallowed",
                Some(ErrorCode::TriageStateInvalid) => "in_triage",
                _ => return Err(e),
            };
            sqlx::query("ROLLBACK TO SAVEPOINT fixed_by")
                .execute(&mut *conn)
                .await?;
            Ok(Decision::Suggested(reason))
        }
    }
}

/// R-F5: the command document `transition`, evaluated as the sponsor and
/// written as the integration.
async fn transition(
    conn: &mut PgConnection,
    p: &Pending,
    as_sponsor: AuthContext,
    sponsor: Uuid,
    target: Uuid,
    lock_version: i32,
) -> Result<Uuid> {
    let on = json!({"type": "issue", "id": p.issue});
    let doc = json!({
        "command_schema_version": 1,
        "ops": [{"kind": "transition", "op_id": p.id, "target": on, "to_status_id": target}],
        "preconditions": [{"target": on, "lock_version": lock_version}],
    });
    let parsed = commands::parse_for(&mut *conn, &doc, 1, as_sponsor).await?;
    let fp = commands::fingerprint(ACTION_FIXED_BY, &parsed.hash);
    let op = derived_id(NS_FIXED_BY, &[p.id.as_bytes()]);
    let Some(request) =
        commands::open_request(&mut *conn, SYSTEM_PRINCIPAL, op, ACTION_FIXED_BY, &fp).await?
    else {
        bail!("fix link {} already has an operation request", p.id);
    };
    let ectx = EvalCtx {
        mode: Mode::Direct,
        caller: as_sponsor,
        ceiling: None,
        deadlines: Vec::new(),
        author: None,
    };
    let sim = commands::simulate(&mut *conn, &ectx, &parsed).await?;
    let consistent_after = sim.token.clone().unwrap_or_default();
    let execution = Uuid::now_v7();
    sqlx::query("INSERT INTO execution_refs (id, kind) VALUES ($1, 'command')")
        .bind(execution)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO command_executions (id, operation_request_id, actor_id, on_behalf_of_id, origin,
                                         command_hash, consistent_after)
         VALUES ($1, $2, $3, $4, 'integration', $5, $6)",
    )
    .bind(execution)
    .bind(request)
    .bind(SYSTEM_PRINCIPAL)
    .bind(sponsor)
    .bind(&parsed.hash[..])
    .bind(&consistent_after)
    .execute(&mut *conn)
    .await?;
    let (repo, number): (String, i32) = sqlx::query_as(
        "SELECT r.full_name, p.number FROM github_pull_requests p JOIN github_repos r ON r.id = p.repo_id
         WHERE p.id = $1 AND r.id = $2",
    )
    .bind(p.pr)
    .bind(p.repo)
    .fetch_one(&mut *conn)
    .await?;
    let at = Attribution {
        origin: "integration",
        actor_id: SYSTEM_PRINCIPAL,
        actor_kind: "system",
        author_id: SYSTEM_PRINCIPAL,
        author_on_behalf: None,
        request_id: request,
        command_execution_id: Some(execution),
        execution_ref: execution,
        changeset: None,
        changeset_execution_id: None,
        proposer_id: None,
        approver_id: None,
        on_behalf_of_id: Some(sponsor),
        delegation_id: None,
        agent_client_id: None,
        record_changes: false,
        system_operation: Some("fixed_by"),
        // Never the PR's title or a commit message.
        event_extra: Some((
            "fix",
            json!({"repo": repo, "pr_number": number, "keyword": p.keyword}),
        )),
        quiet: false,
    };
    let effects = commands::write_effects(&mut *conn, &parsed, &sim, &at).await?;
    commands::complete_request(
        &mut *conn,
        request,
        &json!({
            "execution_id": execution,
            "consistent_after": consistent_after,
            "issues": effects.iter().map(|e| e.issue_id).collect::<Vec<_>>(),
        }),
    )
    .await?;
    Ok(execution)
}

// ---------------------------------------------------------------- reading (R-F9)

/// A fix keyword found for an issue, as its development card shows it.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct DevFix {
    /// `owner/name`.
    pub repo: String,
    /// The pull request (with `repo`); null for a pushed commit.
    pub pr_number: Option<i32>,
    /// The pushed commit; null for a pull request.
    pub sha: Option<String>,
    pub keyword: String,
    /// `pending` (GitHub is being asked), `applied`, `suggested` or
    /// `skipped`.
    pub state: String,
    /// Why nothing moved by itself: `no_rule`, `direct_push`,
    /// `already_closed`, `in_triage`, `workflow_disallowed`, `form_rule`,
    /// `sponsor_inactive`, `sponsor_lost_permission`, `too_many`,
    /// `not_on_default_branch`, `unconfirmed`.
    pub reason: Option<String>,
    /// The rule's target status (to preselect in the transition dialog, or
    /// what it was moved to).
    pub target_status_id: Option<Uuid>,
    pub target_status: Option<String>,
    /// Whose rule it is.
    pub sponsor: Option<String>,
    pub at: DateTime<Utc>,
}

/// The fix links of an issue the caller has been shown, through live links
/// only; nothing of a hidden PR or commit; not what needs no attention
/// (`already_in_target`).
pub async fn fixes_of(conn: &mut PgConnection, issue: Uuid) -> Result<Vec<DevFix>> {
    type Row = (
        String,
        Option<i32>,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        DateTime<Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.full_name, p.number, x.sha, x.keyword, x.state, x.reason, u.target_status_id, s.name,
                sp.name, coalesce(x.decided_at, x.detected_at)
         FROM issue_fix_links x
         JOIN project_repo_links l ON l.id = x.link_id AND l.unlinked_at IS NULL AND l.project_id = x.project_id
         JOIN github_repos r ON r.id = x.repo_id
         LEFT JOIN github_pull_requests p ON p.id = x.pr_id
         LEFT JOIN project_fix_rules u ON u.id = x.rule_id
         LEFT JOIN issue_statuses s ON s.id = u.target_status_id
         LEFT JOIN principals sp ON sp.id = u.sponsor_id
         WHERE x.issue_id = $1 AND x.reason IS DISTINCT FROM 'already_in_target'
           AND NOT EXISTS (SELECT 1 FROM project_pr_hidden h
                           WHERE h.project_id = x.project_id AND h.pr_id = x.pr_id)
           AND NOT EXISTS (SELECT 1 FROM project_commit_hidden h
                           WHERE h.project_id = x.project_id AND h.repo_id = x.repo_id AND h.sha = x.sha)
           AND NOT (x.sha IS NOT NULL AND EXISTS (
                        SELECT 1 FROM issue_fix_links y
                        WHERE y.issue_id = x.issue_id AND y.repo_id = x.repo_id AND y.pr_id IS NOT NULL
                          AND y.link_id = x.link_id))
         ORDER BY x.detected_at DESC, x.id DESC LIMIT 40",
    )
    .bind(issue)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DevFix {
            repo: r.0,
            pr_number: r.1,
            sha: r.2,
            keyword: r.3,
            state: r.4,
            reason: r.5,
            target_status_id: r.6,
            target_status: r.7,
            sponsor: r.8,
            at: r.9,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kws(extra: &[&str]) -> Vec<String> {
        DEFAULT_KEYWORDS
            .iter()
            .chain(extra.iter())
            .map(|k| k.to_string())
            .collect()
    }

    fn found(text: &str, extra: &[&str]) -> Vec<(String, String)> {
        scan(text, &kws(extra))
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn syntax() {
        assert_eq!(found("fixes AUTH-1", &[]), pairs(&[("fixes", "AUTH-1")]));
        assert_eq!(found("Fixes: AUTH-1.", &[]), pairs(&[("fixes", "AUTH-1")]));
        assert_eq!(
            found("FIX AUTH-1, AUTH-2 and WEB-3; see AUTH-4", &[]),
            pairs(&[("fix", "AUTH-1"), ("fix", "AUTH-2"), ("fix", "WEB-3")])
        );
        assert_eq!(
            found("closes AUTH-1,AUTH-2\nresolved AUTH-9", &[]),
            pairs(&[
                ("closes", "AUTH-1"),
                ("closes", "AUTH-2"),
                ("resolved", "AUTH-9")
            ])
        );
    }

    #[test]
    fn not_a_keyword() {
        for text in [
            "prefixes AUTH-1",
            "fixesAUTH-1",
            "fixes  \n AUTH-1",
            "fixes auth-1",
            "fixes #12",
            "fixes AUTH-1x",
            "fixed_by AUTH-1",
            "AUTH-1 fixes",
        ] {
            assert!(found(text, &[]).is_empty(), "{text}");
        }
        // `and` needs spaces on both sides; a trailing comma ends the list.
        assert_eq!(
            found("fixes AUTH-1 andAUTH-2", &[]),
            pairs(&[("fixes", "AUTH-1")])
        );
        assert_eq!(
            found("fixes AUTH-1, and more", &[]),
            pairs(&[("fixes", "AUTH-1")])
        );
    }

    #[test]
    fn custom_keywords() {
        assert_eq!(
            found("修正 AUTH-1", &["修正"]),
            pairs(&[("修正", "AUTH-1")])
        );
        assert_eq!(
            found("已修正: AUTH-1", &["修正"]),
            pairs(&[("修正", "AUTH-1")])
        );
        assert!(found("修正AUTH-1", &["修正"]).is_empty());
        // The longest keyword wins.
        assert_eq!(
            found("fixes AUTH-1", &["fixe"]),
            pairs(&[("fixes", "AUTH-1")])
        );
    }

    #[test]
    fn derived_ids_are_stable_and_distinct() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(
            derived_id(NS_FIXED_BY, &[a.as_bytes()]),
            derived_id(NS_FIXED_BY, &[a.as_bytes()])
        );
        assert_ne!(
            derived_id(NS_FIXED_BY, &[a.as_bytes()]),
            derived_id(NS_FIXED_BY, &[b.as_bytes()])
        );
        assert_ne!(
            derived_id(NS_FIXED_BY, &[a.as_bytes()]),
            derived_id(NS_AUTO_DISABLE, &[a.as_bytes()])
        );
        assert_eq!(
            derived_id(NS_FIXED_BY, &[a.as_bytes()]).get_version_num(),
            8
        );
    }
}

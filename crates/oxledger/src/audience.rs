// SPDX-License-Identifier: GPL-2.0-or-later
//! Audience sets and the containment check for agent writes (contract 02
//! §3.8 M8, M9; §6.7 steps 8 and 14).
//!
//! A(P) is everyone who can read a hypothetical non-private issue of P:
//! active admins, active users holding `view_issues` (visibility all or
//! default) as members or through a group, and — for a public project — the
//! Non member role's holders (every active non-member) and the Anonymous role
//! (`anonymous`). It does not depend on which issues P has. A project that is
//! archived or has no issue tracking has no readers.
//!
//! An agent write may only land where its readers are a subset of the
//! readers of every source project of the version (∩ A(R snapshot)):
//! public content → A(project); a private note or a new private issue →
//! its actual readers D. Comparison is set containment, never counts.

use crate::commands::{DestIssue, DestKind, Parsed, TimeDest, VersionDest};
use crate::error::{ApiError, ErrorCode};
use anyhow::Result;
use serde_json::json;
use sqlx::PgConnection;
use std::collections::BTreeSet;
use uuid::Uuid;

/// Bumped when the rules above change (§3.8 M10: part of the proposal hash).
pub const AUDIENCE_ALGORITHM_VERSION: i32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Audience {
    pub users: BTreeSet<Uuid>,
    pub anonymous: bool,
}

impl Audience {
    pub fn is_subset(&self, of: &Audience) -> bool {
        self.users.is_subset(&of.users) && (!self.anonymous || of.anonymous)
    }

    fn intersect(&self, o: &Audience) -> Audience {
        Audience {
            users: self.users.intersection(&o.users).copied().collect(),
            anonymous: self.anonymous && o.anonymous,
        }
    }

    fn union(mut self, o: Audience) -> Audience {
        self.users.extend(o.users);
        self.anonymous |= o.anonymous;
        self
    }
}

/// Which issue visibilities a role needs to count.
#[derive(Clone, Copy)]
enum Vis {
    /// all or default: reads non-private issues.
    Public,
    /// all: reads private issues too.
    All,
    /// any visibility (a permission other than view_issues).
    Any,
}

impl Vis {
    fn sql(self) -> &'static str {
        match self {
            Vis::Public => "('all', 'default')",
            Vis::All => "('all')",
            Vis::Any => "('all', 'default', 'own')",
        }
    }
}

/// Holders of `permission` in `project` (for `tracker`, or for every
/// tracker when `None`) with a role of visibility `vis`, plus active admins.
async fn holders(
    conn: &mut PgConnection,
    project: Uuid,
    permission: &str,
    tracker: Option<Uuid>,
    vis: Vis,
) -> Result<Audience> {
    holders_in(conn, "issue_tracking", project, permission, tracker, vis).await
}

/// [`holders`] for a project that has `module` on (and is not archived);
/// no readers otherwise.
async fn holders_in(
    conn: &mut PgConnection,
    module: &str,
    project: Uuid,
    permission: &str,
    tracker: Option<Uuid>,
    vis: Vis,
) -> Result<Audience> {
    let eligible: Option<bool> = sqlx::query_scalar(
        "SELECT p.is_public FROM projects p
         JOIN enabled_modules m ON m.project_id = p.id AND m.module = $2
         WHERE p.id = $1 AND p.status <> 'archived'",
    )
    .bind(project)
    .bind(module)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(public) = eligible else {
        return Ok(Audience::default());
    };
    // A role grants `permission` for the tracker: all_trackers, or the
    // tracker is listed (or, with no tracker, every tracker is covered).
    let role_ok = format!(
        "r.issues_visibility IN {} AND EXISTS (
             SELECT 1 FROM role_permissions rp WHERE rp.role_id = r.id AND rp.permission = $2
               AND (rp.all_trackers OR ($3::uuid IS NOT NULL AND EXISTS (
                    SELECT 1 FROM role_permission_trackers t
                    WHERE t.role_id = r.id AND t.permission = rp.permission AND t.tracker_id = $3))))",
        vis.sql()
    );
    let sql = format!(
        "WITH member_of AS (
             SELECT u.id AS user_id, m.id AS member_id FROM members m
             JOIN principals u ON u.id = m.principal_id AND u.kind = 'user'
             WHERE m.project_id = $1
             UNION
             SELECT gm.user_id, m.id FROM members m
             JOIN group_members gm ON gm.group_id = m.principal_id
             WHERE m.project_id = $1
         )
         SELECT u.id FROM principals u
         WHERE u.kind = 'user' AND u.status = 'active' AND (
             u.is_admin
             OR EXISTS (SELECT 1 FROM member_of mo JOIN member_roles mr ON mr.member_id = mo.member_id
                        JOIN roles r ON r.id = mr.role_id
                        WHERE mo.user_id = u.id AND {role_ok})
             OR ($4 AND NOT EXISTS (SELECT 1 FROM member_of mo WHERE mo.user_id = u.id)
                 AND EXISTS (SELECT 1 FROM roles r WHERE r.builtin = 'non_member' AND {role_ok})))
         ORDER BY u.id"
    );
    let users: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(project)
        .bind(permission)
        .bind(tracker)
        .bind(public)
        .fetch_all(&mut *conn)
        .await?;
    let anon_sql = format!(
        "SELECT $4 AND EXISTS (SELECT 1 FROM roles r WHERE r.builtin = 'anonymous' AND {role_ok}) AND $1 IS NOT NULL"
    );
    let anonymous: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(anon_sql))
        .bind(project)
        .bind(permission)
        .bind(tracker)
        .bind(public)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Audience {
        users: users.into_iter().collect(),
        anonymous,
    })
}

/// A(P): the readers of a hypothetical non-private issue of `project`.
pub async fn project_audience(conn: &mut PgConnection, project: Uuid) -> Result<Audience> {
    holders(conn, project, "view_issues", None, Vis::Public).await
}

/// Design 06 R-AG1: who holds the wiki permission `perm` in `project` — W(P)
/// for `view_wiki_pages`, H(P) for `view_wiki_edits`. Not [`holders`]: that
/// one needs the issue module and counts only roles reading non-private
/// issues, so a wiki without issues (or an `own` role holding the wiki
/// permission) would come out with fewer readers than it has. Here the wiki
/// module must be on (project not archived) and every role counts whatever
/// its `issues_visibility`; Non member, Anonymous and admins as in
/// [`holders`].
pub async fn wiki_audience(conn: &mut PgConnection, project: Uuid, perm: &str) -> Result<Audience> {
    holders_in(conn, "wiki", project, perm, None, Vis::Any).await
}

/// R-AG1 for `project`: whether W(P) ⊇ A(P) (an agent may read the wiki)
/// and H(P) ⊇ A(P) (it may read older versions too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WikiDelegable {
    pub pages: bool,
    pub history: bool,
    /// W(P) = A(P): what the wiki holds reaches exactly the issue readers
    /// (R-AG3 needs it for a write; W ⊆ ∩A(R) is checked per write).
    pub equal: bool,
}

pub async fn wiki_delegable(conn: &mut PgConnection, project: Uuid) -> Result<WikiDelegable> {
    let a = project_audience(&mut *conn, project).await?;
    let w = wiki_audience(&mut *conn, project, "view_wiki_pages").await?;
    let h = wiki_audience(&mut *conn, project, "view_wiki_edits").await?;
    Ok(WikiDelegable {
        pages: a.is_subset(&w),
        history: a.is_subset(&h),
        equal: a == w,
    })
}

/// ∩ A(R) over `sources` (an empty R allows no one).
pub(crate) async fn allowed_readers(conn: &mut PgConnection, sources: &[Uuid]) -> Result<Audience> {
    let mut allowed: Option<Audience> = None;
    for s in sources {
        let a = project_audience(conn, *s).await?;
        allowed = Some(match allowed {
            None => a,
            Some(x) => x.intersect(&a),
        });
    }
    Ok(allowed.unwrap_or_default())
}

/// Active users among `ids`, groups expanded to their active members.
async fn people(conn: &mut PgConnection, ids: &[Uuid]) -> Result<BTreeSet<Uuid>> {
    let v: Vec<Uuid> = sqlx::query_scalar(
        "SELECT u.id FROM principals u
         WHERE u.kind = 'user' AND u.status = 'active'
           AND (u.id = ANY($1) OR u.id IN (SELECT user_id FROM group_members WHERE group_id = ANY($1)))",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(v.into_iter().collect())
}

/// D for a new private issue: `all` visibility readers, admins, and the
/// author (the delegator, §16.2) and assignee when they read the project.
async fn new_private_issue_readers(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
    delegator: Uuid,
    assignee: Option<Uuid>,
) -> Result<Audience> {
    let all = holders(conn, project, "view_issues", Some(tracker), Vis::All).await?;
    let any = holders(conn, project, "view_issues", Some(tracker), Vis::Any).await?;
    let mut ids = vec![delegator];
    ids.extend(assignee);
    let own: BTreeSet<Uuid> = people(conn, &ids)
        .await?
        .into_iter()
        .filter(|u| any.users.contains(u))
        .collect();
    Ok(all.union(Audience {
        users: own,
        anonymous: false,
    }))
}

/// D for a private note on an issue of `project`/`tracker` that is read by
/// `issue_readers`: who also has view_private_notes, plus the author (the
/// delegator) and admins.
async fn private_note_readers(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
    delegator: Uuid,
    issue_readers: &Audience,
) -> Result<Audience> {
    let pn = holders(conn, project, "view_private_notes", Some(tracker), Vis::Any).await?;
    let mut d = pn.intersect(issue_readers);
    if issue_readers.users.contains(&delegator) {
        d.users.insert(delegator);
    }
    Ok(d)
}

fn forbidden(kind: &str, op_index: usize) -> anyhow::Error {
    ApiError::new(
        ErrorCode::AudienceExpansionForbidden,
        "this would show content to people who cannot read every project the agent may have read from",
    )
    .with_details(json!({"kind": kind, "op_index": op_index}))
    .into()
}

/// Step 8 / step 14: every destination of `doc` must be read by no one
/// outside ∩ A(sources). Existing private issues are left to the visibility
/// check (the agent cannot read them: `not_found`, T-Q5).
pub(crate) async fn check(
    conn: &mut PgConnection,
    doc: &Parsed,
    sources: &[Uuid],
    delegator: Uuid,
) -> Result<()> {
    let allowed = allowed_readers(&mut *conn, sources).await?;
    let dests = doc.destinations();
    // Readers of each issue the document creates, by op index.
    let mut created: Vec<(usize, Uuid, Uuid, Audience)> = Vec::new();
    for (i, project, tracker) in doc.creates() {
        let d = dests
            .iter()
            .find(|d| d.op_index == i)
            .expect("a create has a destination");
        let DestKind::NewIssue {
            private, assignee, ..
        } = &d.kind
        else {
            unreachable!()
        };
        let private = match private {
            Some(p) => *p,
            None => sqlx::query_scalar("SELECT private_by_default FROM trackers WHERE id = $1")
                .bind(tracker)
                .fetch_optional(&mut *conn)
                .await?
                .unwrap_or(false),
        };
        let readers = if private {
            new_private_issue_readers(conn, project, tracker, delegator, *assignee).await?
        } else {
            project_audience(conn, project).await?
        };
        if !readers.is_subset(&allowed) {
            return Err(forbidden(
                if private {
                    "private_destination"
                } else {
                    "cross_project"
                },
                i,
            ));
        }
        created.push((i, project, tracker, readers));
    }
    for d in &dests {
        let (target, private_note) = match &d.kind {
            DestKind::NewIssue { .. } => continue,
            DestKind::Version(v) => {
                // §17.9a, M8: a milestone is public content of its project.
                let project: Option<Uuid> = match v {
                    VersionDest::New(p) => Some(*p),
                    VersionDest::Existing(id) => {
                        sqlx::query_scalar("SELECT project_id FROM versions WHERE id = $1")
                            .bind(id)
                            .fetch_optional(&mut *conn)
                            .await?
                    }
                };
                let Some(project) = project else {
                    continue; // not_found later
                };
                if !project_audience(conn, project).await?.is_subset(&allowed) {
                    return Err(forbidden("cross_project", d.op_index));
                }
                continue;
            }
            DestKind::Time(t) => {
                let op_index = d.op_index;
                // Contract 05 §3.5.4: who reads the entry (time readers with
                // `all`, admins, the delegator), cut to the issue's readers.
                let (project, issue) = time_dest(conn, *t).await?;
                let Some(project) = project else {
                    continue; // not_found later
                };
                let mut d = time_readers(conn, project, delegator).await?;
                if let Some(i) = issue {
                    let private: Option<bool> =
                        sqlx::query_scalar("SELECT is_private FROM issues WHERE id = $1")
                            .bind(i)
                            .fetch_optional(&mut *conn)
                            .await?;
                    if private != Some(false) {
                        continue; // the agent cannot read it: not_found later
                    }
                    d = d.intersect(&project_audience(conn, project).await?);
                }
                if !d.is_subset(&allowed) {
                    return Err(forbidden("private_destination", op_index));
                }
                continue;
            }
            DestKind::Content(t) => (t, false),
            DestKind::PrivateNote(t) => (t, true),
        };
        let (project, tracker, issue_readers) = match target {
            DestIssue::Created(i) => {
                let Some((_, p, t, r)) = created.iter().find(|c| c.0 == *i) else {
                    continue;
                };
                (*p, *t, r.clone())
            }
            DestIssue::Existing(id) => {
                let row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
                    "SELECT project_id, tracker_id, is_private FROM issues WHERE id = $1",
                )
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
                let Some((p, t, private)) = row else {
                    continue; // not_found later
                };
                if private {
                    continue; // the agent cannot read it: not_found later
                }
                (p, t, project_audience(conn, p).await?)
            }
        };
        let readers = if private_note {
            private_note_readers(conn, project, tracker, delegator, &issue_readers).await?
        } else {
            issue_readers
        };
        if !readers.is_subset(&allowed) {
            return Err(forbidden(
                if private_note {
                    "private_destination"
                } else {
                    "cross_project"
                },
                d.op_index,
            ));
        }
    }
    Ok(())
}

/// The project and issue a time destination ends up in (`None` project: the
/// entry or issue does not exist; the plan reports it).
async fn time_dest(conn: &mut PgConnection, t: TimeDest) -> Result<(Option<Uuid>, Option<Uuid>)> {
    Ok(match t {
        TimeDest::New { project_id, issue } => match (project_id, issue) {
            (_, Some(i)) => (
                sqlx::query_scalar("SELECT project_id FROM issues WHERE id = $1")
                    .bind(i)
                    .fetch_optional(&mut *conn)
                    .await?,
                Some(i),
            ),
            (p, None) => (p, None),
        },
        TimeDest::Existing { entry_id, issue } => {
            let row: Option<(Uuid, Option<Uuid>)> =
                sqlx::query_as("SELECT project_id, issue_id FROM time_entries WHERE id = $1")
                    .bind(entry_id)
                    .fetch_optional(&mut *conn)
                    .await?;
            match row {
                None => (None, None),
                Some((p, old)) => (Some(p), issue.unwrap_or(old)),
            }
        }
    })
}

/// Who reads a time entry of `project` logged for `delegator`: users whose
/// `time_visibility` is `all` (admins included), and the delegator.
async fn time_readers(conn: &mut PgConnection, project: Uuid, delegator: Uuid) -> Result<Audience> {
    let mut users: BTreeSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM principals WHERE kind = 'user' AND status = 'active'
           AND time_visibility($1, id) = 'all'",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    users.insert(delegator);
    Ok(Audience {
        users,
        anonymous: false,
    })
}

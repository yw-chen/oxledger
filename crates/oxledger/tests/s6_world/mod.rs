// SPDX-License-Identifier: GPL-2.0-or-later
//! Shared world for the S6 suites: two audience-consistent projects WEB and
//! AUTH, where AUTH's readers are a subset of WEB's (A(AUTH) ⊂ A(WEB)).
//!
//! - alice (delegator), bob: members of WEB and AUTH (role Dev, visibility all);
//! - carol: member of WEB only;
//! - an agent client "Claude Code"; alice signs in through `auth::login`.
#![allow(dead_code)]

pub mod mcp_client;

use oxledger::agents::{self, DelegationInput, DelegationView};
use oxledger::auth::{self, AuthContext, LoginInput, SessionPolicy};
use oxledger::custom_fields;
use oxledger::error::{ErrorCode, classify};
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

pub const ORIGIN: &str = "https://ledger.test";
pub const PW: &str = "correct horse battery";

pub async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE changeset_applications, changeset_executions, changeset_decisions, changeset_version_ops,
                  changeset_version_source_projects, changeset_versions, changesets, changeset_groups,
                  proposal_contents,
                  issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
                  outbox, outbox_cursors, issue_keys, issues, issue_events, issue_notes, issue_duplicates,
                  issue_field_origins, execution_field_changes, execution_relation_changes,
                  command_executions, execution_refs, operation_requests,
                  workflow_transitions, workflow_field_rules,
                  project_trackers, member_roles, members, role_permission_trackers, role_permissions,
                  roles, enabled_modules, versions, projects, api_keys, group_members, aliases,
                  principals, trackers, issue_statuses, agent_clients, auth_events, login_challenges CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE org_settings SET issue_group_assignment = false")
        .execute(pool)
        .await
        .unwrap();
    custom_fields::reconcile_indexes(pool).await.unwrap();
}

pub async fn id(pool: &PgPool, sql: &str) -> Uuid {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

pub async fn exec(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

pub async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

pub fn code(e: &anyhow::Error) -> ErrorCode {
    classify(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .code
}

pub fn kind(e: &anyhow::Error) -> String {
    classify(e)
        .and_then(|a| a.details)
        .and_then(|d| d.get("kind").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

pub async fn epoch(pool: &PgPool, project: Uuid) -> i64 {
    sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1")
        .bind(project)
        .fetch_one(pool)
        .await
        .unwrap()
}

pub struct World {
    pub pool: PgPool,
    pub app: Arc<App>,
    pub policy: SessionPolicy,
    pub new: Uuid,
    pub doing: Uuid,
    pub done: Uuid,
    pub bug: Uuid,
    pub dev: Uuid,
    pub alice: Uuid,
    pub bob: Uuid,
    pub carol: Uuid,
    pub web: Uuid,
    pub auth_p: Uuid,
    pub client: Uuid,
    /// alice's browser session.
    pub alice_ctx: AuthContext,
}

pub async fn grant(pool: &PgPool, role: Uuid, perms: &[&str]) {
    for p in perms {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(role)
            .bind(p)
            .execute(pool)
            .await
            .unwrap();
    }
}

pub async fn member(pool: &PgPool, project: Uuid, principal: Uuid, role: Uuid) -> Uuid {
    let m: Uuid = sqlx::query_scalar(
        "INSERT INTO members (project_id, principal_id) VALUES ($1,$2) RETURNING id",
    )
    .bind(project)
    .bind(principal)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
        .bind(m)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    m
}

pub async fn sign_in(pool: &PgPool, policy: &SessionPolicy, login: &str) -> AuthContext {
    let ch = auth::new_login_challenge(pool, policy).await.unwrap();
    let s = auth::login(
        pool,
        policy,
        LoginInput {
            challenge_cookie: Some(&ch.cookie),
            challenge_csrf: &ch.csrf_token,
            login,
            password: PW,
        },
    )
    .await
    .unwrap();
    auth::session_from_cookie(pool, policy, &s.token)
        .await
        .unwrap()
        .unwrap()
        .ctx
}

impl World {
    pub async fn build(pool: &PgPool) -> World {
        reset(pool).await;
        let new = id(pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
        let doing = id(pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Doing','doing',2) RETURNING id").await;
        let done = id(pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Done','done',3) RETURNING id").await;
        let bug = id(pool, "INSERT INTO trackers (name, position, default_status_id) SELECT 'Bug', 1, id FROM issue_statuses WHERE name = 'New' RETURNING id").await;
        let alice = auth::create_user(pool, "alice", "alice", PW, false)
            .await
            .unwrap();
        let bob = auth::create_user(pool, "bob", "bob", PW, false)
            .await
            .unwrap();
        let carol = auth::create_user(pool, "carol", "carol", PW, false)
            .await
            .unwrap();
        let web = id(
            pool,
            "INSERT INTO projects (key, name) VALUES ('WEB','Web') RETURNING id",
        )
        .await;
        let auth_p = id(
            pool,
            "INSERT INTO projects (key, name) VALUES ('AUTH','Auth') RETURNING id",
        )
        .await;
        exec(
            pool,
            "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
        )
        .await;
        for p in [web, auth_p] {
            sqlx::query("INSERT INTO project_trackers VALUES ($1,$2)")
                .bind(p)
                .bind(bug)
                .execute(pool)
                .await
                .unwrap();
        }
        let dev = id(
            pool,
            "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
        )
        .await;
        grant(
            pool,
            dev,
            &[
                "view_issues",
                "add_issues",
                "edit_issues",
                "add_issue_notes",
                "set_issues_private",
                "manage_issue_relations",
                "view_private_notes",
                "set_notes_private",
            ],
        )
        .await;
        for p in [web, auth_p] {
            member(pool, p, alice, dev).await;
            member(pool, p, bob, dev).await;
        }
        member(pool, web, carol, dev).await;
        sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,'always')")
            .bind(bug)
            .bind(dev)
            .bind(new)
            .bind(doing)
            .execute(pool)
            .await
            .unwrap();
        let client = agents::create_agent_client(pool, "Claude Code", "policy-2026-01")
            .await
            .unwrap();
        agents::set_agent_client_enabled(pool, "Claude Code", true)
            .await
            .unwrap();
        let app = Arc::new(App {
            pool: pool.clone(),
            index: SearchIndex::open(None).unwrap(),
        });
        let policy = SessionPolicy::default();
        let alice_ctx = sign_in(pool, &policy, "alice").await;
        World {
            pool: pool.clone(),
            app,
            policy,
            new,
            doing,
            done,
            bug,
            dev,
            alice,
            bob,
            carol,
            web,
            auth_p,
            client,
            alice_ctx,
        }
    }

    pub async fn issue(&self, project: Uuid, subject: &'static str) -> Uuid {
        create_issue(
            &self.pool,
            NewIssue {
                project_id: project,
                tracker_id: self.bug,
                status_id: self.new,
                subject,
                description: "",
                author_id: self.bob,
                assigned_to_id: None,
                is_private: false,
                parent_id: None,
            },
        )
        .await
        .unwrap()
        .0
    }

    /// alice delegates `project` to the agent (in her active context).
    pub async fn delegate(&self, project: Uuid) -> anyhow::Result<DelegationView> {
        agents::create_delegation(
            &self.pool,
            &self.policy,
            &self.alice_ctx,
            DelegationInput {
                agent_client_id: self.client,
                project_id: project,
                proposal_ops: [
                    "create_issue",
                    "update_fields",
                    "transition",
                    "add_note",
                    "duplicate_of",
                ]
                .map(String::from)
                .to_vec(),
                writable_fields: ["subject", "description", "assigned_to_id"]
                    .map(String::from)
                    .to_vec(),
                max_ops: None,
                expires_in_days: Some(30),
                policies: None,
            },
        )
        .await
    }

    /// A token for `context` and the agent's AuthContext from it.
    pub async fn token(&self, context: Uuid) -> (String, AuthContext) {
        let c = agents::issue_credential(
            &self.pool,
            &self.policy,
            &self.alice_ctx,
            context,
            "t",
            None,
        )
        .await
        .unwrap();
        let ctx = auth::authenticate_bearer(&self.pool, &c.token)
            .await
            .unwrap()
            .unwrap();
        (c.token, ctx)
    }

    /// alice starts a new work context (closing the active one).
    pub async fn new_context(&self) -> Uuid {
        agents::new_context(&self.pool, &self.policy, &self.alice_ctx, self.client, true)
            .await
            .unwrap()
    }
}

pub fn op() -> String {
    Uuid::now_v7().to_string()
}

pub fn tgt(i: Uuid) -> Value {
    json!({"type": "issue", "id": i})
}

pub fn note_doc(i: Uuid, lv: i32, body: &str) -> Value {
    json!({"command_schema_version": 1, "title": "agent proposal",
           "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

pub fn subject_doc(i: Uuid, lv: i32, subject: &str) -> Value {
    json!({"command_schema_version": 1, "title": "agent proposal",
           "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": subject}}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

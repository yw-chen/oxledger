// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.5: forms and field management (§2.6, §2.7, §3.6, §3.7,
//! §7.5, §7.6). T-M1-45 … T-M1-59, the leakage of `describe_form`, the
//! stale `expected_rules_version`, `describe_form` and `validate` over MCP,
//! the HTTP routes, conversions with an injected failure, the field
//! lock_version under concurrency, and the CHECK lists of 0024/0025 against
//! the code's constants. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents::{self, PolicyInput};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{AuthContext, SessionPolicy};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, ApproveInput, SubmitResult};
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ErrorCode, classify};
use oxledger::field_admin::{
    self, ConversionInput, FieldPatch, FieldQuery, FieldSet, NewFieldInput, OptionLabel,
};
use oxledger::forms::{
    self, DraftInput, NewForm, Part, PublishInput, PublishResult, RollbackInput,
    ValidateCommandInput,
};
use oxledger::policy::PolicyMode;
use oxledger::service::{self, SearchParams};
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::undo::{self, UndoInput};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

struct F {
    w: World,
    /// The site form of Bug.
    form: Uuid,
    /// A list field "Severity" (Low, Medium, High) for Bug in every project.
    sev: i32,
    low: Uuid,
    medium: Uuid,
    high: Uuid,
}

fn field_input(name: &str, ftype: &str, trackers: Vec<Uuid>, options: &[&str]) -> NewFieldInput {
    NewFieldInput {
        name: name.into(),
        ftype: ftype.into(),
        description: None,
        is_required: None,
        is_for_all: Some(true),
        project_ids: None,
        tracker_ids: trackers,
        visible: None,
        role_ids: None,
        options: (!options.is_empty()).then(|| {
            options
                .iter()
                .map(|l| OptionLabel { label: (*l).into() })
                .collect()
        }),
    }
}

async fn setup(pool: &sqlx::PgPool) -> F {
    faults::clear();
    let w = World::build(pool).await;
    exec(
        pool,
        &format!(
            "UPDATE principals SET is_admin = true WHERE id = '{}'",
            w.alice
        ),
    )
    .await;
    // The system principal of 0008 (the world's reset truncates principals);
    // conversions write their events as it.
    exec(
        pool,
        "INSERT INTO principals (id, kind, name) VALUES ('00000000-0000-7000-8000-000000000001', 'system', 'oxledger')
         ON CONFLICT DO NOTHING",
    )
    .await;
    for (from, to) in [(w.new, w.done), (w.doing, w.done), (w.done, w.new)] {
        sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,'always')")
            .bind(w.bug)
            .bind(w.dev)
            .bind(from)
            .bind(to)
            .execute(pool)
            .await
            .unwrap();
    }
    let f = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Severity", "list", vec![w.bug], &["Low", "Medium", "High"]),
    )
    .await
    .unwrap();
    let opt = |l: &str| f.options.iter().find(|o| o.label == l).unwrap().id;
    let (low, medium, high) = (opt("Low"), opt("Medium"), opt("High"));
    let form = forms::ensure(
        &w.app,
        w.alice_ctx,
        &NewForm {
            tracker_id: w.bug,
            project: None,
        },
    )
    .await
    .unwrap()
    .summary
    .id;
    F {
        w,
        form,
        sev: f.id,
        low,
        medium,
        high,
    }
}

async fn mk(w: &World, subject: &str) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: w.web,
            tracker_id: w.bug,
            status_id: w.new,
            subject,
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn doc_of(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "forms", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

fn set_op(i: Uuid, set: Value) -> Value {
    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": set})
}

fn upd(i: Uuid, lv: i32, set: Value) -> Value {
    doc_of(vec![set_op(i, set)], vec![(i, lv)])
}

async fn human(w: &World, who: AuthContext, d: &Value) -> anyhow::Result<CommandResult> {
    commands::submit(&w.app, who, Uuid::now_v7(), d).await
}

/// Updates `i` as `who` with the current lock_version.
async fn set(w: &World, who: AuthContext, i: Uuid, s: Value) -> anyhow::Result<CommandResult> {
    let l = lv(w, i).await;
    human(w, who, &upd(i, l, s)).await
}

fn details(e: &anyhow::Error) -> Value {
    classify(e).and_then(|a| a.details).unwrap_or(Value::Null)
}

/// Replaces the draft of `part` and publishes it.
async fn publish(
    f: &F,
    part: Part,
    body: Value,
    mode: Option<&str>,
) -> anyhow::Result<PublishResult> {
    publish_form(&f.w, f.form, part, body, mode).await
}

async fn publish_form(
    w: &World,
    form: Uuid,
    part: Part,
    body: Value,
    mode: Option<&str>,
) -> anyhow::Result<PublishResult> {
    let v = draft(w, form, part, body).await?;
    forms::publish(
        &w.app,
        &w.policy,
        w.alice_ctx,
        form,
        part,
        &PublishInput {
            draft_version: v,
            mode: mode.map(String::from),
        },
    )
    .await
}

/// Puts a draft; its version.
async fn draft(w: &World, form: Uuid, part: Part, body: Value) -> anyhow::Result<i32> {
    let d = forms::get(&w.app, w.alice_ctx, form).await?;
    let (pv, published) = match part {
        Part::Rules => (&d.rules, d.summary.published_rules_version),
        Part::Layout => (&d.layout, d.summary.published_layout_version),
    };
    let base = pv.draft.as_ref().map(|v| v.version).unwrap_or(published);
    Ok(forms::put_draft(
        &w.app,
        w.alice_ctx,
        form,
        part,
        &DraftInput {
            base_version: base,
            body,
        },
    )
    .await?
    .version)
}

fn due_when_assigned() -> Value {
    json!({"id": "due_when_assigned", "field": "due_date", "kind": "required_if",
           "when": {"all": [{"field": "assignee", "op": "is_set"}]}})
}

fn rules(rs: Vec<Value>) -> Value {
    json!({ "rules": rs })
}

async fn describe(w: &World, who: AuthContext) -> forms::FormDescription {
    forms::describe_form(&w.app, who, "WEB", "Bug", "create", None)
        .await
        .unwrap()
}

fn validate_input(command: Value, expected: Option<&str>) -> ValidateCommandInput {
    ValidateCommandInput {
        command,
        expected_rules_version: expected.map(String::from),
    }
}

/// An agent of alice delegated WEB; (delegation, token, agent context).
async fn agent(w: &World) -> (agents::DelegationView, String, AuthContext) {
    let ctx = w.new_context().await;
    let d = w.delegate(w.web).await.unwrap();
    let (token, a) = w.token(ctx).await;
    (d, token, a)
}

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    )
}

async fn open_marks(w: &World, i: Uuid) -> i64 {
    count(
        &w.pool,
        &format!(
            "SELECT count(*) FROM form_rule_marks WHERE issue_id = '{i}' AND cleared_at IS NULL"
        ),
    )
    .await
}

// ---------------------------------------------------------------- T-M1-45, 52, 53

#[tokio::test]
async fn t45_required_if_on_every_entry_point() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_d, token, agent) = agent(w).await;

    // Before the rule: a pending proposal (apply entry) and an agent write
    // that cleared the assignee (undo entry).
    let (i_prop, _) = mk(w, "proposal").await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i_prop, 0, json!({"assigned_to_id": w.bob})),
        ORIGIN,
    )
    .await
    .unwrap();
    let (i_undo, _) = mk(w, "undo").await;
    set(w, w.alice_ctx, i_undo, json!({"assigned_to_id": w.bob}))
        .await
        .unwrap();
    let exec_id = match changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i_undo, lv(w, i_undo).await, json!({"assigned_to_id": null})),
        false,
        ORIGIN,
    )
    .await
    .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("not applied: {other:?}"),
    };
    let before = describe(w, w.alice_ctx).await.rules_version;
    let r = publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    assert_ne!(r.rules_version, before);
    // describe_form lists the rule on its target.
    let fd = describe(w, w.alice_ctx).await;
    assert_eq!(fd.rules_version, r.rules_version);
    let due = fd.fields.iter().find(|x| x.key == "due_date").unwrap();
    assert_eq!(due.required_if.len(), 1);
    assert_eq!(due.required_if[0].rule_id, "due_when_assigned");

    let check = |d: &Value, what: &str| {
        assert_eq!(d["rule_id"], "due_when_assigned", "{what}: {d}");
        assert_eq!(d["fields"], json!(["due_date"]), "{what}: {d}");
        assert_eq!(d["reason"], "form_rule", "{what}: {d}");
    };
    // 1. The UI (/commands).
    let (i1, _) = mk(w, "ui").await;
    let e = set(w, w.alice_ctx, i1, json!({"assigned_to_id": w.bob}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    check(&details(&e), "ui");
    // With the due date it passes.
    set(
        w,
        w.alice_ctx,
        i1,
        json!({"assigned_to_id": w.bob, "due_date": "2026-12-01"}),
    )
    .await
    .unwrap();

    // 2. MCP command_submit.
    let (i2, _) = mk(w, "mcp").await;
    let mut m = world::mcp_client::Mcp::connect(router(w), token).await;
    let e = m
        .call(
            "command_submit",
            json!({"operation_id": Uuid::now_v7(),
                   "command": upd(i2, 0, json!({"assigned_to_id": w.bob}))}),
        )
        .await
        .unwrap_err();
    let s = e.to_string();
    assert!(s.contains("required_field_missing"), "{s}");
    assert!(s.contains("due_when_assigned"), "{s}");

    // 3. validate (person and agent).
    let v = forms::validate_human(
        &w.app,
        w.alice_ctx,
        &validate_input(upd(i2, 0, json!({"assigned_to_id": w.bob})), None),
    )
    .await
    .unwrap();
    assert_eq!(v.outcome, "invalid");
    assert_eq!(v.rules_version.as_deref(), Some(r.rules_version.as_str()));
    assert_eq!(v.items[0].status, "blocked");
    assert_eq!(v.items[0].errors[0].code, ErrorCode::RequiredFieldMissing);
    assert_eq!(
        v.items[0].errors[0].rule_id,
        Some(json!("due_when_assigned"))
    );
    let v = changesets::validate_command(
        &w.app,
        agent,
        &validate_input(upd(i2, 0, json!({"assigned_to_id": w.bob})), None),
    )
    .await
    .unwrap();
    assert_eq!(v.items[0].errors[0].code, ErrorCode::RequiredFieldMissing);
    check(
        v.items[0].errors[0].details.as_ref().unwrap(),
        "agent validate",
    );

    // 4. Applying a proposal made under the old rules (T-M1-52): the queue
    // flags it and refuses a tick; a full review applies under the new
    // rules, which fails.
    let list = changesets::list(&w.app, w.alice_ctx, Some("awaiting_me"), None, None, None)
        .await
        .unwrap();
    let item = list
        .items
        .iter()
        .find(|c| c.changeset_id == p.changeset_id)
        .unwrap();
    assert!(item.rules_changed);
    assert_eq!(item.review_modes, vec!["detail".to_string()]);
    let view = changesets::get(&w.app, w.alice_ctx, p.changeset_id, None)
        .await
        .unwrap();
    assert!(view.rules_changed);
    let e = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: p.proposal_hash.clone(),
            review_mode: Some("queue".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReviewOutdated);
    assert!(
        details(&e)["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("rules_changed")),
        "{}",
        details(&e)
    );
    match changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: p.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    {
        Err(e) => {
            assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
            check(&details(&e), "apply");
        }
        Ok(o) => {
            let ex = o.execution.unwrap();
            assert_eq!(ex.status, "failed");
            let err = ex.error.unwrap();
            assert_eq!(err.code, "required_field_missing");
            check(&err.details, "apply");
        }
    }

    // 5. Undo (T-M1-53): restoring the assignee would break the rule.
    let e = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        exec_id,
        &UndoInput::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    let reasons = details(&e)["reasons"].clone();
    assert_eq!(reasons[0]["reason"], "rules_changed", "{reasons}");
    assert_eq!(reasons[0]["code"], "required_field_missing", "{reasons}");
    assert_eq!(reasons[0]["rule_id"], "due_when_assigned", "{reasons}");
}

// ---------------------------------------------------------------- T-M1-46, 47, 55

#[tokio::test]
async fn t46_allowed_values_and_t47_readonly_baseline() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let sev = format!("cf:{}", f.sev);
    let (i, _) = mk(w, "allowed").await;
    set(
        w,
        w.alice_ctx,
        i,
        json!({ sev.clone(): {"list": f.low}, "due_date": "2026-12-01" }),
    )
    .await
    .unwrap();
    publish(
        &f,
        Part::Rules,
        rules(vec![
            json!({"id": "sev_when_due", "field": sev, "kind": "allowed_values_if",
                   "when": {"all": [{"field": "due_date", "op": "is_set"}]},
                   "values": [{"list": f.medium}, {"list": f.high}]}),
            json!({"id": "due_frozen_when_done", "field": "due_date", "kind": "readonly_if",
                   "when": {"all": [{"field": "status_category", "op": "eq", "value": "done"}]}}),
        ]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    // T-M1-46: keeping the old value passes; a value outside the list fails.
    set(
        w,
        w.alice_ctx,
        i,
        json!({ sev.clone(): {"list": f.low}, "subject": "kept" }),
    )
    .await
    .unwrap();
    let e = set(
        w,
        w.alice_ctx,
        i,
        json!({ sev.clone(): {"list": f.low}, "due_date": "2026-12-02" }),
    )
    .await
    .map(|_| ())
    .err();
    // The unchanged Low with a new due date still keeps the old value.
    assert!(e.is_none(), "{e:?}");
    set(w, w.alice_ctx, i, json!({ sev.clone(): {"list": f.high} }))
        .await
        .unwrap();
    let e = set(w, w.alice_ctx, i, json!({ sev.clone(): {"list": f.low} }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    let d = details(&e);
    assert_eq!(d["kind"], "rule_disallowed", "{d}");
    assert_eq!(d["rule_id"], "sev_when_due", "{d}");
    assert_eq!(d["field"], json!(sev), "{d}");
    // Without a due date the rule does not hold.
    set(
        w,
        w.alice_ctx,
        i,
        json!({ sev.clone(): {"list": f.low}, "due_date": null }),
    )
    .await
    .unwrap();

    // T-M1-47: in one document, transition to done then change due_date:
    // judged on the issue at the start (todo) → passes.
    let (j, _) = mk(w, "readonly").await;
    let l = lv(w, j).await;
    human(
        w,
        w.alice_ctx,
        &doc_of(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(j), "to_status_id": w.done}),
                set_op(j, json!({"due_date": "2026-12-24"})),
            ],
            vec![(j, l)],
        ),
    )
    .await
    .unwrap();
    // The next document: the issue is done now.
    let e = set(w, w.alice_ctx, j, json!({"due_date": "2026-12-25"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReadonlyField);
    let d = details(&e);
    assert_eq!(d["reason"], "form_rule", "{d}");
    assert_eq!(d["rule_id"], "due_frozen_when_done", "{d}");
    // Other fields stay writable.
    set(w, w.alice_ctx, j, json!({"subject": "still editable"}))
        .await
        .unwrap();

    // T-M1-55: a note-only document on an issue that breaks a required_if.
    let (k, _) = mk(w, "old").await;
    set(w, w.alice_ctx, k, json!({"assigned_to_id": w.bob}))
        .await
        .unwrap();
    publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    let l = lv(w, k).await;
    human(
        w,
        w.alice_ctx,
        &doc_of(
            vec![
                json!({"op_id": op(), "kind": "add_note", "target": tgt(k), "body": "just a note"}),
            ],
            vec![(k, l)],
        ),
    )
    .await
    .unwrap();
    let e = set(w, w.alice_ctx, k, json!({"subject": "touch"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
}

// ---------------------------------------------------------------- T-M1-48

#[tokio::test]
async fn t48_form_invalid_before_publishing() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    let kind_of = |e: &anyhow::Error| {
        assert_eq!(code(e), ErrorCode::FormInvalid, "{e:#}");
        kind(e)
    };
    // A condition on a restricted field.
    let e = draft(
        w,
        f.form,
        Part::Rules,
        rules(vec![
            json!({"id": "r1", "field": "due_date", "kind": "required_if",
                          "when": {"all": [{"field": secret.key, "op": "is_set"}]}}),
        ]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "restricted_condition_field");
    assert_eq!(details(&e)["rule_id"], "r1");
    // Three levels.
    let e = draft(
        w,
        f.form,
        Part::Rules,
        rules(vec![json!({"id": "r2", "field": "due_date", "kind": "required_if",
                          "when": {"all": [{"any": [{"all": [{"field": "assignee", "op": "is_set"}]}]}]}})]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "condition_too_complex");
    // Six items in a group.
    let six: Vec<Value> = (0..6)
        .map(|_| json!({"field": "assignee", "op": "is_set"}))
        .collect();
    let e = draft(
        w,
        f.form,
        Part::Rules,
        rules(vec![
            json!({"id": "r3", "field": "due_date", "kind": "required_if",
                          "when": {"all": six}}),
        ]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "condition_too_complex");
    // An unknown field.
    let e = draft(
        w,
        f.form,
        Part::Rules,
        rules(vec![
            json!({"id": "r4", "field": "cf:999999", "kind": "required_if",
                          "when": {"all": [{"field": "assignee", "op": "is_set"}]}}),
        ]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "unknown_field");
    // show_if hides a field that is required (subject: always).
    let e = draft(
        w,
        f.form,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "description"}]},
            {"id": "extra", "title": "Extra",
             "show_if": {"all": [{"field": "status_category", "op": "eq", "value": "done"}]},
             "fields": [{"key": "subject"}]}
        ]}}}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "hides_required");
    // show_if that depends on each other.
    let e = draft(
        w,
        f.form,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [
                {"key": "subject"},
                {"key": "assigned_to_id", "show_if": {"all": [{"field": "due_date", "op": "is_set"}]}},
                {"key": "due_date", "show_if": {"all": [{"field": "assignee", "op": "is_set"}]}}
            ]}
        ]}}}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "cycle");
    // With a published required_if, a show_if that is not implied by its
    // condition hides it; the same condition is fine.
    publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    let layout = |show: Value| {
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}, {"key": "assigned_to_id"}]},
            {"id": "plan", "title": "Plan", "show_if": show, "fields": [{"key": "due_date"}]}
        ]}}})
    };
    let e = draft(
        w,
        f.form,
        Part::Layout,
        layout(json!({"all": [{"field": "status_category", "op": "eq", "value": "done"}]})),
    )
    .await
    .unwrap_err();
    assert_eq!(kind_of(&e), "hides_required");
    draft(
        w,
        f.form,
        Part::Layout,
        layout(json!({"all": [{"field": "assignee", "op": "is_set"}]})),
    )
    .await
    .unwrap();
    // Every kind the code reports is listed in the contract constants.
    for k in [
        "restricted_condition_field",
        "condition_too_complex",
        "hides_required",
        "cycle",
        "unknown_field",
    ] {
        assert!(forms::INVALID_KINDS.contains(&k));
    }
}

// ---------------------------------------------------------------- T-M1-49, 50, stale expected_rules_version

#[tokio::test]
async fn t49_t50_expected_rules_version() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_d, token, agent) = agent(w).await;
    let (i, _) = mk(w, "agent target").await;
    let fd = forms::describe_form(&w.app, agent, "WEB", "Bug", "create", None)
        .await
        .unwrap();
    let rv0 = fd.rules_version.clone();
    let lv0 = fd.layout_version.clone();
    let with_expected = |d: Value, v: &str| {
        let mut d = d;
        d["expected_rules_version"] = json!(v);
        d
    };
    // T-M1-49: a layout publish changes layout_version only.
    let r = publish(
        &f,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "columns": 2,
             "fields": [{"key": "subject", "span": 2}, {"key": "assigned_to_id"}]}
        ]}}}),
        None,
    )
    .await
    .unwrap();
    assert_eq!(r.rules_version, rv0);
    let fd = forms::describe_form(&w.app, agent, "WEB", "Bug", "create", None)
        .await
        .unwrap();
    assert_eq!(fd.rules_version, rv0);
    assert_ne!(fd.layout_version, lv0);
    assert_eq!(fd.layout.sections[0].id, "main");
    match changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &with_expected(upd(i, 0, json!({"subject": "layout only"})), &rv0),
        false,
        ORIGIN,
    )
    .await
    .unwrap()
    {
        SubmitResult::Applied { .. } => {}
        other => panic!("not applied: {other:?}"),
    }
    // T-M1-50: a rules publish → form_rules_changed for the old version.
    let r = publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    assert_ne!(r.rules_version, rv0);
    let stale = with_expected(upd(i, lv(w, i).await, json!({"subject": "stale"})), &rv0);
    let e = changesets::command_submit(&w.app, agent, Uuid::now_v7(), &stale, false, ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::FormRulesChanged);
    assert_eq!(details(&e)["expected"], json!(rv0));
    assert_eq!(details(&e)["current"], json!(r.rules_version));
    // A person sees the same; validate rejects it as a real submit would.
    let e = human(w, w.alice_ctx, &stale).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::FormRulesChanged);
    let e = forms::validate_human(
        &w.app,
        w.alice_ctx,
        &validate_input(upd(i, lv(w, i).await, json!({"subject": "x"})), Some(&rv0)),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::FormRulesChanged);
    // With the current version it passes.
    let fresh = with_expected(
        upd(i, lv(w, i).await, json!({"subject": "fresh"})),
        &r.rules_version,
    );
    human(w, w.alice_ctx, &fresh).await.unwrap();
    // Over MCP: validate with the stale version.
    let mut m = world::mcp_client::Mcp::connect(router(w), token).await;
    let e = m
        .call(
            "validate",
            json!({"command": upd(i, lv(w, i).await, json!({"subject": "x"})),
                   "expected_rules_version": rv0}),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("form_rules_changed"), "{e}");
    // A layout rollback publishes an old body under a new number and leaves
    // the rules version alone.
    let rb = forms::rollback(
        &w.app,
        &w.policy,
        w.alice_ctx,
        f.form,
        &RollbackInput {
            part: "layout".into(),
            to_version: 1,
            mode: None,
        },
    )
    .await
    .unwrap();
    assert!(rb.version > 1);
    assert_eq!(rb.rules_version, r.rules_version);
}

// ---------------------------------------------------------------- T-M1-51

#[tokio::test]
async fn t51_publish_modes_and_marks() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = mk(w, "breaks it").await;
    set(w, w.alice_ctx, i, json!({"assigned_to_id": w.bob}))
        .await
        .unwrap();
    let (fine, _) = mk(w, "fine").await;
    // The impact names it.
    let v = draft(w, f.form, Part::Rules, rules(vec![due_when_assigned()]))
        .await
        .unwrap();
    let imp = forms::impact(&w.app, w.alice_ctx, f.form, Part::Rules)
        .await
        .unwrap();
    assert_eq!(imp.draft_version, v);
    let ex = imp.existing_issues.as_ref().unwrap();
    assert_eq!(ex.violating, 1);
    assert_eq!(ex.sample_keys, vec![key.clone()]);
    assert!(ex.complete);
    // block: refused with the impact.
    let e = forms::publish(
        &w.app,
        &w.policy,
        w.alice_ctx,
        f.form,
        Part::Rules,
        &PublishInput {
            draft_version: v,
            mode: Some("block".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::FormPublishBlocked);
    assert_eq!(details(&e)["existing_issues"]["violating"], 1);
    // A stale draft version: conflict.
    let e = forms::publish(
        &w.app,
        &w.policy,
        w.alice_ctx,
        f.form,
        Part::Rules,
        &PublishInput {
            draft_version: v + 7,
            mode: Some("accept_and_mark".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // accept_and_mark: published, the issue marked.
    let r = forms::publish(
        &w.app,
        &w.policy,
        w.alice_ctx,
        f.form,
        Part::Rules,
        &PublishInput {
            draft_version: v,
            mode: Some("accept_and_mark".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(r.marked, 1);
    assert_eq!(open_marks(w, i).await, 1);
    assert_eq!(open_marks(w, fine).await, 0);
    let rule_ids: Vec<String> =
        sqlx::query_scalar("SELECT rule_ids FROM form_rule_marks WHERE issue_id = $1")
            .bind(i)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rule_ids, vec!["due_when_assigned".to_string()]);
    // A note does not touch fields: the mark stays.
    let l = lv(w, i).await;
    human(
        w,
        w.alice_ctx,
        &doc_of(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "n"})],
            vec![(i, l)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(open_marks(w, i).await, 1);
    // A write that passes clears it.
    set(w, w.alice_ctx, i, json!({"due_date": "2026-12-31"}))
        .await
        .unwrap();
    assert_eq!(open_marks(w, i).await, 0);
    // Versions: the published one and nothing in draft.
    let vs = forms::versions(&w.app, w.alice_ctx, f.form, Part::Rules)
        .await
        .unwrap();
    assert_eq!(vs[0].state, "published");
    assert_eq!(vs[0].publish_mode.as_deref(), Some("accept_and_mark"));
    // Discarding when there is no draft: not_found.
    let e = forms::discard(&w.app, w.alice_ctx, f.form, Part::Rules)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // bob (not an administrator, no manage_project) cannot manage forms.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = forms::get(&w.app, bob, f.form).await.unwrap_err();
    assert!(
        matches!(code(&e), ErrorCode::ForbiddenOp | ErrorCode::NotFound),
        "{e:#}"
    );
}

// ---------------------------------------------------------------- T-M1-54

#[tokio::test]
async fn t54_validate_writes_nothing() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    let (i, _) = mk(w, "validate me").await;
    let snapshot = || async move {
        (
            count(&w.pool, "SELECT count(*) FROM operation_requests").await,
            count(&w.pool, "SELECT count(*) FROM changesets").await,
            count(&w.pool, "SELECT count(*) FROM issue_events").await,
            lv(w, i).await,
        )
    };
    let before = snapshot().await;
    let ok = upd(i, 0, json!({"subject": "would change"}));
    let bad = upd(i, 0, json!({"assigned_to_id": w.bob}));
    for n in 0..1000 {
        let d = if n % 2 == 0 { &ok } else { &bad };
        let r = forms::validate_human(&w.app, w.alice_ctx, &validate_input(d.clone(), None))
            .await
            .unwrap();
        assert_eq!(r.outcome, if n % 2 == 0 { "valid" } else { "invalid" });
        assert!(!r.would_require_review);
    }
    assert_eq!(snapshot().await, before);
    // Agents: 120 an hour per delegation, then quota_exceeded.
    let (d, _token, agent) = agent(w).await;
    let r = changesets::validate_command(&w.app, agent, &validate_input(ok.clone(), None))
        .await
        .unwrap();
    assert_eq!(r.outcome, "valid");
    exec(
        &w.pool,
        &format!(
            "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
             SELECT agent_principal_id, context_id, id, 'validate' FROM delegations, generate_series(1, 119)
             WHERE id = '{}'",
            d.id
        ),
    )
    .await;
    let e = changesets::validate_command(&w.app, agent, &validate_input(ok.clone(), None))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::QuotaExceeded);
    assert_eq!(snapshot().await.1, before.1);
    assert_eq!(lv(w, i).await, 0);
}

// ---------------------------------------------------------------- T-M1-56

#[tokio::test]
async fn t56_conformance_vectors() {
    let file: Value =
        serde_json::from_str(include_str!("../../../api/form-conformance.json")).unwrap();
    let types: BTreeMap<i32, String> = file["cf_types"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.parse().unwrap(), v.as_str().unwrap().to_string()))
        .collect();
    let vectors = file["vectors"].as_array().unwrap();
    assert!(vectors.len() >= 30);
    for v in vectors {
        let got = forms::eval_vector(&v["when"], &v["values"], &types)
            .unwrap_or_else(|e| panic!("{}: {e:#}", v["name"]));
        assert_eq!(got, v["expected"].as_bool().unwrap(), "{}", v["name"]);
    }
}

// ---------------------------------------------------------------- leakage

#[tokio::test]
async fn describe_form_leaks_nothing_unreadable() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    grant(&pool, lead, &["view_issues", "edit_issues"]).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{lead}' FROM members
             WHERE project_id = '{}' AND principal_id = '{}'",
            w.web, w.carol
        ),
    )
    .await;
    let mut secret = field_input("Secret", "list", vec![w.bug], &["TopSecretOption"]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    let secret_opt = secret.options[0].id;
    publish(
        &f,
        Part::Rules,
        rules(vec![
            json!({"id": "secret_rule", "field": secret.key, "kind": "required_if",
                   "when": {"all": [{"field": "status_category", "op": "eq", "value": "todo"}]}}),
            json!({"id": "secret_values", "field": secret.key, "kind": "allowed_values_if",
                   "when": {"all": [{"field": "due_date", "op": "is_set"}]},
                   "values": [{"list": secret_opt}]}),
        ]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    publish(
        &f,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}, {"key": secret.key}]}
        ]}}}),
        None,
    )
    .await
    .unwrap();
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let fd = describe(w, bob).await;
    let text = serde_json::to_string(&fd).unwrap();
    for needle in [
        secret.key.as_str(),
        "Secret",
        "TopSecretOption",
        &secret_opt.to_string(),
        "secret_rule",
        "secret_values",
    ] {
        assert!(!text.contains(needle), "bob sees {needle}: {text}");
    }
    assert!(
        fd.layout
            .sections
            .iter()
            .all(|s| s.fields.iter().all(|x| x.key != secret.key))
    );
    // carol (Lead) sees the field, its option and the rules on it.
    let fd = describe(w, carol).await;
    let field = fd.fields.iter().find(|x| x.key == secret.key).unwrap();
    assert_eq!(field.required_if[0].rule_id, "secret_rule");
    assert_eq!(field.allowed_values_if[0].rule_id, "secret_values");
    assert!(
        serde_json::to_string(&fd)
            .unwrap()
            .contains("TopSecretOption")
    );
    // Writes: bob is not held to a rule about a field he cannot read;
    // carol is; the agent of alice is blocked without the name.
    let (i, _) = mk(w, "secret").await;
    set(w, bob, i, json!({"subject": "bob writes"}))
        .await
        .unwrap();
    let e = set(w, carol, i, json!({"subject": "carol writes"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    assert_eq!(details(&e)["rule_id"], "secret_rule");
    let (_d, _token, agent) = agent(w).await;
    let e = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i, lv(w, i).await, json!({"subject": "agent writes"})),
        false,
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    let d = details(&e);
    assert_eq!(d["blocked"], true, "{d}");
    let text = d.to_string();
    assert!(
        !text.contains(&secret.key) && !text.contains("secret_rule"),
        "{text}"
    );
    // validate for bob does not name it either.
    let r = forms::validate_human(
        &w.app,
        bob,
        &validate_input(upd(i, lv(w, i).await, json!({"subject": "v"})), None),
    )
    .await
    .unwrap();
    assert_eq!(r.outcome, "valid");
    // Marks: the same rules again with accept_and_mark mark the issue; only
    // a reader of the field sees the rule id on the issue page.
    let body = forms::get(&w.app, w.alice_ctx, f.form)
        .await
        .unwrap()
        .rules
        .published
        .unwrap()
        .body;
    let p = publish(&f, Part::Rules, body, Some("accept_and_mark"))
        .await
        .unwrap();
    assert!(p.marked >= 1);
    let key: String =
        sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
            .bind(i)
            .fetch_one(&pool)
            .await
            .unwrap();
    let seen = service::get(&w.app, bob, &key).await.unwrap();
    assert!(
        seen.form_rule_marks.is_empty(),
        "{:?}",
        seen.form_rule_marks
    );
    let seen = service::get(&w.app, carol, &key).await.unwrap();
    assert_eq!(seen.form_rule_marks, vec!["secret_rule".to_string()]);
}

// ---------------------------------------------------------------- MCP

#[tokio::test]
async fn mcp_describe_form_and_validate() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    let (d, token, _agent) = agent(w).await;
    let (i, key) = mk(w, "mcp").await;
    let mut m = world::mcp_client::Mcp::connect(router(w), token).await;
    let listed = m.rpc("tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"describe_form") && names.contains(&"validate"));
    let fd = m
        .call(
            "describe_form",
            json!({"project": "WEB", "tracker": "Bug", "op": "create"}),
        )
        .await
        .unwrap();
    let rv = fd["rules_version"].as_str().unwrap().to_string();
    let due = fd["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["key"] == "due_date")
        .unwrap();
    assert_eq!(due["required_if"][0]["rule_id"], "due_when_assigned");
    // Not in the delegation's writable fields: not editable, reason policy.
    assert_eq!(due["editable"], false, "{due}");
    let subject = fd["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["key"] == "subject")
        .unwrap();
    assert_eq!(subject["editable"], true, "{subject}");
    let upd_fd = m
        .call(
            "describe_form",
            json!({"project": "WEB", "tracker": "Bug", "op": "update", "issue_key": key}),
        )
        .await
        .unwrap();
    assert_eq!(upd_fd["lock_version"], 0);
    let v = m
        .call(
            "validate",
            json!({"command": upd(i, 0, json!({"subject": "ok"})), "expected_rules_version": rv}),
        )
        .await
        .unwrap();
    assert_eq!(v["outcome"], "valid", "{v}");
    assert_eq!(v["would_require_review"], false);
    let v = m
        .call(
            "validate",
            json!({"command": upd(i, 0, json!({"assigned_to_id": w.alice})), "expected_rules_version": rv}),
        )
        .await
        .unwrap();
    assert_eq!(v["outcome"], "invalid", "{v}");
    assert_eq!(v["items"][0]["errors"][0]["code"], "required_field_missing");
    assert_eq!(v["items"][0]["errors"][0]["rule_id"], "due_when_assigned");
    // A review policy shows in would_require_review.
    agents::set_delegation_policies(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        PolicyInput {
            commands: BTreeMap::from([("update_fields".to_string(), PolicyMode::Review)]),
            fields: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    let v = m
        .call(
            "validate",
            json!({"command": upd(i, 0, json!({"subject": "ok"}))}),
        )
        .await
        .unwrap();
    assert_eq!(v["would_require_review"], true, "{v}");
    // Nothing was written.
    assert_eq!(lv(w, i).await, 0);
}

// ---------------------------------------------------------------- field management (T-M1-57, 58, 59)

#[tokio::test]
async fn t57_visibility_quarantines_agents() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_d, _token, agent) = agent(w).await;
    let (i, _) = mk(w, "q").await;
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    let (e_web, e_auth) = (epoch(&pool, w.web).await, epoch(&pool, w.auth_p).await);
    let l0 = count(&pool, "SELECT form_l0_version FROM org_settings").await;
    let lv0 = field_admin::get(
        &w.app,
        w.alice_ctx,
        &format!("cf:{}", f.sev),
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    let r = field_admin::patch(
        &w.app,
        w.alice_ctx,
        &f.sev.to_string(),
        &FieldPatch {
            lock_version: lv0,
            set: FieldSet {
                visible: Some(false),
                role_ids: Some(vec![lead]),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert!(!r.field.visible);
    assert_eq!(r.field.lock_version, lv0 + 1);
    // Every project the field applies to moves (once per changed part:
    // visibility and roles here).
    let (e_web2, e_auth2) = (epoch(&pool, w.web).await, epoch(&pool, w.auth_p).await);
    assert!(
        e_web2 > e_web && e_auth2 > e_auth,
        "{e_web}→{e_web2}, {e_auth}→{e_auth2}"
    );
    assert!(count(&pool, "SELECT form_l0_version FROM org_settings").await > l0);
    let e = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i, 0, json!({"subject": "after"})),
        false,
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    // A rename moves neither the epoch nor L0.
    let l0 = count(&pool, "SELECT form_l0_version FROM org_settings").await;
    field_admin::patch(
        &w.app,
        w.alice_ctx,
        &f.sev.to_string(),
        &FieldPatch {
            lock_version: lv0 + 1,
            set: FieldSet {
                name: Some("Severity (renamed)".into()),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(epoch(&pool, w.web).await, e_web2);
    assert_eq!(
        count(&pool, "SELECT form_l0_version FROM org_settings").await,
        l0
    );
}

#[tokio::test]
async fn field_lock_version_and_errors() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let key = format!("cf:{}", f.sev);
    // Two administrators change the field at once: one wins.
    let one = FieldPatch {
        lock_version: 0,
        set: FieldSet {
            description: Some("one".into()),
            ..Default::default()
        },
    };
    let two = FieldPatch {
        lock_version: 0,
        set: FieldSet {
            description: Some("two".into()),
            ..Default::default()
        },
    };
    let a = field_admin::patch(&w.app, w.alice_ctx, &key, &one);
    let b = field_admin::patch(&w.app, w.alice_ctx, &key, &two);
    let (a, b) = common::wait_for("two racing field patches", common::WAIT_TIMEOUT, async {
        tokio::join!(a, b)
    })
    .await;
    let (ok, err) = match (a, b) {
        (Ok(x), Err(e)) | (Err(e), Ok(x)) => (x, e),
        (a, b) => panic!("one must win: {:?} {:?}", a.is_ok(), b.is_ok()),
    };
    assert_eq!(ok.field.lock_version, 1);
    assert_eq!(code(&err), ErrorCode::Conflict);
    assert_eq!(details(&err)["current_lock_version"], 1);
    // ftype never changes in place.
    let e = field_admin::patch(
        &w.app,
        w.alice_ctx,
        &key,
        &FieldPatch {
            lock_version: 1,
            set: FieldSet {
                ftype: Some(json!("string")),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UnsupportedOperation);
    assert_eq!(details(&e)["suggested"], "conversion");
    // Required: the answer lists issues without a value.
    let (i, _) = mk(w, "no severity").await;
    let r = field_admin::patch(
        &w.app,
        w.alice_ctx,
        &key,
        &FieldPatch {
            lock_version: 1,
            set: FieldSet {
                is_required: Some(true),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    let imp = serde_json::to_value(r.required_impact.unwrap()).unwrap();
    assert!(
        imp.to_string().contains(&i.to_string()) || imp.to_string().contains("WEB-"),
        "{imp}"
    );
    // A name taken; a non-administrator.
    let e = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("severity", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "name_taken");
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = field_admin::list(&w.app, bob).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // An option in use cannot be deleted; deactivating keeps it.
    set(w, w.alice_ctx, i, json!({ key.clone(): {"list": f.low} }))
        .await
        .unwrap();
    let lv_now = field_admin::get(
        &w.app,
        w.alice_ctx,
        &key,
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    let e = field_admin::delete_option(&w.app, w.alice_ctx, &key, f.low, lv_now)
        .await
        .unwrap_err();
    assert_eq!(kind(&e), "option_in_use");
    let v = field_admin::patch_option(
        &w.app,
        w.alice_ctx,
        &key,
        f.low,
        &field_admin::OptionPatch {
            lock_version: lv_now,
            label: None,
            position: None,
            active: Some(false),
        },
    )
    .await
    .unwrap();
    assert!(!v.options.iter().find(|o| o.id == f.low).unwrap().active);
    // An inactive option takes no new values.
    let (j, _) = mk(w, "inactive option").await;
    let e = set(w, w.alice_ctx, j, json!({ key.clone(): {"list": f.low} }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    let _ = f.medium;
}

#[tokio::test]
async fn t58_conversion_with_failure_and_resume() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    // string → int: three values do not parse.
    let text = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Estimate text", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap();
    let mut issues = Vec::new();
    for (n, v) in ["1", "22", "nope", "333", "x9", "4.5.6"].iter().enumerate() {
        let (i, k) = mk(w, &format!("conv {n}")).await;
        set(
            w,
            w.alice_ctx,
            i,
            json!({ text.key.clone(): {"string": v} }),
        )
        .await
        .unwrap();
        issues.push((i, k));
    }
    let mut to = field_input("Estimate", "int", vec![w.bug], &[]);
    to.description = Some("converted".into());
    let p = field_admin::preview_conversion(
        &w.app,
        w.alice_ctx,
        &text.key,
        &ConversionInput {
            to,
            mapping: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(p.convertible, 3);
    assert_eq!(p.failures.count, 3);
    let mut sample = p.failures.sample_issue_keys.clone();
    sample.sort();
    let mut expected: Vec<String> = [2usize, 4, 5]
        .iter()
        .map(|n| issues[*n].1.clone())
        .collect();
    expected.sort();
    assert_eq!(sample, expected);
    // Batches of 2; the second batch fails once.
    field_admin::set_batch_size(2);
    faults::arm("cf_convert.batch", Action::Fail);
    let v = field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, p.conversion_id)
        .await
        .unwrap();
    assert_eq!(v.state, "running");
    let v = wait_state(w, p.conversion_id, "failed").await;
    assert_eq!(v.batches_done, 1);
    let lvs_mid: Vec<i32> = {
        let mut out = Vec::new();
        for (i, _) in &issues {
            out.push(lv(w, *i).await);
        }
        out
    };
    // Resume: from the cursor; nothing is written twice.
    field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, p.conversion_id)
        .await
        .unwrap();
    let v = wait_state(w, p.conversion_id, "succeeded").await;
    let report = v.report.unwrap();
    assert_eq!(report["converted"], 3, "{report}");
    assert_eq!(report["failed"], 3, "{report}");
    let reasons: Vec<&str> = report["failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, vec!["unparseable"; 3]);
    assert!(!report.to_string().contains("nope"), "no values in reports");
    let op = format!("cf_convert:{}", p.conversion_id);
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_events WHERE system_operation = '{op}'
                   AND origin = 'import' AND actor_kind = 'system'"
            )
        )
        .await,
        3
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_cf WHERE field_id = {}",
                p.to_field_id
            )
        )
        .await,
        3
    );
    // The issue_field_changes projection (M1.0) covers status, assignee,
    // version and priority only; custom fields stay out of it.
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_field_changes WHERE field_key = 'cf:{}'",
                p.to_field_id
            )
        )
        .await,
        0
    );
    // Every converted issue moved exactly once; the others not at all.
    for (n, (i, _)) in issues.iter().enumerate() {
        let after = lv(w, *i).await;
        let converted = ![2usize, 4, 5].contains(&n);
        assert!(
            after == lvs_mid[n] || (after == lvs_mid[n] + 1 && converted),
            "issue {n}: {} → {after}",
            lvs_mid[n]
        );
        let total_events = count(
            &pool,
            &format!("SELECT count(*) FROM issue_events WHERE issue_id = '{i}' AND system_operation = '{op}'"),
        )
        .await;
        assert_eq!(total_events, i64::from(converted), "issue {n}");
    }
    let v: i64 = sqlx::query_scalar(
        "SELECT v_num::bigint FROM issue_cf WHERE issue_id = $1 AND field_id = $2",
    )
    .bind(issues[1].0)
    .bind(p.to_field_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(v, 22);
    // Started again after success: conflict.
    let e = field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, p.conversion_id)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);

    // list → list with a mapping; a running conversion blocks deleting the
    // old field.
    let mut to = field_input("Severity 2", "list", vec![w.bug], &["Minor", "Major"]);
    to.is_for_all = Some(true);
    let sev_key = format!("cf:{}", f.sev);
    for (n, o) in [f.low, f.high, f.low].iter().enumerate() {
        set(
            w,
            w.alice_ctx,
            issues[n].0,
            json!({ sev_key.clone(): {"list": o} }),
        )
        .await
        .unwrap();
    }
    let e = field_admin::preview_conversion(
        &w.app,
        w.alice_ctx,
        &sev_key,
        &ConversionInput {
            to: field_input("Severity 3", "list", vec![w.bug], &["Minor"]),
            mapping: json!({"options": {f.low.to_string(): "Nope"}}),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "mapping_invalid");
    let p = field_admin::preview_conversion(
        &w.app,
        w.alice_ctx,
        &sev_key,
        &ConversionInput {
            to,
            mapping: json!({"options": {f.low.to_string(): "Minor", f.high.to_string(): "Major"}}),
        },
    )
    .await
    .unwrap();
    assert_eq!(p.convertible, 3);
    assert_eq!(p.failures.count, 0);
    field_admin::set_batch_size(1);
    let gate = Arc::new(Gate::default());
    faults::arm("cf_convert.batch", Action::Block(gate.clone()));
    field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, p.conversion_id)
        .await
        .unwrap();
    // The second batch starts (bounded: the gate panics with the state).
    gate.reached.notified().await;
    let lv_sev = field_admin::get(
        &w.app,
        w.alice_ctx,
        &sev_key,
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    let e = field_admin::delete(&w.app, &w.policy, w.alice_ctx, &sev_key, lv_sev)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    assert_eq!(kind(&e), "conversion_running");
    gate.release.notify_one();
    let v = wait_state(w, p.conversion_id, "succeeded").await;
    assert_eq!(v.report.unwrap()["converted"], 3);
    let minor = count(
        &pool,
        &format!(
            "SELECT count(*) FROM issue_cf c JOIN field_options o ON o.id = c.v_ref
             WHERE c.field_id = {} AND o.label = 'Minor'",
            p.to_field_id
        ),
    )
    .await;
    assert_eq!(minor, 2);
    field_admin::set_batch_size(500);
    faults::clear();
}

async fn wait_state(w: &World, id: Uuid, want: &str) -> field_admin::ConversionView {
    common::poll_until(
        &format!("conversion {id} to reach {want}"),
        common::WAIT_TIMEOUT,
        async || {
            let v = field_admin::get_conversion(&w.app, w.alice_ctx, id)
                .await
                .unwrap();
            if v.state == want {
                common::Break(v)
            } else {
                common::Continue(format!("state={}", v.state))
            }
        },
    )
    .await
}

#[tokio::test]
async fn t59_deactivated_field() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let key = format!("cf:{}", f.sev);
    let (i, ik) = mk(w, "has severity").await;
    set(w, w.alice_ctx, i, json!({ key.clone(): {"list": f.high} }))
        .await
        .unwrap();
    let before = describe(w, w.alice_ctx).await;
    assert!(before.fields.iter().any(|x| x.key == key));
    let r = field_admin::patch(
        &w.app,
        w.alice_ctx,
        &key,
        &FieldPatch {
            lock_version: 0,
            set: FieldSet {
                deactivated: Some(true),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert!(r.field.deactivated);
    // Not in describe_form or the general describe; the rules version moved (L0).
    let after = describe(w, w.alice_ctx).await;
    assert!(after.fields.iter().all(|x| x.key != key));
    assert_ne!(after.rules_version, before.rules_version);
    // A form rule or layout cannot name it any more.
    let e = draft(
        w,
        f.form,
        Part::Rules,
        rules(vec![json!({"id": "r", "field": key, "kind": "required_if",
                          "when": {"all": [{"field": "assignee", "op": "is_set"}]}})]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "unknown_field");
    // No new values.
    let (j, _) = mk(w, "new value").await;
    let e = set(w, w.alice_ctx, j, json!({ key.clone(): {"list": f.low} }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    // Filters still find the old value.
    let res = service::search(
        &w.app,
        w.alice_ctx,
        SearchParams {
            where_: Some(json!({"field": key, "op": "eq", "value": f.high.to_string()})),
            limit: Some(100),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let keys: Vec<String> = res.items.iter().map(|x| x.key.clone()).collect();
    assert_eq!(keys, vec![ik]);
    // Reactivated: back in the form.
    field_admin::patch(
        &w.app,
        w.alice_ctx,
        &key,
        &FieldPatch {
            lock_version: 1,
            set: FieldSet {
                deactivated: Some(false),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert!(
        describe(w, w.alice_ctx)
            .await
            .fields
            .iter()
            .any(|x| x.key == key)
    );
}

// ---------------------------------------------------------------- CHECK lists (0024, 0025)

async fn check_values(pool: &sqlx::PgPool, table: &str, column: &str) -> Vec<String> {
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c
         JOIN pg_class t ON t.oid = c.conrelid WHERE t.relname = $1 AND c.contype = 'c'",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap();
    let def = defs
        .iter()
        .find(|d| d.contains(&format!("({column} = ANY")) || d.contains(&format!("{column} = ANY")))
        .unwrap_or_else(|| panic!("{table}.{column}: no CHECK in {defs:?}"));
    let mut out = Vec::new();
    let mut rest = def.as_str();
    while let Some(a) = rest.find('\'') {
        let tail = &rest[a + 1..];
        let b = tail.find('\'').unwrap();
        out.push(tail[..b].to_string());
        rest = &tail[b + 1..];
    }
    out.sort();
    out
}

#[tokio::test]
async fn check_lists_match_constants() {
    let pool = test_pool().await;
    let sorted = |xs: &[&str]| {
        let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };
    assert_eq!(
        check_values(&pool, "form_rule_versions", "state").await,
        sorted(&forms::VERSION_STATES)
    );
    assert_eq!(
        check_values(&pool, "form_layout_versions", "state").await,
        sorted(&forms::VERSION_STATES)
    );
    assert_eq!(
        check_values(&pool, "form_rule_versions", "publish_mode").await,
        sorted(&forms::PUBLISH_MODES)
    );
    assert_eq!(
        check_values(&pool, "custom_field_conversions", "state").await,
        sorted(&field_admin::CONVERSION_STATES)
    );
}

// ---------------------------------------------------------------- HTTP

const ORIGIN_SPA: &str = "https://spa.test";

struct Browser {
    session: String,
    csrf: String,
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
    let cookie = lc
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{LOGIN_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
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
    let session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

async fn call(
    r: &Router,
    b: Option<&Browser>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value, HeaderMap) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::ORIGIN, ORIGIN_SPA);
    if let Some(b) = b {
        req = req
            .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
            .header("x-csrf-token", &b.csrf);
    }
    // Writes carry the JSON content type even without a body (CSRF).
    if method != "GET" {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    let resp = r.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v, headers)
}

#[tokio::test]
async fn m1_5_http() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let r = router(w);
    let alice = login(&r, "alice").await;
    let bob = login(&r, "bob").await;
    for (m, path) in [
        ("GET", "/projects/WEB/trackers/Bug/form"),
        ("GET", "/admin/forms"),
        ("GET", "/admin/custom-fields"),
    ] {
        let (st, v, _) = call(&r, None, m, path, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{path}: {v}");
    }
    // The form for a person.
    let (st, v, _) = call(
        &r,
        Some(&bob),
        "GET",
        "/projects/WEB/trackers/Bug/form",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let rv0 = v["data"]["rules_version"].as_str().unwrap().to_string();
    assert_eq!(v["data"]["layout"]["sections"][0]["id"], "default");
    // Administration: ensure (idempotent), draft, impact, publish.
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        "/admin/forms",
        Some(json!({"tracker_id": w.bug})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["id"], json!(f.form));
    let (st, v, _) = call(
        &r,
        Some(&bob),
        "POST",
        "/admin/forms",
        Some(json!({"tracker_id": w.bug})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    let base = format!("/forms/{}", f.form);
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "PUT",
        &format!("{base}/drafts/rules"),
        Some(
            json!({"base_version": 0, "body": {"rules": [{"id": "bad", "field": "due_date",
            "kind": "required_if", "when": {"all": [{"field": "nope", "op": "is_set"}]}}]}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["code"], "form_invalid");
    assert_eq!(v["error"]["details"]["kind"], "unknown_field");
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "PUT",
        &format!("{base}/drafts/rules"),
        Some(json!({"base_version": 0, "body": rules(vec![due_when_assigned()])})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let dv = v["data"]["version"].as_i64().unwrap();
    // A second writer with the old base: conflict.
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "PUT",
        &format!("{base}/drafts/rules"),
        Some(json!({"base_version": 0, "body": rules(vec![])})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["details"]["current_version"], dv);
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("{base}/drafts/rules/impact"),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["existing_issues"]["violating"], 0);
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("{base}/drafts/rules/publish"),
        Some(json!({"draft_version": dv, "mode": "block"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let rv1 = v["data"]["rules_version"].as_str().unwrap().to_string();
    assert_ne!(rv1, rv0);
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "GET",
        &format!("{base}/versions?part=rules"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"][0]["state"], "published");
    let (st, v, _) = call(&r, Some(&alice), "GET", "/admin/forms", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"][0]["published_rules_version"], dv);
    // /commands/validate: read-only, no Idempotency-Key.
    let (i, _) = mk(w, "http").await;
    let (st, v, _) = call(
        &r,
        Some(&bob),
        "POST",
        "/commands/validate",
        Some(json!({"command": upd(i, 0, json!({"assigned_to_id": w.bob}))})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["outcome"], "invalid");
    assert_eq!(
        v["data"]["items"][0]["errors"][0]["rule_id"],
        "due_when_assigned"
    );
    let (st, v, _) = call(
        &r,
        Some(&bob),
        "POST",
        "/commands/validate",
        Some(json!({"command": upd(i, 0, json!({"subject": "x"})), "expected_rules_version": rv0})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["code"], "form_rules_changed");
    assert_eq!(lv(w, i).await, 0);
    // Custom fields.
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        "/admin/custom-fields",
        Some(
            json!({"name": "Browser", "ftype": "list", "is_for_all": true,
                    "tracker_ids": [w.bug], "options": [{"label": "Safari"}]}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let cf = v["data"]["id"].as_i64().unwrap();
    assert_eq!(v["data"]["key"], format!("cf:{cf}"));
    let (st, v, _) = call(&r, Some(&bob), "GET", "/admin/custom-fields", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("/admin/custom-fields/{cf}/options"),
        Some(json!({"label": "Firefox"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let lock = v["data"]["lock_version"].as_i64().unwrap();
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "PATCH",
        &format!("/admin/custom-fields/cf:{cf}"),
        Some(json!({"lock_version": lock + 5, "set": {"description": "x"}})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["details"]["current_lock_version"], lock);
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "GET",
        &format!("/admin/custom-fields/{cf}?preview_required=true"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["data"]["required_impact"].is_object(), "{v}");
    let (st, v, headers) = call(
        &r,
        Some(&alice),
        "DELETE",
        &format!("/admin/custom-fields/{cf}?lock_version={lock}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    assert_eq!(
        headers.get(header::LOCATION).unwrap(),
        &format!("/api/v1/admin/custom-fields/{cf}")
    );
    assert_eq!(v["data"]["index_state"], "dropping");
    // A conversion preview over HTTP (201).
    let sev = f.sev;
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("/admin/custom-fields/{sev}/conversions"),
        Some(
            json!({"to": {"name": "Severity text", "ftype": "string", "is_for_all": true,
                           "tracker_ids": [w.bug]}, "mapping": {}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let conv = v["data"]["conversion_id"].as_str().unwrap().to_string();
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "GET",
        &format!("/admin/conversions/{conv}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["state"], "previewed");
    // The issue page carries open marks.
    let (j, jk) = mk(w, "marked").await;
    set(w, w.alice_ctx, j, json!({"assigned_to_id": w.bob}))
        .await
        .unwrap_err();
    exec(
        &pool,
        &format!(
            "UPDATE issues SET assigned_to_id = '{}' WHERE id = '{j}'",
            w.bob
        ),
    )
    .await;
    let d = forms::get(&w.app, w.alice_ctx, f.form).await.unwrap();
    forms::put_draft(
        &w.app,
        w.alice_ctx,
        f.form,
        Part::Rules,
        &DraftInput {
            base_version: d.summary.published_rules_version,
            body: rules(vec![
                due_when_assigned(),
                json!({"id": "extra", "field": "start_date", "kind": "readonly_if",
                       "when": {"all": [{"field": "status_category", "op": "eq", "value": "done"}]}}),
            ]),
        },
    )
    .await
    .unwrap();
    let dv2 = forms::get(&w.app, w.alice_ctx, f.form)
        .await
        .unwrap()
        .rules
        .draft
        .unwrap()
        .version;
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("{base}/drafts/rules/publish"),
        Some(json!({"draft_version": dv2, "mode": "accept_and_mark"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["marked"], 1);
    let (st, v, _) = call(&r, Some(&alice), "GET", &format!("/issues/{jk}"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["data"]["form_rule_marks"],
        json!(["due_when_assigned"]),
        "{v}"
    );
}

// ---------------------------------------------------------------- independent verification (M1.5)
//
// Repros from the M1.5 verification. Append to tests/m1_5_forms.rs (they use
// its helpers: setup, mk, set, rules, publish, draft, details, …).
// v1–v4, v6, v7 fail on the code under review (defects); v5 is a gap test
// for R19 (then_required with form rules) and passes.

/// A non-admin manager of WEB whose role sees only non-private issues
/// (issues_visibility = 'default'), and who holds no restricted-field role.
async fn v_manager(f: &F) -> AuthContext {
    let w = &f.w;
    let role = id(
        &w.pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('VMgr','default') RETURNING id",
    )
    .await;
    grant(
        &w.pool,
        role,
        &["view_issues", "edit_issues", "manage_project"],
    )
    .await;
    let u = oxledger::auth::create_user(&w.pool, "vmgr", "vmgr", PW, false)
        .await
        .unwrap();
    member(&w.pool, w.web, u, role).await;
    sign_in(&w.pool, &w.policy, "vmgr").await
}

async fn v_project_form(f: &F, who: AuthContext) -> Uuid {
    forms::ensure(
        &f.w.app,
        who,
        &NewForm {
            tracker_id: f.w.bug,
            project: Some("WEB".into()),
        },
    )
    .await
    .unwrap()
    .summary
    .id
}

/// D9 says impact counts only what the caller can see. A project manager
/// who cannot see bob's private issue still learns it exists: `scanned`
/// counts it, and `block` is refused with "existing issues break the new
/// rules" while `violating` says 0.
#[tokio::test]
async fn v1_impact_and_block_reveal_invisible_issues() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    create_issue(
        &w.pool,
        NewIssue {
            project_id: w.web,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "bob's private issue",
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: true,
            parent_id: None,
        },
    )
    .await
    .unwrap();
    let form = v_project_form(&f, mgr).await;
    let body = rules(vec![json!({
        "id": "due_todo", "field": "due_date", "kind": "required_if",
        "when": {"all": [{"field": "status_category", "op": "eq", "value": "todo"}]}})]);
    let v = forms::put_draft(
        &w.app,
        mgr,
        form,
        Part::Rules,
        &DraftInput {
            base_version: 0,
            body,
        },
    )
    .await
    .unwrap()
    .version;
    let imp = forms::impact(&w.app, mgr, form, Part::Rules).await.unwrap();
    let ex = imp.existing_issues.unwrap();
    assert_eq!(ex.violating, 0);
    let r = forms::publish(
        &w.app,
        &w.policy,
        mgr,
        form,
        Part::Rules,
        &PublishInput {
            draft_version: v,
            mode: Some("block".into()),
        },
    )
    .await;
    let refused = r.err().map(|e| details(&e));
    assert!(
        ex.scanned == 0
            && refused
                .as_ref()
                .is_none_or(|d| d["existing_issues"]["complete"] == false),
        "invisible issue revealed: scanned = {}, block refusal = {refused:?}",
        ex.scanned
    );
}

/// A non-admin project manager can write rules about a restricted field
/// they cannot read, and impact's sample_keys then tells them which issues
/// have that field empty.
#[tokio::test]
async fn v2_manager_rules_about_unreadable_fields() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    let (with, _) = mk(w, "secret set").await;
    set(
        w,
        w.alice_ctx,
        with,
        json!({ secret.key.clone(): {"string": "x"} }),
    )
    .await
    .unwrap();
    let (_without, without_key) = mk(w, "secret empty").await;
    let form = v_project_form(&f, mgr).await;
    let r = forms::put_draft(
        &w.app,
        mgr,
        form,
        Part::Rules,
        &DraftInput {
            base_version: 0,
            body: rules(vec![json!({
                "id": "probe", "field": secret.key, "kind": "required_if",
                "when": {"all": [{"field": "status_category", "op": "eq", "value": "todo"}]}})]),
        },
    )
    .await;
    if r.is_ok() {
        let imp = forms::impact(&w.app, mgr, form, Part::Rules).await.unwrap();
        let ex = imp.existing_issues.unwrap();
        assert!(
            !ex.sample_keys.contains(&without_key) && ex.violating == 0,
            "impact tells a non-reader which issues have the restricted field empty: {:?}",
            ex.sample_keys
        );
    }
}

/// A condition field restricted after the rules were published: a person
/// who cannot read it is still held to the rule, and the error tells them
/// the hidden value (Severity is High).
#[tokio::test]
async fn v3_condition_field_restricted_after_publish() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let sev = format!("cf:{}", f.sev);
    let (i, _) = mk(w, "high, no due date").await;
    set(w, w.alice_ctx, i, json!({ sev.clone(): {"list": f.high} }))
        .await
        .unwrap();
    publish(
        &f,
        Part::Rules,
        rules(vec![json!({
            "id": "due_high", "field": "due_date", "kind": "required_if",
            "when": {"all": [{"field": sev, "op": "eq", "value": {"list": f.high}}]}})]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    let lv0 = field_admin::get(
        &w.app,
        w.alice_ctx,
        &sev,
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    let restricted = field_admin::patch(
        &w.app,
        w.alice_ctx,
        &f.sev.to_string(),
        &FieldPatch {
            lock_version: lv0,
            set: FieldSet {
                visible: Some(false),
                role_ids: Some(vec![lead]),
                ..Default::default()
            },
        },
    )
    .await;
    if restricted.is_err() {
        // Refusing to restrict a field published rules depend on is a fix too.
        return;
    }
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let r = set(w, bob, i, json!({"subject": "bob edits"})).await;
    assert!(
        r.is_ok(),
        "bob is held to a rule whose condition he cannot read: {}",
        r.err().map(|e| details(&e)).unwrap_or_default()
    );
}

/// Rollback while a draft is open: the draft keeps its lower number, and
/// publishing it afterwards moves published_rules_version backwards
/// (§3.6.3: "版本號遞增，不倒退"), from a draft based on the pre-rollback rules.
#[tokio::test]
async fn v4_publishing_a_draft_after_rollback_goes_backwards() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let p1 = publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    let dv = draft(
        w,
        f.form,
        Part::Rules,
        rules(vec![json!({
            "id": "due_done", "field": "due_date", "kind": "required_if",
            "when": {"all": [{"field": "status_category", "op": "eq", "value": "done"}]}})]),
    )
    .await
    .unwrap();
    let rb = forms::rollback(
        &w.app,
        &w.policy,
        w.alice_ctx,
        f.form,
        &RollbackInput {
            part: "rules".into(),
            to_version: p1.version,
            mode: Some("block".into()),
        },
    )
    .await
    .unwrap();
    assert!(rb.version > dv);
    let r = forms::publish(
        &w.app,
        &w.policy,
        w.alice_ctx,
        f.form,
        Part::Rules,
        &PublishInput {
            draft_version: dv,
            mode: Some("block".into()),
        },
    )
    .await;
    match r {
        Err(e) => assert_eq!(code(&e), ErrorCode::Conflict, "{e:#}"),
        Ok(p) => assert!(
            p.version > rb.version,
            "published version went from {} back to {}",
            rb.version,
            p.version
        ),
    }
}

/// Gap (R19, not covered by the M1.5 tests): `GET /issues/{key}/actions`
/// names the target of a required_if that holds in the target status.
#[tokio::test]
async fn v5_then_required_includes_form_rules() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    publish(
        &f,
        Part::Rules,
        rules(vec![json!({
            "id": "due_done", "field": "due_date", "kind": "required_if",
            "when": {"all": [{"field": "status_category", "op": "eq", "value": "done"}]}})]),
        Some("block"),
    )
    .await
    .unwrap();
    let (_, key) = mk(w, "to be done").await;
    let a = oxledger::describe::issue_actions(&w.app, w.alice_ctx, &key)
        .await
        .unwrap();
    let to_done = a
        .transitions
        .iter()
        .find(|t| t.to_status_id == w.done)
        .expect("new → done");
    assert!(
        to_done.then_required.contains(&"due_date".to_string()),
        "{:?}",
        to_done.then_required
    );
    let to_doing = a.transitions.iter().find(|t| t.to_status_id == w.doing);
    if let Some(t) = to_doing {
        assert!(!t.then_required.contains(&"due_date".to_string()));
    }
}

/// The impact stored with a published version was computed with the
/// publisher's visibility (an admin's), and `GET /forms/{id}` and
/// `/versions` return it to a project manager who cannot see those issues:
/// their keys come back in `sample_keys`.
#[tokio::test]
async fn v6_stored_impact_shows_admin_view_to_managers() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let (_, private_key) = create_issue(
        &w.pool,
        NewIssue {
            project_id: w.web,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "bob's private issue",
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: true,
            parent_id: None,
        },
    )
    .await
    .unwrap();
    let form = v_project_form(&f, w.alice_ctx).await;
    publish_form(
        w,
        form,
        Part::Rules,
        rules(vec![json!({
            "id": "due_todo", "field": "due_date", "kind": "required_if",
            "when": {"all": [{"field": "status_category", "op": "eq", "value": "todo"}]}})]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    let detail = serde_json::to_string(&forms::get(&w.app, mgr, form).await.unwrap()).unwrap();
    let versions = serde_json::to_string(
        &forms::versions(&w.app, mgr, form, Part::Rules)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        !detail.contains(&private_key) && !versions.contains(&private_key),
        "the manager reads the key of a private issue they cannot see: {versions}"
    );
}

/// D13: a write that "passed the checks" clears every mark of the issue.
/// A person who cannot read a rule's target is not held to it, so their
/// write passes and clears the mark while the issue still breaks the rule.
#[tokio::test]
async fn v7_unbound_writer_clears_marks_of_rules_still_broken() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    let (i, _) = mk(w, "secret empty").await;
    let p = publish(
        &f,
        Part::Rules,
        rules(vec![json!({
            "id": "secret_todo", "field": secret.key, "kind": "required_if",
            "when": {"all": [{"field": "status_category", "op": "eq", "value": "todo"}]}})]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    assert!(p.marked >= 1);
    assert_eq!(open_marks(w, i).await, 1);
    let bob = sign_in(&pool, &w.policy, "bob").await;
    set(w, bob, i, json!({"subject": "bob edits"}))
        .await
        .unwrap();
    let still_empty = count(
        &pool,
        &format!(
            "SELECT count(*) FROM issue_cf WHERE issue_id = '{i}' AND field_id = {}",
            secret.id
        ),
    )
    .await
        == 0;
    assert!(still_empty);
    assert_eq!(
        open_marks(w, i).await,
        1,
        "the mark was cleared although the issue still breaks secret_todo"
    );
}

// ---------------------------------------------------------------- verification gaps (M1.5)

async fn lead_role(pool: &sqlx::PgPool) -> Uuid {
    id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await
}

/// A condition on a field that becomes restricted after publishing: the
/// PATCH warns; a person who cannot read the condition field is not held
/// to the rule (although they can read its target); a person who can is
/// held and sees the rule; an agent is blocked without the rule's id.
#[tokio::test]
async fn g_condition_unreadable_target_readable() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let lead = lead_role(&pool).await;
    grant(&pool, lead, &["view_issues", "edit_issues"]).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{lead}' FROM members
             WHERE project_id = '{}' AND principal_id = '{}'",
            w.web, w.carol
        ),
    )
    .await;
    let secret = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Secret", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap();
    let (i, _) = mk(w, "secret set").await;
    set(
        w,
        w.alice_ctx,
        i,
        json!({ secret.key.clone(): {"string": "x"} }),
    )
    .await
    .unwrap();
    publish(
        &f,
        Part::Rules,
        rules(vec![json!({
            "id": "due_if_secret", "field": "due_date", "kind": "required_if",
            "when": {"all": [{"field": secret.key, "op": "is_set"}]}})]),
        Some("accept_and_mark"),
    )
    .await
    .unwrap();
    let d = field_admin::patch(
        &w.app,
        w.alice_ctx,
        &secret.id.to_string(),
        &FieldPatch {
            lock_version: secret.lock_version,
            set: FieldSet {
                visible: Some(false),
                role_ids: Some(vec![lead]),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(d.warnings.len(), 1, "{:?}", d.warnings);
    assert_eq!(d.warnings[0]["kind"], "restricted_condition_field");
    assert_eq!(d.warnings[0]["rule_ids"], json!(["due_if_secret"]));
    // bob reads due_date but not the condition: not held.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    set(w, bob, i, json!({"subject": "bob edits"}))
        .await
        .unwrap();
    // carol (Lead) reads both: held, the rule named.
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let e = set(w, carol, i, json!({"subject": "carol edits"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    assert_eq!(details(&e)["rule_id"], "due_if_secret");
    // The agent: blocked, no rule id, no field.
    let (_d, _token, agent) = agent(w).await;
    let v = changesets::validate_command(
        &w.app,
        agent,
        &validate_input(
            upd(i, lv(w, i).await, json!({"assigned_to_id": w.bob})),
            None,
        ),
    )
    .await
    .unwrap();
    let err = &v.items[0].errors[0];
    assert_eq!(err.code, ErrorCode::RequiredFieldMissing);
    let det = err.details.as_ref().unwrap();
    assert_eq!(det["blocked"], true, "{det}");
    assert!(
        det.get("rule_id").is_none() && err.rule_id.is_none(),
        "{det}"
    );
    assert_eq!(det["fields"], json!([]), "{det}");
    // bob's describe_form does not show the rule on due_date.
    let fd = describe(w, bob).await;
    let due = fd.fields.iter().find(|x| x.key == "due_date").unwrap();
    assert!(due.required_if.is_empty());
    // A write bob is not held to leaves the issue's mark open.
    assert_eq!(open_marks(w, i).await, 1);
}

/// A proposal version stored before 0024 (rules_version NULL) counts as
/// "rules changed" once L1 rules are in force, and not before.
#[tokio::test]
async fn g_pre_0024_proposal_counts_as_changed() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_d, _token, agent) = agent(w).await;
    let (i, _) = mk(w, "old proposal").await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i, 0, json!({"subject": "renamed"})),
        ORIGIN,
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "ALTER TABLE changeset_versions DISABLE TRIGGER changeset_versions_append_only;
             UPDATE changeset_versions SET rules_version = NULL WHERE changeset_id = '{}';
             ALTER TABLE changeset_versions ENABLE TRIGGER changeset_versions_append_only;",
            p.changeset_id
        ),
    )
    .await;
    let changed = || async {
        changesets::get(&w.app, w.alice_ctx, p.changeset_id, None)
            .await
            .unwrap()
            .rules_changed
    };
    assert!(!changed().await, "no L1 rules yet");
    publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    assert!(changed().await, "a NULL version under L1 rules");
}

/// Impact lists only the caller's own proposals: a project manager does
/// not learn of alice's pending proposal (on an issue they can see).
#[tokio::test]
async fn g_impact_pending_only_own_proposals() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let (_d, _token, agent) = agent(w).await;
    let (i, _) = mk(w, "pending").await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i, 0, json!({"assigned_to_id": w.bob})),
        ORIGIN,
    )
    .await
    .unwrap();
    let form = v_project_form(&f, mgr).await;
    forms::put_draft(
        &w.app,
        mgr,
        form,
        Part::Rules,
        &DraftInput {
            base_version: 0,
            body: rules(vec![due_when_assigned()]),
        },
    )
    .await
    .unwrap();
    let mine = forms::impact(&w.app, w.alice_ctx, form, Part::Rules)
        .await
        .unwrap()
        .pending_changesets
        .unwrap();
    assert_eq!(mine.would_fail, 1);
    assert_eq!(mine.ids, vec![p.changeset_id]);
    let theirs = forms::impact(&w.app, mgr, form, Part::Rules)
        .await
        .unwrap()
        .pending_changesets
        .unwrap();
    assert_eq!(theirs.would_fail, 0);
    assert!(theirs.ids.is_empty());
}

/// A publish waits for a write that holds the form (step 6b): the write
/// validated under the old rules commits first, and the publish then sees
/// and marks the issue it left breaking the new rules.
#[tokio::test]
async fn g_publish_waits_for_write_in_flight() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_d, _token, agent) = agent(w).await;
    let (i, _) = mk(w, "racing").await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i, 0, json!({"assigned_to_id": w.bob})),
        ORIGIN,
    )
    .await
    .unwrap();
    let gate = Arc::new(Gate::default());
    faults::arm("apply.after_locks", Action::Block(gate.clone()));
    let approve = ApproveInput {
        version: 1,
        proposal_hash: p.proposal_hash.clone(),
        review_mode: None,
    };
    let apply = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p.changeset_id,
        &approve,
    );
    let racer = async {
        gate.reached.notified().await;
        let pub_fut = publish(
            &f,
            Part::Rules,
            rules(vec![due_when_assigned()]),
            Some("accept_and_mark"),
        );
        tokio::pin!(pub_fut);
        let early = tokio::time::timeout(Duration::from_millis(700), &mut pub_fut).await;
        let finished_early = early.is_ok();
        gate.release.notify_one();
        let r = match early {
            Ok(r) => r,
            Err(_) => {
                common::wait_for(
                    "the publish after the release",
                    common::WAIT_TIMEOUT,
                    pub_fut,
                )
                .await
            }
        };
        (finished_early, r)
    };
    let (applied, (finished_early, published)) = common::wait_for(
        "the write holding the form and the racing publish",
        common::WAIT_TIMEOUT,
        async { tokio::join!(apply, racer) },
    )
    .await;
    faults::clear();
    applied.unwrap();
    assert!(
        !finished_early,
        "the publish did not wait for the write holding the form"
    );
    let published = published.unwrap();
    assert_eq!(published.marked, 1);
    assert_eq!(open_marks(w, i).await, 1);
}

/// A workflow change moves the rules version (its `w` part): describe says
/// so and a proposal made before is flagged.
#[tokio::test]
async fn g_workflow_change_moves_rules_version() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_d, _token, agent) = agent(w).await;
    let (i, _) = mk(w, "workflow").await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &upd(i, 0, json!({"subject": "renamed"})),
        ORIGIN,
    )
    .await
    .unwrap();
    let rv0 = describe(w, w.alice_ctx).await.rules_version;
    sqlx::query(
        "INSERT INTO workflow_field_rules (tracker_id, old_status_id, role_id, field_key, rule)
         VALUES ($1, $2, $3, 'description', 'readonly')",
    )
    .bind(w.bug)
    .bind(w.doing)
    .bind(w.dev)
    .execute(&pool)
    .await
    .unwrap();
    let rv1 = describe(w, w.alice_ctx).await.rules_version;
    assert_ne!(rv1, rv0);
    assert!(
        changesets::get(&w.app, w.alice_ctx, p.changeset_id, None)
            .await
            .unwrap()
            .rules_changed
    );
}

/// Publishing, rolling back, starting a conversion and deleting a field
/// all need a recent password (step-up) over HTTP; a stale session changes
/// nothing.
#[tokio::test]
async fn g_step_up_over_http() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let r = router(w);
    let alice = login(&r, "alice").await;
    let p1 = publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    let dv = draft(w, f.form, Part::Rules, rules(vec![])).await.unwrap();
    let code_f = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Code", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap();
    let conv = field_admin::preview_conversion(
        &w.app,
        w.alice_ctx,
        &code_f.key,
        &ConversionInput {
            to: field_input("CodeN", "int", vec![w.bug], &[]),
            mapping: json!({}),
        },
    )
    .await
    .unwrap();
    let tmp = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Tmp", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE browser_sessions SET reauthenticated_at = now() - interval '1 day' WHERE user_id = '{}'",
            w.alice
        ),
    )
    .await;
    let base = format!("/forms/{}", f.form);
    let calls: Vec<(&str, String, Option<Value>)> = vec![
        (
            "POST",
            format!("{base}/drafts/rules/publish"),
            Some(json!({"draft_version": dv, "mode": "block"})),
        ),
        (
            "POST",
            format!("{base}/rollback"),
            Some(json!({"part": "rules", "to_version": p1.version, "mode": "block"})),
        ),
        (
            "POST",
            format!("/admin/conversions/{}/start", conv.conversion_id),
            None,
        ),
        (
            "DELETE",
            format!(
                "/admin/custom-fields/{}?lock_version={}",
                tmp.id, tmp.lock_version
            ),
            None,
        ),
    ];
    for (m, path, body) in calls {
        let (st, v, _) = call(&r, Some(&alice), m, &path, body).await;
        assert_eq!(
            v["error"]["code"], "reauthentication_required",
            "{m} {path}: {st} {v}"
        );
    }
    let d = forms::get(&w.app, w.alice_ctx, f.form).await.unwrap();
    assert_eq!(d.summary.published_rules_version, p1.version);
    assert_eq!(d.summary.rules_draft, Some(dv));
    assert_eq!(
        field_admin::get_conversion(&w.app, w.alice_ctx, conv.conversion_id)
            .await
            .unwrap()
            .state,
        "previewed"
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM field_defs WHERE id = {} AND index_state <> 'dropping'",
                tmp.id
            )
        )
        .await,
        1
    );
    // With a fresh password the same publish goes through.
    exec(
        &pool,
        &format!(
            "UPDATE browser_sessions SET reauthenticated_at = now() WHERE user_id = '{}'",
            w.alice
        ),
    )
    .await;
    let (st, v, _) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("{base}/drafts/rules/publish"),
        Some(json!({"draft_version": dv, "mode": "block"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// A Triage accept and a document mixing log_time with a field change go
/// through the L1 rules like any other write.
#[tokio::test]
async fn g_triage_accept_and_log_time_mixed_checked() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let cur = oxledger::triage::settings(&w.app, w.alice_ctx, "WEB")
        .await
        .unwrap();
    oxledger::triage::put_settings(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        "WEB",
        &oxledger::triage::TriageSettingsPut {
            lock_version: cur.lock_version,
            enabled: true,
            entry_rule: "all_new".into(),
            responsible_user_id: Some(w.alice),
            decline_status_id: Some(w.done),
        },
    )
    .await
    .unwrap();
    human(
        w,
        w.alice_ctx,
        &doc_of(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "to triage"}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    let t: Uuid = sqlx::query_scalar("SELECT id FROM issues WHERE subject = 'to triage'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_triage WHERE issue_id = '{t}' AND state = 'pending'"
            )
        )
        .await,
        1
    );
    publish(
        &f,
        Part::Rules,
        rules(vec![due_when_assigned()]),
        Some("block"),
    )
    .await
    .unwrap();
    let accept = |set: Value, l: i32| {
        doc_of(
            vec![
                json!({"op_id": op(), "kind": "triage_decide", "target": tgt(t), "decision": "accept"}),
                set_op(t, set),
            ],
            vec![(t, l)],
        )
    };
    let e = human(
        w,
        w.alice_ctx,
        &accept(json!({"assigned_to_id": w.bob}), lv(w, t).await),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing, "{e:#}");
    assert_eq!(details(&e)["rule_id"], "due_when_assigned");
    human(
        w,
        w.alice_ctx,
        &accept(
            json!({"assigned_to_id": w.bob, "due_date": "2026-12-01"}),
            lv(w, t).await,
        ),
    )
    .await
    .unwrap();

    // log_time with a field change in one document.
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING;
         UPDATE org_settings SET timelog_max_hours_per_day = 0, timelog_accept_future_dates = true,
                timelog_require_issue = false, timelog_require_comments = false;
         UPDATE time_entry_activities SET active = true;",
    )
    .await;
    let (i, _) = mk(w, "logged").await;
    let mixed = |set: Value, l: i32| {
        doc_of(
            vec![
                json!({"op_id": op(), "kind": "log_time", "issue": tgt(i),
                        "fields": {"hours": "1", "spent_on": "2026-11-02"}}),
                set_op(i, set),
            ],
            vec![(i, l)],
        )
    };
    let e = human(
        w,
        w.alice_ctx,
        &mixed(json!({"assigned_to_id": w.bob}), lv(w, i).await),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing, "{e:#}");
    assert_eq!(details(&e)["rule_id"], "due_when_assigned");
    human(
        w,
        w.alice_ctx,
        &mixed(
            json!({"assigned_to_id": w.bob, "due_date": "2026-12-01"}),
            lv(w, i).await,
        ),
    )
    .await
    .unwrap();
}

/// Conversions: the old field is read-only while one runs; issues the new
/// field does not apply to are skipped and counted; a deactivated field
/// cannot be converted.
#[tokio::test]
async fn g_conversion_guards() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let code_f = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Code", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap();
    let (i1, _) = mk(w, "one").await;
    let (i2, _) = mk(w, "two").await;
    for (i, v) in [(i1, "1"), (i2, "2")] {
        set(
            w,
            w.alice_ctx,
            i,
            json!({ code_f.key.clone(): {"string": v} }),
        )
        .await
        .unwrap();
    }
    let preview = |name: &str, scope_other: bool| {
        let mut to = field_input(name, "int", vec![w.bug], &[]);
        if scope_other {
            to.is_for_all = Some(false);
            to.project_ids = Some(vec![w.auth_p]);
        }
        let key = code_f.key.clone();
        async move {
            field_admin::preview_conversion(
                &w.app,
                w.alice_ctx,
                &key,
                &ConversionInput {
                    to,
                    mapping: json!({}),
                },
            )
            .await
            .unwrap()
        }
    };
    // Frozen while running: the second batch waits at the gate.
    let c1 = preview("CodeN", false).await;
    field_admin::set_batch_size(1);
    let gate = Arc::new(Gate::default());
    faults::arm("cf_convert.batch", Action::Block(gate.clone()));
    field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, c1.conversion_id)
        .await
        .unwrap();
    // The second batch (bounded: the gate panics with the state).
    gate.reached.notified().await;
    let first = if i1 < i2 { i1 } else { i2 };
    let e = set(
        w,
        w.alice_ctx,
        first,
        json!({ code_f.key.clone(): {"string": "9"} }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");
    assert_eq!(kind(&e), "field_converting");
    set(
        w,
        w.alice_ctx,
        first,
        json!({"subject": "other fields are fine"}),
    )
    .await
    .unwrap();
    gate.release.notify_one();
    wait_state(w, c1.conversion_id, "succeeded").await;
    field_admin::set_batch_size(500);
    faults::clear();
    set(
        w,
        w.alice_ctx,
        first,
        json!({ code_f.key.clone(): {"string": "9"} }),
    )
    .await
    .unwrap();
    // The new field only applies to AUTH: WEB issues are skipped.
    let c2 = preview("CodeAuth", true).await;
    field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, c2.conversion_id)
        .await
        .unwrap();
    let v = wait_state(w, c2.conversion_id, "succeeded").await;
    let report = v.report.unwrap();
    assert_eq!(report["not_applicable"], 2, "{report}");
    assert_eq!(report["converted"], 0, "{report}");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_cf WHERE field_id = {}",
                c2.to_field_id
            )
        )
        .await,
        0
    );
    // A deactivated source cannot start.
    let c3 = preview("CodeLate", false).await;
    let lv0 = field_admin::get(
        &w.app,
        w.alice_ctx,
        &code_f.key,
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    field_admin::patch(
        &w.app,
        w.alice_ctx,
        &code_f.id.to_string(),
        &FieldPatch {
            lock_version: lv0,
            set: FieldSet {
                deactivated: Some(true),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    let e = field_admin::start_conversion(&w.app, &w.policy, w.alice_ctx, c3.conversion_id)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict, "{e:#}");
    assert_eq!(kind(&e), "field_inactive");
}

/// Layouts: a field a workflow makes required in a status cannot be hidden
/// there by show_if; a section whose every field the reader cannot see is
/// left out with its title.
#[tokio::test]
async fn g_layout_workflow_required_and_hidden_sections() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    sqlx::query(
        "INSERT INTO workflow_field_rules (tracker_id, old_status_id, role_id, field_key, rule)
         VALUES ($1, $2, $3, 'due_date', 'required')",
    )
    .bind(w.bug)
    .bind(w.new)
    .bind(w.dev)
    .execute(&pool)
    .await
    .unwrap();
    let layout = |show: Value| {
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}]},
            {"id": "plan", "title": "Plan", "show_if": show, "fields": [{"key": "due_date"}]}
        ]}}})
    };
    let e = draft(
        w,
        f.form,
        Part::Layout,
        layout(json!({"all": [{"field": "status_category", "op": "eq", "value": "done"}]})),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::FormInvalid, "{e:#}");
    assert_eq!(kind(&e), "hides_required");
    draft(
        w,
        f.form,
        Part::Layout,
        layout(json!({"all": [{"field": "status", "op": "eq", "value": w.new}]})),
    )
    .await
    .unwrap();
    // A section of restricted fields only.
    let lead = lead_role(&pool).await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    publish(
        &f,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}, {"key": "due_date"}]},
            {"id": "classified", "title": "Classified matters", "fields": [{"key": secret.key}]}
        ]}}}),
        None,
    )
    .await
    .unwrap();
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let fd = describe(w, bob).await;
    assert!(
        fd.layout.sections.iter().all(|s| s.id != "classified"),
        "{:?}",
        fd.layout.sections.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
    assert!(
        !serde_json::to_string(&fd)
            .unwrap()
            .contains("Classified matters")
    );
    let fd = describe(w, w.alice_ctx).await;
    assert!(fd.layout.sections.iter().any(|s| s.id == "classified"));
}

/// M1.6 follow-up of D21: a project manager's layout is checked against
/// the fields they can read. Placing a field they cannot read, or naming
/// it in a show_if, is `unknown_field` — never `field_not_applicable` or
/// `hides_required` naming it — while an administrator may place it.
#[tokio::test]
async fn g_manager_layout_unreadable_field() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let lead = lead_role(&pool).await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    secret.is_required = Some(true);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    // Restricted and not for this tracker: no field_not_applicable either.
    let task = id(
        &pool,
        "INSERT INTO trackers (name, position, default_status_id) SELECT 'Task', 2, id FROM issue_statuses WHERE name = 'New' RETURNING id",
    )
    .await;
    let mut other = field_input("Other", "string", vec![task], &[]);
    other.visible = Some(false);
    other.role_ids = Some(vec![lead]);
    let other = field_admin::create(&w.app, w.alice_ctx, &other)
        .await
        .unwrap();
    let form = v_project_form(&f, mgr).await;
    let put = |who: AuthContext, body: Value| {
        let app = w.app.clone();
        async move {
            forms::put_draft(
                &app,
                who,
                form,
                Part::Layout,
                &DraftInput {
                    base_version: 0,
                    body,
                },
            )
            .await
        }
    };
    let placed = |key: &str, show: Option<Value>| {
        let mut field = json!({"key": key});
        if let Some(s) = show {
            field["show_if"] = s;
        }
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}, field]}
        ]}}})
    };
    let hidden = json!({"all": [{"field": "status_category", "op": "eq", "value": "done"}]});
    for (what, body) in [
        ("placed", placed(&secret.key, None)),
        (
            "placed under show_if",
            placed(&secret.key, Some(hidden.clone())),
        ),
        ("not applicable", placed(&other.key, None)),
        (
            "named in show_if",
            placed(
                "description",
                Some(json!({"all": [{"field": secret.key.clone(), "op": "is_set"}]})),
            ),
        ),
    ] {
        let e = put(mgr, body).await.unwrap_err();
        let d = details(&e);
        assert_eq!(d["kind"], "unknown_field", "{what}: {d}");
    }
    // Readable fields are fine for the manager.
    put(mgr, placed("description", Some(hidden.clone())))
        .await
        .unwrap();
    // The administrator's checks are unchanged: the required restricted
    // field under a show_if is hides_required, plainly placed it is fine.
    let base = forms::get(&w.app, w.alice_ctx, form)
        .await
        .unwrap()
        .layout
        .draft
        .map_or(0, |d| d.version);
    let e = forms::put_draft(
        &w.app,
        w.alice_ctx,
        form,
        Part::Layout,
        &DraftInput {
            base_version: base,
            body: placed(&secret.key, Some(hidden)),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "hides_required");
    forms::put_draft(
        &w.app,
        w.alice_ctx,
        form,
        Part::Layout,
        &DraftInput {
            base_version: base,
            body: placed(&secret.key, None),
        },
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------- independent verification (M1.6-D13/D14)

/// F1 (M1.6-D13 left a path out): `POST /forms/{id}/drafts/layout/impact`
/// builds `layout_warnings` from the whole catalog, so a project manager
/// gets `{"kind": "unplaced", "field": "cf:N"}` for a field they cannot
/// read (it applies to the tracker, is active, and is not placed).
/// Expected: only fields in the author's catalog are named.
#[tokio::test]
async fn verify_m16_manager_layout_impact_names_unreadable_field() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let lead = lead_role(&pool).await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    let form = v_project_form(&f, mgr).await;
    forms::put_draft(
        &w.app,
        mgr,
        form,
        Part::Layout,
        &DraftInput {
            base_version: 0,
            body: json!({"modes": {"create": {"sections": [
                {"id": "main", "title": "Main", "fields": [{"key": "subject"}, {"key": "description"}]}
            ]}}}),
        },
    )
    .await
    .unwrap();
    let imp = forms::impact(&w.app, mgr, form, Part::Layout)
        .await
        .unwrap();
    let s = serde_json::to_string(&imp.layout_warnings).unwrap();
    assert!(
        !s.contains(&format!("\"{}\"", secret.key)),
        "the manager's impact names {}: {s}",
        secret.key
    );
}

/// F2 (M1.6-D14 in practice): an administrator places a restricted field
/// in the WEB layout and publishes it, then deactivates the field. Every
/// later rule draft of that form fails the re-check of the published
/// layout with `unknown_field` naming the field — for the project manager
/// too (who cannot read it), and whatever the rule is about.
/// Expected: the manager can save an unrelated rule; no error names the
/// field.
#[tokio::test]
async fn verify_m16_manager_rules_blocked_by_deactivated_layout_field() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let lead = lead_role(&pool).await;
    let mut secret = field_input("Secret", "string", vec![w.bug], &[]);
    secret.visible = Some(false);
    secret.role_ids = Some(vec![lead]);
    let secret = field_admin::create(&w.app, w.alice_ctx, &secret)
        .await
        .unwrap();
    let form = v_project_form(&f, mgr).await;
    publish_form(
        w,
        form,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}, {"key": secret.key}]}
        ]}}}),
        None,
    )
    .await
    .unwrap();
    let lv = field_admin::get(
        &w.app,
        w.alice_ctx,
        &secret.key,
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    field_admin::patch(
        &w.app,
        w.alice_ctx,
        &secret.key,
        &FieldPatch {
            lock_version: lv,
            set: FieldSet {
                deactivated: Some(true),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    let r = forms::put_draft(
        &w.app,
        mgr,
        form,
        Part::Rules,
        &DraftInput {
            base_version: 0,
            body: rules(vec![
                json!({"id": "r1", "field": "due_date", "kind": "required_if",
                       "when": {"all": [{"field": "assignee", "op": "is_set"}]}}),
            ]),
        },
    )
    .await;
    if let Err(e) = r {
        panic!(
            "manager's unrelated rule refused: {e:#} {} (names {}: {})",
            details(&e),
            secret.key,
            format!("{e:#} {}", details(&e)).contains(&secret.key)
        );
    }
}

/// M1.6-D14: a rule change re-checks published layouts only for what it
/// breaks: a rule that makes a field required where a published layout
/// hides it is `hides_required` naming the (readable) field; an unrelated
/// rule passes even though the layout also places a deactivated field.
#[tokio::test]
async fn g_rules_recheck_only_new_hides_required() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mgr = v_manager(&f).await;
    let form = v_project_form(&f, mgr).await;
    let old = field_admin::create(
        &w.app,
        w.alice_ctx,
        &field_input("Old", "string", vec![w.bug], &[]),
    )
    .await
    .unwrap();
    publish_form(
        w,
        form,
        Part::Layout,
        json!({"modes": {"create": {"sections": [
            {"id": "main", "title": "Main", "fields": [{"key": "subject"}, {"key": "assigned_to_id"}, {"key": old.key}]},
            {"id": "plan", "title": "Plan",
             "show_if": {"all": [{"field": "status_category", "op": "eq", "value": "done"}]},
             "fields": [{"key": "due_date"}]}
        ]}}}),
        None,
    )
    .await
    .unwrap();
    let lv = field_admin::get(
        &w.app,
        w.alice_ctx,
        &old.key,
        &FieldQuery {
            preview_required: None,
        },
    )
    .await
    .unwrap()
    .field
    .lock_version;
    field_admin::patch(
        &w.app,
        w.alice_ctx,
        &old.key,
        &FieldPatch {
            lock_version: lv,
            set: FieldSet {
                deactivated: Some(true),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    let draft = |body: Value| {
        let app = w.app.clone();
        async move {
            forms::put_draft(
                &app,
                mgr,
                form,
                Part::Rules,
                &DraftInput {
                    base_version: 0,
                    body,
                },
            )
            .await
        }
    };
    let e = draft(rules(vec![due_when_assigned()])).await.unwrap_err();
    let d = details(&e);
    assert_eq!(d["kind"], "hides_required", "{d}");
    assert_eq!(d["field"], "due_date", "{d}");
    draft(rules(vec![
        json!({"id": "r1", "field": "description", "kind": "required_if",
        "when": {"all": [{"field": "assignee", "op": "is_set"}]}}),
    ]))
    .await
    .unwrap();
}

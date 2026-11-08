// SPDX-License-Identifier: GPL-2.0-or-later
//! Custom fields on PostgreSQL (decision D14).
//!
//! Loads a world with every field type, fields restricted by tracker, by
//! project and by role (`visible = false`), and one rare field (3 issues).
//! Filters are generated as typed trees and sent as the JSON a client would
//! send; the server path (`filter::resolve` → `push_pred` AND `push_filter`)
//! must return exactly what an independent reference says: the test computes
//! each field's gate E (applicability + readability, Redmine semantics) itself
//! and evaluates with `Filter::eval`.
//!
//! Also covers: the per-field partial index under a generic plan,
//! `service::search` (JSON filter, paging, free text with candidate growth),
//! `service::get` custom fields, input errors, the database guards (type,
//! empty string, option deletion, type change), dropping a field
//! and the index reconcile job.
//!
//! Needs PostgreSQL 18; see `common::test_pool` for which database it uses.

mod common;

use chrono::NaiveDate;
use common::test_pool;
use ox_core::filter::{Cmp, Field, FieldType, Filter, Test, Value};
use ox_core::{IssueFacts, Pred, Scope};
use oxledger::custom_fields::{self, NewField, set_value};
use oxledger::db::{self, Caller, push_pred};
use oxledger::filter::{self as cf_filter, push_filter};
use oxledger::search::SearchIndex;
use oxledger::service::{self, App, SearchParams};
use serde_json::{Value as Json, json};
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

const ISSUES: usize = 600;
const RANDOM_FILTERS: usize = 120;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }
}

/// A field as the test sees it: definition + the facts the reference needs.
#[derive(Clone)]
struct Def {
    f: Field,
    name: &'static str,
    trackers: Vec<Uuid>,
    /// None = all projects
    projects: Option<Vec<Uuid>>,
    /// None = visible to all; Some(viewer → projects) = readable by that
    /// viewer (None = anonymous) only there; admins read everything.
    readable_by: Option<HashMap<Option<Uuid>, Vec<Uuid>>>,
}

struct World {
    issues: Vec<(Uuid, IssueFacts)>,
    values: HashMap<Uuid, HashMap<i32, Value>>,
    defs: Vec<Def>,
    seen: HashMap<i32, Vec<Value>>,
    /// option / principal id → the text a client sends
    ref_text: HashMap<Uuid, String>,
    admin: Uuid,
    member: Uuid,
    outsider: Uuid,
    bug: Uuid,
    task: Uuid,
    private_project: Uuid,
}

impl World {
    fn def(&self, name: &str) -> &Def {
        self.defs.iter().find(|d| d.name == name).unwrap()
    }
}

async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
                  outbox, outbox_cursors, issue_keys, issues, workflow_transitions, project_trackers,
                  member_roles, members, role_permission_trackers, role_permissions, roles, enabled_modules,
                  projects, api_keys, group_members, aliases, principals, trackers, issue_statuses CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    // Indexes of field ids from earlier runs are orphans now.
    custom_fields::reconcile_indexes(pool).await.unwrap();
}

async fn exec(pool: &PgPool, sql: &'static str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

async fn scalar(pool: &PgPool, sql: &'static str) -> Uuid {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn new_field(
    pool: &PgPool,
    name: &'static str,
    ty: FieldType,
    trackers: &[Uuid],
    projects: Option<&[Uuid]>,
    roles: Option<&[Uuid]>,
    options: &[&str],
    readable_by: Option<HashMap<Option<Uuid>, Vec<Uuid>>>,
) -> Def {
    let id = custom_fields::create_field(
        pool,
        NewField {
            name,
            ty,
            description: "",
            is_required: false,
            is_for_all: projects.is_none(),
            trackers,
            projects: projects.unwrap_or(&[]),
            options,
            visible: roles.is_none(),
            roles: roles.unwrap_or(&[]),
        },
    )
    .await
    .unwrap();
    Def {
        f: Field { id, ty },
        name,
        trackers: trackers.to_vec(),
        projects: projects.map(<[Uuid]>::to_vec),
        readable_by,
    }
}

async fn load_world(pool: &PgPool, rng: &mut Rng) -> World {
    reset(pool).await;
    exec(
        pool,
        "INSERT INTO issue_statuses (name, category) VALUES ('New','todo')",
    )
    .await;
    exec(pool, "INSERT INTO trackers (name) VALUES ('Bug'), ('Task')").await;
    exec(
        pool,
        "INSERT INTO principals (kind, login, name, is_admin) VALUES
           ('user','admin','Admin',true), ('user','member','Mia Member',false), ('user','outsider','Oscar Out',false)",
    )
    .await;
    exec(
        pool,
        "INSERT INTO projects (key, name, is_public) VALUES ('PRIV','Private',false), ('PUB','Public',true)",
    )
    .await;
    exec(
        pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
    )
    .await;
    exec(
        pool,
        "INSERT INTO roles (name, issues_visibility, builtin) VALUES
           ('Dev','all',NULL), ('Non member','default','non_member'), ('Anonymous','default','anonymous')",
    )
    .await;
    exec(
        pool,
        "INSERT INTO role_permissions (role_id, permission) SELECT id, 'view_issues' FROM roles",
    )
    .await;
    exec(
        pool,
        "INSERT INTO members (project_id, principal_id)
         SELECT p.id, u.id FROM projects p, principals u WHERE p.key = 'PRIV' AND u.login = 'member'",
    )
    .await;
    exec(
        pool,
        "INSERT INTO member_roles SELECT m.id, r.id FROM members m, roles r WHERE r.name = 'Dev'",
    )
    .await;

    let status = scalar(pool, "SELECT id FROM issue_statuses").await;
    let bug = scalar(pool, "SELECT id FROM trackers WHERE name = 'Bug'").await;
    let task = scalar(pool, "SELECT id FROM trackers WHERE name = 'Task'").await;
    let private_project = scalar(pool, "SELECT id FROM projects WHERE key = 'PRIV'").await;
    let public_project = scalar(pool, "SELECT id FROM projects WHERE key = 'PUB'").await;
    let dev = scalar(pool, "SELECT id FROM roles WHERE name = 'Dev'").await;
    let non_member = scalar(pool, "SELECT id FROM roles WHERE builtin = 'non_member'").await;
    let anonymous = scalar(pool, "SELECT id FROM roles WHERE builtin = 'anonymous'").await;
    let admin = scalar(pool, "SELECT id FROM principals WHERE login = 'admin'").await;
    let member = scalar(pool, "SELECT id FROM principals WHERE login = 'member'").await;
    let outsider = scalar(pool, "SELECT id FROM principals WHERE login = 'outsider'").await;
    let users = [admin, member, outsider];
    let both = [bug, task];
    let projects = [private_project, public_project];

    use FieldType as T;
    let defs = vec![
        new_field(pool, "Estimate", T::Float, &both, None, None, &[], None).await,
        new_field(pool, "Points", T::Int, &[task], None, None, &[], None).await,
        new_field(
            pool,
            "Due",
            T::Date,
            &both,
            Some(&[private_project]),
            None,
            &[],
            None,
        )
        .await,
        new_field(
            pool,
            "Severity",
            T::List,
            &[bug],
            None,
            None,
            &["Low", "Normal", "High", "Critical"],
            None,
        )
        .await,
        new_field(pool, "Reviewer", T::User, &both, None, None, &[], None).await,
        new_field(pool, "Regression", T::Bool, &[bug], None, None, &[], None).await,
        new_field(pool, "Customer", T::String, &both, None, None, &[], None).await,
        new_field(pool, "Note", T::Text, &both, None, None, &[], None).await,
        new_field(pool, "Rare score", T::Float, &both, None, None, &[], None).await,
        // Readable only with role Dev, which the member holds in PRIV.
        new_field(
            pool,
            "Secret",
            T::Int,
            &both,
            None,
            Some(&[dev]),
            &[],
            Some(HashMap::from([(Some(member), vec![private_project])])),
        )
        .await,
        // Readable with the builtin roles only: Non member (member and
        // outsider in PUB) and Anonymous (PUB). The member's Dev role in PRIV
        // does not grant it.
        new_field(
            pool,
            "Triage",
            T::Int,
            &both,
            None,
            Some(&[non_member, anonymous]),
            &[],
            Some(HashMap::from([
                (Some(member), vec![public_project]),
                (Some(outsider), vec![public_project]),
                (None, vec![public_project]),
            ])),
        )
        .await,
    ];
    let mut ref_text: HashMap<Uuid, String> = HashMap::new();
    let sev = defs[3].f;
    let options: Vec<(String, Uuid)> =
        sqlx::query_as("SELECT label, id FROM field_options WHERE field_id = $1 ORDER BY position")
            .bind(sev.id)
            .fetch_all(pool)
            .await
            .unwrap();
    for (label, id) in &options {
        ref_text.insert(*id, label.clone());
    }
    for (u, login) in users.iter().zip(["admin", "member", "outsider"]) {
        ref_text.insert(*u, login.into());
    }

    let day0 = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    let customers = ["台灣銀行", "Acme", "Globex", "Initech"];
    let mut issues = Vec::new();
    let mut values: HashMap<Uuid, HashMap<i32, Value>> = HashMap::new();
    for n in 0..ISSUES {
        let f = IssueFacts {
            project_id: projects[rng.below(2)],
            tracker_id: both[rng.below(2)],
            is_private: rng.chance(0.3),
            author_id: users[rng.below(3)],
            assigned_to_id: rng.chance(0.5).then(|| users[rng.below(3)]),
        };
        let (id, _) = oxledger::test_support::create_issue(
            pool,
            oxledger::test_support::NewIssue {
                project_id: f.project_id,
                tracker_id: f.tracker_id,
                status_id: status,
                subject: &format!("issue {n}"),
                description: "",
                author_id: f.author_id,
                assigned_to_id: f.assigned_to_id,
                is_private: f.is_private,
                parent_id: None,
            },
        )
        .await
        .unwrap();
        // Values are stored regardless of applicability: retained values of
        // fields that do not apply must not count (Redmine).
        let mut vals: HashMap<i32, Value> = HashMap::new();
        let mut put = |d: &Def, p: f64, v: Value, rng: &mut Rng| {
            if rng.chance(p) {
                vals.insert(d.f.id, v);
            }
        };
        let r = rng.below(41) as f64 * 0.5;
        put(&defs[0], 0.7, Value::Num(r), rng);
        let r = rng.below(13) as f64;
        put(&defs[1], 0.5, Value::Num(r), rng);
        let r = Value::Date(day0 + chrono::Days::new(rng.below(120) as u64));
        put(&defs[2], 0.6, r, rng);
        let r = Value::Ref(options[rng.below(4)].1);
        put(&defs[3], 0.8, r, rng);
        let r = Value::Ref(users[rng.below(3)]);
        put(&defs[4], 0.3, r, rng);
        let r = Value::Bool(rng.chance(0.5));
        put(&defs[5], 0.4, r, rng);
        let r = Value::Text(customers[rng.below(4)].into());
        put(&defs[6], 0.2, r, rng);
        put(&defs[7], 0.1, Value::Text(format!("long note {n}")), rng);
        let r = rng.below(5) as f64;
        put(&defs[9], 0.5, Value::Num(r), rng);
        let r = rng.below(4) as f64;
        put(&defs[10], 0.5, Value::Num(r), rng);
        if n % 200 == 37 {
            vals.insert(defs[8].f.id, Value::Num(n as f64 / 10.0));
        }
        for (fid, v) in &vals {
            set_value(pool, id, *fid, v).await.unwrap();
        }
        values.insert(id, vals);
        issues.push((id, f));
    }
    sqlx::query("ANALYZE issue_cf").execute(pool).await.unwrap();

    let mut seen: HashMap<i32, Vec<Value>> = HashMap::new();
    for vals in values.values() {
        for (fid, v) in vals {
            let e = seen.entry(*fid).or_default();
            if !e.contains(v) {
                e.push(v.clone());
            }
        }
    }
    World {
        issues,
        values,
        defs,
        seen,
        ref_text,
        admin,
        member,
        outsider,
        bug,
        task,
        private_project,
    }
}

// ---------------------------------------------------------------- reference

/// The test's own gate E for `def` and `viewer` (None = anonymous):
/// tracker ∩ project applicability, and readability (effective roles,
/// builtin included; the same rule for filtering and display).
/// `None` when the viewer may not read the field anywhere.
fn ref_gate(w: &World, def: &Def, viewer: Option<Uuid>) -> Option<Pred> {
    let projects = |ids: &[Uuid]| Pred::Or(ids.iter().map(|p| Pred::ProjectIs(*p)).collect());
    let readable = match (&def.readable_by, viewer) {
        (None, _) => Pred::True,
        (Some(_), Some(v)) if v == w.admin => Pred::True,
        (Some(by), v) => match by.get(&v) {
            Some(ps) if !ps.is_empty() => projects(ps),
            _ => return None,
        },
    };
    let project = def.projects.as_deref().map_or(Pred::True, projects);
    Some(Pred::And(vec![
        Pred::TrackerIn(def.trackers.clone()),
        project,
        readable,
    ]))
}

/// A filter as the test builds it: leaves name a field by index into `w.defs`.
#[derive(Clone, Debug)]
enum Spec {
    And(Vec<Spec>),
    Or(Vec<Spec>),
    Leaf(usize, Test),
}

impl Spec {
    /// Typed filter with the reference gates; None if a field is unreadable.
    fn typed(&self, w: &World, viewer: Option<Uuid>) -> Option<Filter> {
        Some(match self {
            Spec::And(xs) => Filter::And(
                xs.iter()
                    .map(|x| x.typed(w, viewer))
                    .collect::<Option<_>>()?,
            ),
            Spec::Or(xs) => Filter::Or(
                xs.iter()
                    .map(|x| x.typed(w, viewer))
                    .collect::<Option<_>>()?,
            ),
            Spec::Leaf(i, t) => {
                let d = &w.defs[*i];
                Filter::cond_where(d.f, t.clone(), ref_gate(w, d, viewer)?).unwrap()
            }
        })
    }

    fn json(&self, w: &World) -> Json {
        let val = |v: &Value| match v {
            Value::Num(n) => json!(n),
            Value::Date(d) => json!(d.to_string()),
            Value::Text(t) => json!(t),
            Value::Ref(id) => json!(w.ref_text[id]),
            Value::Bool(b) => json!(b),
        };
        match self {
            Spec::And(xs) => json!({"and": xs.iter().map(|x| x.json(w)).collect::<Vec<_>>()}),
            Spec::Or(xs) => json!({"or": xs.iter().map(|x| x.json(w)).collect::<Vec<_>>()}),
            Spec::Leaf(i, t) => {
                let field = w.defs[*i].name;
                match t {
                    Test::Cmp(op, v) => {
                        let op = match op {
                            Cmp::Eq => "eq",
                            Cmp::Ne => "ne",
                            Cmp::Lt => "lt",
                            Cmp::Le => "le",
                            Cmp::Gt => "gt",
                            Cmp::Ge => "ge",
                        };
                        json!({"field": field, "op": op, "value": val(v)})
                    }
                    Test::Range { min, max } => json!({"field": field, "op": "between",
                        "value": [min.as_ref().map(val), max.as_ref().map(val)]}),
                    Test::In(vs) => {
                        json!({"field": field, "op": "in", "value": vs.iter().map(val).collect::<Vec<_>>()})
                    }
                    Test::IsNull => json!({"field": field, "op": "is_null"}),
                    Test::NotNull => json!({"field": field, "op": "not_null"}),
                }
            }
        }
    }
}

fn expected(w: &World, scope: &Scope, f: &Filter) -> BTreeSet<Uuid> {
    w.issues
        .iter()
        .filter(|(id, facts)| {
            let vals = &w.values[id];
            scope.can_see(facts) && f.eval(facts, &|fid| vals.get(&fid))
        })
        .map(|(id, _)| *id)
        .collect()
}

/// The server path: JSON → resolve → visibility AND filter in SQL.
async fn server_ids(
    app: &App,
    viewer: Option<Uuid>,
    scope: &Scope,
    json: &Json,
) -> Result<BTreeSet<Uuid>, String> {
    let caller = Caller(viewer);
    let input = cf_filter::parse_input(json).map_err(|e| e.to_string())?;
    let f = cf_filter::resolve(app, caller, scope, &input)
        .await
        .map_err(|e| e.to_string())?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT i.id FROM issues i WHERE ");
    push_pred(&mut qb, "i", &scope.predicate());
    qb.push(" AND ");
    push_filter(&mut qb, "i", &f);
    Ok(qb
        .build_query_scalar()
        .fetch_all(&app.pool)
        .await
        .unwrap_or_else(|e| panic!("{e}\nSQL: {}", qb.sql().as_str()))
        .into_iter()
        .collect())
}

/// A random value for `d`: usually one that occurs, sometimes not.
fn some_value(rng: &mut Rng, w: &World, d: &Def) -> Value {
    let seen = &w.seen[&d.f.id];
    // Refs must be ones a client can name.
    if matches!(d.f.ty, FieldType::List | FieldType::User) || rng.chance(0.85) {
        return seen[rng.below(seen.len())].clone();
    }
    match d.f.ty {
        FieldType::Int => Value::Num(99.0),
        FieldType::Float => Value::Num(-1.25),
        FieldType::Date => Value::Date(NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()),
        FieldType::Bool => Value::Bool(true),
        _ => Value::Text("nobody".into()),
    }
}

fn random_leaf(rng: &mut Rng, w: &World) -> Spec {
    loop {
        let i = rng.below(w.defs.len());
        let d = &w.defs[i];
        let test = match rng.below(9) {
            0 => Test::IsNull,
            1 => Test::NotNull,
            2 => Test::In(
                (0..1 + rng.below(3))
                    .map(|_| some_value(rng, w, d))
                    .collect(),
            ),
            3 => Test::Range {
                min: rng.chance(0.7).then(|| some_value(rng, w, d)),
                max: rng.chance(0.7).then(|| some_value(rng, w, d)),
            },
            k => {
                let op =
                    [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge][k - 4 + rng.below(2)];
                Test::Cmp(op, some_value(rng, w, d))
            }
        };
        // Invalid combinations (range on a list, value test on text, …) are retried.
        if Filter::cond(d.f, test.clone()).is_ok() {
            return Spec::Leaf(i, test);
        }
    }
}

fn random_spec(rng: &mut Rng, w: &World, depth: usize) -> Spec {
    if depth >= 3 || rng.chance(0.4) {
        return random_leaf(rng, w);
    }
    let parts = (0..1 + rng.below(3))
        .map(|_| random_spec(rng, w, depth + 1))
        .collect();
    if rng.chance(0.5) {
        Spec::And(parts)
    } else {
        Spec::Or(parts)
    }
}

/// EXPLAIN (GENERIC_PLAN) with seq scans off: the plan a prepared statement falls back to.
async fn generic_plan(pool: &PgPool, sql: String) -> String {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();
    // Simple query protocol: the $n placeholders stay unbound, which only
    // EXPLAIN (GENERIC_PLAN) accepts (a prepared statement would demand values).
    let rows: Vec<String> =
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("EXPLAIN (GENERIC_PLAN) {sql}")))
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .iter()
            .map(|r| sqlx::Row::get::<String, _>(r, 0))
            .collect();
    sqlx::query("RESET enable_seqscan")
        .execute(&mut *conn)
        .await
        .unwrap();
    rows.join("\n")
}

async fn search_all(
    app: &App,
    viewer: Uuid,
    q: Option<&str>,
    filter: &Json,
) -> (BTreeSet<Uuid>, bool) {
    let mut got = BTreeSet::new();
    let mut cursor = None;
    let mut truncated = false;
    loop {
        let page = service::search(
            app,
            Caller(Some(viewer)),
            SearchParams {
                q: q.map(str::to_string),
                filter: Some(filter.clone()),
                limit: Some(100),
                cursor,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        truncated |= page.truncated;
        assert!(page.items.len() <= 100);
        for i in &page.items {
            assert!(got.insert(i.id), "issue returned twice across pages");
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return (got, truncated),
        }
    }
}

async fn exists(pool: &PgPool, name: String) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_field_filters_match_reference_on_pg() {
    let pool = test_pool().await;
    let mut rng = Rng(0x00D1_4C0F_FEE5_EED5);
    let w = load_world(&pool, &mut rng).await;
    let index = SearchIndex::open(None).unwrap();
    index.sync(&pool).await.unwrap();
    let app = App {
        pool: pool.clone(),
        index,
    };

    let viewers = [None, Some(w.admin), Some(w.member), Some(w.outsider)];
    let mut scopes = Vec::new();
    for v in viewers {
        scopes.push(
            db::load_scope(&mut pool.acquire().await.unwrap(), Caller(v))
                .await
                .unwrap(),
        );
    }
    let idx = |name: &str| w.defs.iter().position(|d| d.name == name).unwrap();
    let num = |x: f64| Value::Num(x);

    // Compares the server with the reference for every viewer; returns the
    // admin's expected set.
    let check = async |name: &str, spec: &Spec| -> BTreeSet<Uuid> {
        let json = spec.json(&w);
        let mut admin_set = BTreeSet::new();
        for (viewer, scope) in viewers.iter().zip(&scopes) {
            let got = server_ids(&app, *viewer, scope, &json).await;
            match spec.typed(&w, *viewer) {
                Some(f) => {
                    let want = expected(&w, scope, &f);
                    assert_eq!(got.as_ref(), Ok(&want), "{name}, viewer {viewer:?}\n{json}");
                    if *viewer == Some(w.admin) {
                        admin_set = want;
                    }
                }
                None => {
                    let e = got.expect_err("unreadable field must be rejected");
                    assert!(e.contains("no custom field"), "{name}: {e}");
                }
            }
        }
        admin_set
    };

    // --- hand-written: ranges, the rare field, applicability, readability
    let rare = idx("Rare score");
    let rare_n = |t| Spec::Leaf(rare, t);
    let range = |i, lo: f64, hi: f64| {
        Spec::Leaf(
            i,
            Test::Range {
                min: Some(num(lo)),
                max: Some(num(hi)),
            },
        )
    };
    assert_eq!(
        check("rare not null", &rare_n(Test::NotNull)).await.len(),
        3
    );
    assert_eq!(
        check("rare is null", &rare_n(Test::IsNull)).await.len(),
        ISSUES - 3
    );
    assert_eq!(
        check("rare 3..14", &range(rare, 3.0, 14.0)).await.len(),
        1,
        "3.7"
    );
    assert_eq!(
        check("rare = 23.7", &rare_n(Test::Cmp(Cmp::Eq, num(23.7))))
            .await
            .len(),
        1
    );
    let est = check("estimate 2..8", &range(idx("Estimate"), 2.0, 8.0))
        .await
        .len();
    assert!(est > 40 && est < ISSUES / 2, "estimate 2..8: {est}");
    let jan = |d: u32| Value::Date(NaiveDate::from_ymd_opt(2026, 1, d).unwrap());
    check(
        "due in January (PRIV only)",
        &Spec::Leaf(
            idx("Due"),
            Test::Range {
                min: Some(jan(1)),
                max: Some(jan(31)),
            },
        ),
    )
    .await;
    // Points applies to Task only, Severity to Bug only: retained values on
    // other trackers must not count; is_null needs the field to apply, ne does not.
    let points = idx("Points");
    let on_bug = |set: &BTreeSet<Uuid>| {
        set.iter()
            .filter(|id| {
                w.issues
                    .iter()
                    .any(|(i, f)| i == *id && f.tracker_id == w.bug)
            })
            .count()
    };
    let not_null = check("points not null", &Spec::Leaf(points, Test::NotNull)).await;
    assert_eq!(on_bug(&not_null), 0, "Points never counts on Bug");
    let is_null = check("points is null", &Spec::Leaf(points, Test::IsNull)).await;
    assert_eq!(on_bug(&is_null), 0, "is_null needs the field to apply");
    let ne = check(
        "points ne 3",
        &Spec::Leaf(points, Test::Cmp(Cmp::Ne, num(3.0))),
    )
    .await;
    assert!(
        on_bug(&ne) > 100,
        "ne includes issues where the field does not apply"
    );
    assert!(
        w.values
            .iter()
            .any(|(id, v)| v.contains_key(&w.defs[points].f.id)
                && w.issues
                    .iter()
                    .any(|(i, f)| i == id && f.tracker_id == w.bug)),
        "the world must contain retained Points values on Bug issues"
    );
    let _ = w.task;
    // Secret: the member reads it in PRIV only; outsider and anonymous get "no custom field".
    let secret = check("secret not null", &Spec::Leaf(idx("Secret"), Test::NotNull)).await;
    assert!(!secret.is_empty());
    let member_scope = &scopes[2];
    let member_secret = server_ids(
        &app,
        Some(w.member),
        member_scope,
        &Spec::Leaf(idx("Secret"), Test::NotNull).json(&w),
    )
    .await
    .unwrap();
    assert!(!member_secret.is_empty());
    assert!(member_secret.iter().all(|id| {
        w.issues
            .iter()
            .any(|(i, f)| i == id && f.project_id == w.private_project)
    }));

    // --- random nested filters, every viewer
    for n in 0..RANDOM_FILTERS {
        let spec = random_spec(&mut rng, &w, 0);
        check(&format!("random {n}"), &spec).await;
    }

    // --- the per-field partial index is usable, also under a generic plan
    let rare_f = w.defs[rare].f;
    let typed = Filter::cond(
        rare_f,
        Test::Range {
            min: Some(num(3.0)),
            max: Some(num(14.0)),
        },
    )
    .unwrap();
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT i.id FROM issues i WHERE ");
    push_filter(&mut qb, "i", &typed);
    let sql = qb.sql().as_str().to_string();
    let index = custom_fields::index_name(rare_f.id);
    let plan = generic_plan(&pool, sql.clone()).await;
    assert!(
        plan.contains(&index),
        "literal field id should use {index}:\n{plan}"
    );
    // Negative control for the playbook entry: with the field id as a
    // parameter, the generic plan cannot prove `field_id = <id>`.
    let param_sql = sql.replace(
        &format!("cf.field_id = {}", rare_f.id),
        "cf.field_id = $3::int",
    );
    let plan = generic_plan(&pool, param_sql).await;
    assert!(
        !plan.contains(&index),
        "parameterised field id unexpectedly used {index}:\n{plan}"
    );

    // --- service::search: JSON filter with paging, and free text + filter
    let spec = Spec::And(vec![
        Spec::Leaf(idx("Estimate"), Test::NotNull),
        Spec::Or(vec![
            Spec::Leaf(idx("Severity"), Test::IsNull),
            range(idx("Estimate"), 5.0, 20.0),
        ]),
    ]);
    let want = expected(&w, member_scope, &spec.typed(&w, Some(w.member)).unwrap());
    assert!(want.len() > 100, "needs more than one page: {}", want.len());
    let (got, truncated) = search_all(&app, w.member, None, &spec.json(&w)).await;
    assert_eq!(got, want, "service::search with filter");
    assert!(!truncated);
    // Every issue matches q = "issue". A selective filter leaves fewer than a
    // page among the first 500 candidates, so the candidate list must grow.
    let admin_scope = &scopes[1];
    let selective = Spec::Leaf(
        idx("Customer"),
        Test::Cmp(Cmp::Eq, Value::Text("Acme".into())),
    );
    let want = expected(
        &w,
        admin_scope,
        &selective.typed(&w, Some(w.admin)).unwrap(),
    );
    assert!(
        want.len() >= 20 && want.len() < 101,
        "selective: {}",
        want.len()
    );
    let first_round: BTreeSet<Uuid> = app
        .index
        .search("issue", &admin_scope.predicate(), 500)
        .unwrap()
        .into_iter()
        .map(|h| h.issue_id)
        .collect();
    assert!(
        !want.is_subset(&first_round),
        "some matches must rank beyond the first 500 candidates for this check to mean anything"
    );
    let (got, truncated) = search_all(&app, w.admin, Some("issue"), &selective.json(&w)).await;
    assert_eq!(
        got, want,
        "q + filter must find matches beyond the first 500 candidates"
    );
    assert!(!truncated);

    // --- get: applicable and readable fields only
    let get_fields = async |viewer: Uuid, issue: Uuid| -> Vec<String> {
        let key: String = sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1")
            .bind(issue)
            .fetch_one(&pool)
            .await
            .unwrap();
        service::get(&app, Caller(Some(viewer)), &key)
            .await
            .unwrap()
            .custom_fields
            .into_iter()
            .map(|c| c.name)
            .collect()
    };
    let priv_bug = w
        .issues
        .iter()
        .find(|(_, f)| f.project_id == w.private_project && f.tracker_id == w.bug)
        .unwrap()
        .0;
    let names = get_fields(w.member, priv_bug).await;
    assert!(names.contains(&"Secret".into()) && names.contains(&"Severity".into()));
    assert!(
        !names.contains(&"Triage".into()),
        "Dev in PRIV does not grant Triage: {names:?}"
    );
    assert!(names.contains(&"Due".into()), "Due applies in PRIV");
    assert!(
        !names.contains(&"Points".into()),
        "Points is Task-only: {names:?}"
    );
    let pub_bug = w
        .issues
        .iter()
        .find(|(_, f)| f.project_id != w.private_project && f.tracker_id == w.bug && !f.is_private)
        .unwrap()
        .0;
    let names = get_fields(w.outsider, pub_bug).await;
    assert!(!names.contains(&"Secret".into()), "{names:?}");
    assert!(
        names.contains(&"Triage".into()),
        "Non member reads Triage in PUB: {names:?}"
    );
    let names = get_fields(w.member, pub_bug).await;
    assert!(
        names.contains(&"Triage".into()) && !names.contains(&"Secret".into()),
        "the member is a non-member of PUB: {names:?}"
    );
    assert!(
        !names.contains(&"Due".into()),
        "Due is PRIV-only: {names:?}"
    );
    let names = get_fields(w.admin, pub_bug).await;
    assert!(
        names.contains(&"Secret".into()),
        "admins read everything: {names:?}"
    );

    // --- input errors come back as user errors with something to act on
    let user_error = async |v: Json| {
        let e = service::search(
            &app,
            Caller(Some(w.member)),
            SearchParams {
                filter: Some(v),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        let a =
            oxledger::error::as_api_error(&e).unwrap_or_else(|| panic!("not an API error: {e:#}"));
        assert_eq!(a.code, oxledger::error::ErrorCode::InvalidFilter, "{e:#}");
        a.message.clone()
    };
    let msg = user_error(json!({"field": "Sevrity", "op": "eq", "value": "High"})).await;
    assert!(
        msg.contains("no custom field") && msg.contains("Severity"),
        "{msg}"
    );
    let msg = user_error(json!({"field": "Severity", "op": "eq", "value": "Blocker"})).await;
    assert!(msg.contains("Low, Normal, High, Critical"), "{msg}");
    let msg = user_error(json!({"field": "Severity", "op": "lt", "value": "High"})).await;
    assert!(msg.contains("ordering"), "{msg}");
    let msg = user_error(json!({"field": "Note", "op": "eq", "value": "x"})).await;
    assert!(msg.contains("is_null"), "{msg}");
    let msg = user_error(json!({"field": "Points", "op": "eq", "value": 1.5})).await;
    assert!(msg.contains("not an integer"), "{msg}");
    let msg = user_error(json!({"field": "Due", "op": "between", "value": ["2026-01-01"]})).await;
    assert!(msg.contains("[min, max]"), "{msg}");
    let msg = user_error(json!({"field": "Due", "op": "eq", "value": "01/02/2026"})).await;
    assert!(msg.contains("YYYY-MM-DD"), "{msg}");

    // --- database guards
    let issue = w.issues[0].0;
    let sev = w.def("Severity").f;
    let bad_values = [
        (w.def("Estimate").f, Value::Text("2".into())),
        (w.def("Points").f, Value::Num(1.5)),
        (sev, Value::Ref(w.admin)), // not an option of the field
        (w.def("Reviewer").f, Value::Ref(Uuid::now_v7())), // not a principal
    ];
    for (field, bad) in bad_values {
        let err = set_value(&pool, issue, field.id, &bad).await;
        assert!(err.is_err(), "trigger accepted {bad:?} for field {field:?}");
    }
    assert!(
        set_value(&pool, issue, 999_999, &num(1.0)).await.is_err(),
        "unknown field"
    );
    let cust = w.def("Customer").f;
    set_value(&pool, issue, cust.id, &Value::Text("x".into()))
        .await
        .unwrap();
    set_value(&pool, issue, cust.id, &Value::Text(String::new()))
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM issue_cf WHERE issue_id = $1 AND field_id = $2")
            .bind(issue)
            .bind(cust.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "empty string clears the value");
    let raw_empty =
        sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_text) VALUES ($1, $2, '')")
            .bind(issue)
            .bind(cust.id)
            .execute(&pool)
            .await;
    assert!(raw_empty.is_err(), "'' must not be stored");
    let change_type = sqlx::query("UPDATE field_defs SET ftype = 'string' WHERE id = $1")
        .bind(sev.id)
        .execute(&pool)
        .await;
    assert!(change_type.is_err(), "type change must be refused");
    // Options: in use → refused; unused → deleted; REPEATABLE READ → refused.
    let used = w
        .values
        .values()
        .find_map(|v| match v.get(&sev.id) {
            Some(Value::Ref(o)) => Some(*o),
            _ => None,
        })
        .unwrap();
    let del = sqlx::query("DELETE FROM field_options WHERE id = $1")
        .bind(used)
        .execute(&pool)
        .await;
    assert!(del.unwrap_err().to_string().contains("in use"));
    let spare: Uuid = sqlx::query_scalar(
        "INSERT INTO field_options (field_id, label) VALUES ($1, 'Spare') RETURNING id",
    )
    .bind(sev.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .unwrap();
    let rr = sqlx::query("DELETE FROM field_options WHERE id = $1")
        .bind(spare)
        .execute(&mut *tx)
        .await;
    assert!(rr.unwrap_err().to_string().contains("READ COMMITTED"));
    tx.rollback().await.unwrap();
    let ok = sqlx::query("DELETE FROM field_options WHERE id = $1")
        .bind(spare)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(ok.rows_affected(), 1);

    // --- reconcile rebuilds a missing index and finishes an interrupted drop
    let due = w.def("Due").f;
    let est_f = w.def("Estimate").f;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP INDEX {}",
        custom_fields::index_name(due.id)
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE field_defs SET index_state = 'dropping' WHERE id = $1")
        .bind(est_f.id)
        .execute(&pool)
        .await
        .unwrap();
    // A field being dropped takes no new values.
    assert!(set_value(&pool, issue, est_f.id, &num(1.0)).await.is_err());
    assert_eq!(custom_fields::reconcile_indexes(&pool).await.unwrap(), 2);
    assert!(exists(&pool, custom_fields::index_name(due.id)).await);
    assert!(!exists(&pool, custom_fields::index_name(est_f.id)).await);
    let count = async |fid: i32| -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM issue_cf WHERE field_id = $1")
            .bind(fid)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(count(est_f.id).await, 0);

    // --- drop_field: values, index and definition go; the name stops resolving
    assert!(custom_fields::drop_field(&pool, rare_f.id).await.unwrap());
    assert!(!custom_fields::drop_field(&pool, rare_f.id).await.unwrap());
    assert!(!exists(&pool, custom_fields::index_name(rare_f.id)).await);
    assert_eq!(count(rare_f.id).await, 0);
    let msg = user_error(json!({"field": "Rare score", "op": "not_null"})).await;
    assert!(msg.contains("no custom field"), "{msg}");
    assert_eq!(custom_fields::reconcile_indexes(&pool).await.unwrap(), 0);
    let _ = w.outsider;
}

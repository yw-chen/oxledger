// SPDX-License-Identifier: GPL-2.0-or-later
//! Demo data for local development: `oxledger seed-demo`.
//! Prints freshly generated API keys; only their hashes are stored.

use crate::custom_fields::{self, NewField, set_value};
use crate::db::{self, NewIssue};
use anyhow::{Result, bail};
use ox_core::filter::{FieldType, Value};
use sqlx::PgPool;
use uuid::Uuid;

async fn id(pool: &PgPool, sql: &'static str, name: &str) -> Result<Uuid> {
    Ok(sqlx::query_scalar(sql).bind(name).fetch_one(pool).await?)
}

/// A demo custom field for `trackers`, in every project or only in `project`.
async fn field(
    pool: &PgPool,
    (name, ty): (&str, FieldType),
    trackers: &[Uuid],
    project: Option<Uuid>,
    options: &[&str],
) -> Result<i32> {
    custom_fields::create_field(
        pool,
        NewField {
            name,
            ty,
            description: "",
            is_required: false,
            is_for_all: project.is_none(),
            trackers,
            projects: project.as_slice(),
            options,
            visible: true,
            roles: &[],
        },
    )
    .await
}

pub async fn seed_demo(pool: &PgPool) -> Result<Vec<(String, String)>> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(pool)
        .await?;
    if n > 0 {
        bail!("database already has projects; seed-demo only runs on an empty database");
    }
    let mut tx = pool.begin().await?;
    let q = |sql: &'static str| sqlx::query(sql);
    q(
        "INSERT INTO issue_statuses (name, category, position) VALUES
        ('New','todo',1), ('In Progress','doing',2), ('Resolved','done',3), ('Closed','done',4)",
    )
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO trackers (name, position, default_status_id)
        SELECT v.name, v.pos, (SELECT id FROM issue_statuses WHERE name='New')
        FROM (VALUES ('Bug',1), ('Feature',2), ('Task',3)) AS v(name,pos)")
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO aliases (entity_kind, entity_id, label, lang)
        SELECT 'tracker', t.id, v.label, v.lang FROM trackers t JOIN (VALUES
          ('Bug','缺陷','zh-TW'), ('Bug','臭蟲','zh-TW'), ('Bug','defect','en'), ('Bug','錯誤','zh-TW'),
          ('Feature','功能','zh-TW'), ('Feature','需求','zh-TW'), ('Task','任務','zh-TW'), ('Task','工作','zh-TW')
        ) AS v(tracker,label,lang) ON v.tracker = t.name")
        .execute(&mut *tx).await?;
    q("INSERT INTO aliases (entity_kind, entity_id, label, lang)
        SELECT 'status', s.id, v.label, 'zh-TW' FROM issue_statuses s JOIN (VALUES
          ('New','新建'), ('In Progress','進行中'), ('Resolved','已解決'), ('Closed','已關閉')
        ) AS v(status,label) ON v.status = s.name")
    .execute(&mut *tx)
    .await?;
    q(
        "INSERT INTO roles (name, issues_visibility, position, builtin) VALUES
        ('Manager','all',1,NULL), ('Developer','default',2,NULL), ('Reporter','own',3,NULL),
        ('Non member','default',10,'non_member'), ('Anonymous','default',11,'anonymous')",
    )
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO role_permissions (role_id, permission) SELECT id, 'view_issues' FROM roles")
        .execute(&mut *tx)
        .await?;
    // Every role may move New → In Progress → Resolved; only Manager may close.
    q("INSERT INTO workflow_transitions (tracker_id, role_id, old_status_id, new_status_id)
        SELECT t.id, r.id, o.id, n.id FROM trackers t, roles r, issue_statuses o, issue_statuses n
        WHERE r.builtin IS NULL AND (
              (o.name='New' AND n.name='In Progress') OR (o.name='In Progress' AND n.name='Resolved')
           OR (o.name='Resolved' AND n.name='In Progress')
           OR (r.name='Manager' AND n.name='Closed' AND o.name <> 'Closed'))")
        .execute(&mut *tx).await?;
    q("INSERT INTO principals (kind, login, name, is_admin) VALUES
        ('user','admin','Admin',true), ('user','alice','Alice Lin',false),
        ('user','bob','Bob Chen',false), ('user','carol','Carol Wu',false)")
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO principals (kind, name) VALUES ('group','Backend')")
        .execute(&mut *tx)
        .await?;
    q(
        "INSERT INTO group_members (group_id, user_id)
        SELECT g.id, u.id FROM principals g, principals u WHERE g.name='Backend' AND u.login='bob'",
    )
    .execute(&mut *tx)
    .await?;
    q(
        "INSERT INTO projects (key, name, description, is_public) VALUES
        ('AUTH','登入系統','帳號、登入、SSO 與權限',false),
        ('WEB','官方網站','公開網站與文件',true)",
    )
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO aliases (entity_kind, entity_id, label, lang)
        SELECT 'project', id, 'login', 'en' FROM projects WHERE key='AUTH'")
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO enabled_modules (project_id, module) SELECT id, 'issue_tracking' FROM projects")
        .execute(&mut *tx)
        .await?;
    q("INSERT INTO project_trackers SELECT p.id, t.id FROM projects p, trackers t")
        .execute(&mut *tx)
        .await?;
    // alice: Manager on AUTH; Backend group (bob): Developer on AUTH; carol: Reporter on AUTH.
    q("INSERT INTO members (project_id, principal_id)
        SELECT p.id, u.id FROM projects p, principals u
        WHERE p.key='AUTH' AND (u.login IN ('alice','carol') OR u.name='Backend')")
    .execute(&mut *tx)
    .await?;
    q("INSERT INTO member_roles (member_id, role_id)
        SELECT m.id, r.id FROM members m JOIN principals u ON u.id = m.principal_id JOIN roles r ON r.name =
          CASE WHEN u.login='alice' THEN 'Manager' WHEN u.login='carol' THEN 'Reporter' ELSE 'Developer' END")
        .execute(&mut *tx).await?;
    tx.commit().await?;

    let auth = id(pool, "SELECT id FROM projects WHERE key=$1", "AUTH").await?;
    let web = id(pool, "SELECT id FROM projects WHERE key=$1", "WEB").await?;
    let bug = id(pool, "SELECT id FROM trackers WHERE name=$1", "Bug").await?;
    let feature = id(pool, "SELECT id FROM trackers WHERE name=$1", "Feature").await?;
    let task = id(pool, "SELECT id FROM trackers WHERE name=$1", "Task").await?;
    let new = id(pool, "SELECT id FROM issue_statuses WHERE name=$1", "New").await?;
    let doing = id(
        pool,
        "SELECT id FROM issue_statuses WHERE name=$1",
        "In Progress",
    )
    .await?;
    let resolved = id(
        pool,
        "SELECT id FROM issue_statuses WHERE name=$1",
        "Resolved",
    )
    .await?;
    let user = |login: &'static str| id(pool, "SELECT id FROM principals WHERE login=$1", login);
    let (alice, bob, carol) = (
        user("alice").await?,
        user("bob").await?,
        user("carol").await?,
    );
    let backend = id(pool, "SELECT id FROM principals WHERE name=$1", "Backend").await?;

    let issue = |project_id,
                 tracker_id,
                 status_id,
                 subject,
                 description,
                 author_id,
                 assigned_to_id,
                 is_private,
                 parent_id| {
        db::create_issue(
            pool,
            NewIssue {
                project_id,
                tracker_id,
                status_id,
                subject,
                description,
                author_id,
                assigned_to_id,
                is_private,
                parent_id,
            },
        )
    };
    let (sso, _) = issue(
        auth,
        feature,
        doing,
        "支援 Google Workspace SSO 登入",
        "用 OIDC 串接公司的 Google Workspace，取代密碼登入。",
        alice,
        Some(bob),
        false,
        None,
    )
    .await?;
    let (oidc, _) = issue(
        auth,
        task,
        new,
        "設定 OIDC client 與 redirect URI",
        "在 Google Cloud Console 建立 OAuth client。",
        alice,
        Some(bob),
        false,
        Some(sso),
    )
    .await?;
    let (logout, _) = issue(
        auth,
        bug,
        new,
        "登入後偶爾被登出",
        "Session cookie 在 Safari 上似乎 24 小時內就失效，疑似 SameSite 設定。",
        carol,
        Some(backend),
        false,
        None,
    )
    .await?;
    let (smtp, _) = issue(
        auth,
        bug,
        doing,
        "密碼重設信寄送失敗",
        "SMTP 逾時，使用者收不到重設密碼的信。",
        carol,
        Some(alice),
        false,
        None,
    )
    .await?;
    let (brute, _) = issue(
        auth,
        bug,
        new,
        "管理員帳號被暴力破解的跡象",
        "登入失敗紀錄異常，暫不公開。",
        alice,
        Some(alice),
        true,
        None,
    )
    .await?;
    let (lang, _) = issue(
        auth,
        bug,
        resolved,
        "Login page shows wrong language",
        "The sign-in page ignores the browser locale.",
        carol,
        None,
        false,
        None,
    )
    .await?;
    let (img, _) = issue(
        web,
        bug,
        new,
        "首頁圖片載入太慢",
        "首頁的主視覺圖片有 4 MB。",
        alice,
        None,
        false,
        None,
    )
    .await?;
    let (docsearch, _) = issue(
        web,
        feature,
        new,
        "文件站加入搜尋功能",
        "讓訪客可以搜尋說明文件。",
        carol,
        None,
        false,
        None,
    )
    .await?;

    // Custom fields (D14), covering every indexed value kind plus long text.
    let all = [bug, feature, task];
    let severity = field(
        pool,
        ("Severity", FieldType::List),
        &[bug],
        None,
        &["Low", "Normal", "High", "Critical"],
    )
    .await?;
    let estimate = field(pool, ("Estimate (h)", FieldType::Float), &all, None, &[]).await?;
    let target = field(
        pool,
        ("Target date", FieldType::Date),
        &[feature, task],
        None,
        &[],
    )
    .await?;
    // Customer names are restricted: only Managers (alice on AUTH) and admins see them.
    let manager = id(pool, "SELECT id FROM roles WHERE name=$1", "Manager").await?;
    let customer = field(pool, ("Customer", FieldType::String), &all, Some(auth), &[]).await?;
    sqlx::query("UPDATE field_defs SET visible = false WHERE id = $1")
        .bind(customer)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO field_roles (field_id, role_id) VALUES ($1, $2)")
        .bind(customer)
        .bind(manager)
        .execute(pool)
        .await?;
    let reviewer = field(pool, ("Reviewer", FieldType::User), &all, None, &[]).await?;
    let regression = field(pool, ("Regression", FieldType::Bool), &[bug], None, &[]).await?;
    let root_cause = field(pool, ("Root cause", FieldType::Text), &[bug], None, &[]).await?;

    let opt = |label| async move {
        custom_fields::option_id(pool, severity, label)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no option {label}"))
    };
    let date = |s: &str| Value::Date(s.parse().expect("valid date"));
    let values = [
        (sso, estimate, Value::Num(16.0)),
        (sso, target, date("2026-12-15")),
        (sso, customer, Value::Text("台灣銀行".into())),
        (sso, reviewer, Value::Ref(alice)),
        (oidc, estimate, Value::Num(2.5)),
        (oidc, target, date("2026-11-30")),
        (logout, severity, Value::Ref(opt("High").await?)),
        (logout, estimate, Value::Num(4.0)),
        (logout, regression, Value::Bool(true)),
        (logout, customer, Value::Text("台灣銀行".into())),
        (
            logout,
            root_cause,
            Value::Text("SameSite=Strict 讓 Safari 在跨站導回時不送 cookie。".into()),
        ),
        (smtp, severity, Value::Ref(opt("Critical").await?)),
        (smtp, estimate, Value::Num(1.0)),
        (smtp, reviewer, Value::Ref(bob)),
        (brute, severity, Value::Ref(opt("Critical").await?)),
        (lang, severity, Value::Ref(opt("Low").await?)),
        (lang, regression, Value::Bool(false)),
        (img, severity, Value::Ref(opt("Normal").await?)),
        (img, estimate, Value::Num(0.5)),
        (docsearch, estimate, Value::Num(24.0)),
        (docsearch, target, date("2027-01-31")),
        (docsearch, reviewer, Value::Ref(carol)),
    ];
    for (issue_id, field_id, value) in &values {
        set_value(pool, *issue_id, *field_id, value).await?;
    }

    let mut keys = Vec::new();
    for login in ["admin", "alice", "bob", "carol"] {
        let raw = format!("oxl_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO api_keys (token_sha256, principal_id, label) SELECT $1, id, 'demo' FROM principals WHERE login = $2")
            .bind(db::hash_api_key(&raw))
            .bind(login)
            .execute(pool)
            .await?;
        keys.push((login.to_string(), raw));
    }
    Ok(keys)
}

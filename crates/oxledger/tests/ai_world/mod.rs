// SPDX-License-Identifier: GPL-2.0-or-later
//! Shared set-up of the M1.10 suites (design 05 §3.13, §7.10): a fake DNS,
//! the AI registration installed from fake environment variables, an
//! administrator, a provider, the AI switched on, and alice's built-in AI
//! with delegations. Built on the S6 world (WEB and AUTH).
#![allow(dead_code)]

use crate::world::*;
use oxledger::agents::{self, DelegationInput, DelegationView, PolicyInput};
use oxledger::ai::admin::{self, NewProvider, ProviderView, SettingsInput, SettingsView};
use oxledger::ai::chat::{self, AiStatus};
use oxledger::ai::config::{self, AiConfig, Limits};
use oxledger::ai::net::Resolver;
use oxledger::auth::{self, AuthContext};
use oxledger::policy::PolicyMode;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// The key the tests register; it must never show up anywhere.
pub const KEY: &str = "sk-test-KEY-4f1d9c-never-shown";

/// A resolver with scripted answers per host: each lookup takes the next
/// answer; the last one repeats. Counts lookups.
#[derive(Default)]
pub struct Dns {
    answers: Mutex<HashMap<String, VecDeque<Vec<IpAddr>>>>,
    pub lookups: Mutex<HashMap<String, usize>>,
}

impl Dns {
    pub fn set(&self, host: &str, answers: &[&[&str]]) {
        let q = answers
            .iter()
            .map(|a| a.iter().map(|s| s.parse().unwrap()).collect())
            .collect();
        self.answers.lock().unwrap().insert(host.into(), q);
    }

    pub fn count(&self, host: &str) -> usize {
        *self.lookups.lock().unwrap().get(host).unwrap_or(&0)
    }
}

impl Resolver for Dns {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move {
            *self.lookups.lock().unwrap().entry(host.into()).or_default() += 1;
            let mut m = self.answers.lock().unwrap();
            let q = m
                .get_mut(host)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host"))?;
            let a = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap_or_default()
            };
            Ok(a)
        })
    }
}

/// Installs a registration made from `vars` with `dns` and short limits.
pub fn install(vars: &[(&str, &str)], dns: Arc<Dns>) -> (AiConfig, Vec<String>) {
    install_with(vars, dns, |_| {})
}

pub fn install_with(
    vars: &[(&str, &str)],
    dns: Arc<Dns>,
    tweak: impl FnOnce(&mut Limits),
) -> (AiConfig, Vec<String>) {
    let (mut cfg, warnings) = config::from_vars(
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>(),
    );
    cfg.resolver = dns;
    // Short next to production (30 / 30 / 120 / 180 s), but with room for a
    // loaded machine: a run here normally ends within 2 s, and a fake
    // provider answers in milliseconds. 5 s to the first byte and 20 s per
    // run were close enough that a full parallel run could end a healthy run
    // as `provider_error` / `deadline` (T1b). The run limit stays below
    // `wait_run`'s bound (common::WAIT_TIMEOUT, 60 s), so a run always ends
    // before the test gives up on it. Tests of the limits set their own.
    cfg.limits.first_byte = Duration::from_secs(15);
    cfg.limits.chunk_gap = Duration::from_secs(15);
    cfg.limits.request = Duration::from_secs(30);
    cfg.limits.run = Duration::from_secs(45);
    cfg.limits.ping_every = Duration::from_millis(200);
    tweak(&mut cfg.limits);
    config::install(cfg.clone());
    oxledger::ai::runner::reset_breakers();
    (cfg, warnings)
}

/// Creates root (an administrator) and signs in. Also clears the AI rows
/// the world's reset does not reach (the monthly budget has no owner).
pub async fn admin_ctx(w: &World) -> AuthContext {
    sqlx::query("DELETE FROM ai_budget_months")
        .execute(&w.pool)
        .await
        .unwrap();
    auth::create_user(&w.pool, "root", "root", PW, true)
        .await
        .unwrap();
    sign_in(&w.pool, &w.policy, "root").await
}

pub fn new_provider(name: &str, kind: &str, endpoint: &str, model: Option<&str>) -> NewProvider {
    NewProvider {
        name: name.into(),
        kind: kind.into(),
        endpoint_name: endpoint.into(),
        model: model.map(String::from),
        max_output_tokens: None,
        input_usd_per_mtok: "3.00".into(),
        output_usd_per_mtok: "15.00".into(),
        d17_attested: true,
        org_account_attested: true,
    }
}

pub async fn provider(w: &World, root: AuthContext, p: &NewProvider) -> ProviderView {
    admin::create_provider(&w.app, &w.policy, root, p)
        .await
        .unwrap()
}

/// "Test connection" passed (for builds and tests that do not reach a
/// provider).
pub async fn mark_tested(w: &World, provider: Uuid, tools: bool) {
    sqlx::query(
        "UPDATE ai_providers SET last_test_at = now() + interval '1 second',
                last_test_ok_at = now() + interval '1 second', supports_tools = $2,
                last_test_result = '{\"ok\": true}'
         WHERE id = $1",
    )
    .bind(provider)
    .bind(tools)
    .execute(&w.pool)
    .await
    .unwrap();
}

pub async fn settings(w: &World, root: AuthContext) -> SettingsView {
    admin::settings(&w.app, root).await.unwrap()
}

pub async fn put(
    w: &World,
    root: AuthContext,
    f: impl FnOnce(&mut SettingsInput),
) -> anyhow::Result<SettingsView> {
    let s = settings(w, root).await;
    let mut i = SettingsInput {
        lock_version: s.lock_version,
        enabled: s.enabled,
        active_provider_id: s.active_provider_id,
        monthly_budget_usd: s.monthly_budget_usd,
        user_daily_budget_usd: s.user_daily_budget_usd,
        user_runs_per_hour: s.user_runs_per_hour,
        conversation_retention_days: s.conversation_retention_days,
    };
    f(&mut i);
    admin::put_settings(&w.app, &w.policy, root, &i).await
}

/// Turns the AI on with `provider` and a 100 USD month.
pub async fn enable(w: &World, root: AuthContext, provider: Uuid) -> SettingsView {
    put(w, root, |i| {
        i.enabled = true;
        i.active_provider_id = Some(provider);
        i.monthly_budget_usd = Some("100.00".into());
    })
    .await
    .unwrap()
}

pub async fn builtin_client(w: &World) -> Uuid {
    id(
        &w.pool,
        "SELECT id FROM agent_clients WHERE name = 'oxledger-ai' AND builtin",
    )
    .await
}

/// `who` sets up the built-in AI; returns its status.
pub async fn assistant(w: &World, who: AuthContext) -> AiStatus {
    chat::setup(&w.app, who).await.unwrap()
}

pub fn ops() -> Vec<String> {
    ["create_issue", "update_fields", "transition", "add_note"]
        .map(String::from)
        .to_vec()
}

pub async fn delegate_ai(
    w: &World,
    who: AuthContext,
    principal: Uuid,
    project: Uuid,
    commands: &[(&str, PolicyMode)],
    read_own_time: bool,
) -> anyhow::Result<DelegationView> {
    let mut proposal_ops = ops();
    if read_own_time {
        proposal_ops.push("log_time".into());
    }
    agents::create_delegation_for(
        &w.pool,
        &w.policy,
        &who,
        principal,
        DelegationInput {
            agent_client_id: builtin_client(w).await,
            project_id: project,
            proposal_ops,
            writable_fields: ["subject", "description"].map(String::from).to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: Some(PolicyInput {
                commands: commands
                    .iter()
                    .map(|(k, m)| (k.to_string(), *m))
                    .collect::<BTreeMap<_, _>>(),
                fields: BTreeMap::new(),
            }),
            read_own_time,
            read_own_time_ack: read_own_time,
        },
    )
    .await
}

/// A conversation of `who`.
pub async fn conversation(w: &World, who: AuthContext, title: &str) -> Uuid {
    chat::create(
        &w.app,
        who,
        &chat::NewConversation {
            title: Some(title.into()),
        },
    )
    .await
    .unwrap()
    .id
}

pub async fn send(
    w: &World,
    who: AuthContext,
    conversation: Uuid,
    text: &str,
    pages: Vec<chat::PageRef>,
    ack: Option<Uuid>,
) -> anyhow::Result<chat::Accepted> {
    chat::send(
        &w.app,
        who,
        Uuid::now_v7(),
        conversation,
        &chat::SendInput {
            text: text.into(),
            page_context: pages,
            ack_provider_id: ack,
        },
        ORIGIN,
    )
    .await
}

/// Waits until `run` ended (bounded by `common::WAIT_TIMEOUT`, above the
/// runs' own 45 s limit set in `install_with`); returns (state, stop_reason).
pub async fn wait_run(w: &World, run: Uuid) -> (String, String) {
    crate::common::poll_until(
        &format!("AI run {run} to end"),
        crate::common::WAIT_TIMEOUT,
        async || {
            let r: (String, Option<String>) =
                sqlx::query_as("SELECT state, stop_reason FROM ai_runs WHERE id = $1")
                    .bind(run)
                    .fetch_one(&w.pool)
                    .await
                    .unwrap();
            if matches!(r.0.as_str(), "queued" | "running") {
                crate::common::Continue(format!("state={}", r.0))
            } else {
                crate::common::Break((r.0, r.1.unwrap_or_default()))
            }
        },
    )
    .await
}

/// Everything readable that could hold a key: the whole AI admin API,
/// the provider events and test results, as text.
pub async fn admin_dump(w: &World, root: AuthContext) -> String {
    let mut s = String::new();
    s += &serde_json::to_string(&admin::settings(&w.app, root).await.unwrap()).unwrap();
    s += &serde_json::to_string(&admin::endpoints(&w.app, root).await.unwrap()).unwrap();
    s += &serde_json::to_string(&admin::providers(&w.app, root).await.unwrap()).unwrap();
    s += &serde_json::to_string(&admin::events(&w.app, root).await.unwrap()).unwrap();
    s += &serde_json::to_string(&admin::usage(&w.app, root, None).await.unwrap()).unwrap();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(t)::text FROM (SELECT * FROM ai_providers) t
         UNION ALL SELECT row_to_json(t)::text FROM (SELECT * FROM ai_provider_events) t
         UNION ALL SELECT row_to_json(t)::text FROM (SELECT * FROM ai_settings) t
         UNION ALL SELECT row_to_json(t)::text FROM (SELECT * FROM ai_messages) t
         UNION ALL SELECT row_to_json(t)::text FROM (SELECT * FROM ai_runs) t
         UNION ALL SELECT row_to_json(t)::text FROM (SELECT * FROM auth_events) t",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap_or_default();
    s += &rows.join("\n");
    s
}

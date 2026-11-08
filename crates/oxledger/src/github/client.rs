// SPDX-License-Identifier: GPL-2.0-or-later
//! What the worker needs from GitHub's REST API (S6b.7 uses it through
//! this trait only; the HTTP implementation is S6b.8). Every call names
//! the installation (its GitHub id) and the repository (`owner/name`).

use super::rollup::Review;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use std::future::Future;
use std::pin::Pin;

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// GitHub answered 403/429 with the rate limit spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimited {
    pub reset_at: DateTime<Utc>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub rate limit until {}", self.reset_at)
    }
}

impl std::error::Error for RateLimited {}

/// A webhook delivery as `GET /app/hook/deliveries` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDelivery {
    pub id: i64,
    pub guid: String,
    pub status_code: u16,
    pub delivered_at: DateTime<Utc>,
}

pub trait GithubApi: Send + Sync {
    /// `GET /repos/{repo}/pulls/{n}`; `None` = not found.
    fn get_pull<'a>(&'a self, inst: i64, repo: &'a str, number: i32) -> Fut<'a, Option<Json>>;
    /// `GET /repos/{repo}/pulls?state=…&sort=updated&direction=desc`, every
    /// page, newest first; stops before the first PR updated before `since`.
    fn list_pulls<'a>(
        &'a self,
        inst: i64,
        repo: &'a str,
        state: &'a str,
        since: Option<DateTime<Utc>>,
    ) -> Fut<'a, Vec<Json>>;
    /// Commit messages of the PR, at most 250.
    fn list_pull_commits<'a>(
        &'a self,
        inst: i64,
        repo: &'a str,
        number: i32,
    ) -> Fut<'a, Vec<String>>;
    /// Submitted reviews in order.
    fn list_reviews<'a>(&'a self, inst: i64, repo: &'a str, number: i32) -> Fut<'a, Vec<Review>>;
    /// `GET /repos/{repo}/check-suites/{id}`.
    fn get_check_suite<'a>(&'a self, inst: i64, repo: &'a str, id: i64) -> Fut<'a, Option<Json>>;
    /// `GET /repos/{repo}/check-runs/{id}`.
    fn get_check_run<'a>(&'a self, inst: i64, repo: &'a str, id: i64) -> Fut<'a, Option<Json>>;
    /// Suites, runs and statuses of a commit: `(suites, runs, statuses)`.
    fn checks_for_ref<'a>(
        &'a self,
        inst: i64,
        repo: &'a str,
        sha: &'a str,
    ) -> Fut<'a, (Vec<Json>, Vec<Json>, Vec<Json>)>;
    /// `GET /app/installations`: `(id, account login, suspended)`.
    fn list_installations(&self) -> Fut<'_, Vec<(i64, String, bool)>>;
    /// `GET /installation/repositories`: repository objects.
    fn list_installation_repos(&self, inst: i64) -> Fut<'_, Vec<Json>>;
    /// `GET /app/hook/deliveries` since a time.
    fn list_hook_deliveries(&self, since: DateTime<Utc>) -> Fut<'_, Vec<HookDelivery>>;
    /// `POST /app/hook/deliveries/{id}/attempts`.
    fn redeliver(&self, id: i64) -> Fut<'_, ()>;
}

/// The API when GitHub is not configured: every call fails.
pub struct NoGithub;

fn off<'a, T: Send + 'a>() -> Fut<'a, T> {
    Box::pin(async { Err(anyhow::anyhow!("GitHub is not configured")) })
}

impl GithubApi for NoGithub {
    fn get_pull<'a>(&'a self, _: i64, _: &'a str, _: i32) -> Fut<'a, Option<Json>> {
        off()
    }
    fn list_pulls<'a>(
        &'a self,
        _: i64,
        _: &'a str,
        _: &'a str,
        _: Option<DateTime<Utc>>,
    ) -> Fut<'a, Vec<Json>> {
        off()
    }
    fn list_pull_commits<'a>(&'a self, _: i64, _: &'a str, _: i32) -> Fut<'a, Vec<String>> {
        off()
    }
    fn list_reviews<'a>(&'a self, _: i64, _: &'a str, _: i32) -> Fut<'a, Vec<Review>> {
        off()
    }
    fn get_check_suite<'a>(&'a self, _: i64, _: &'a str, _: i64) -> Fut<'a, Option<Json>> {
        off()
    }
    fn get_check_run<'a>(&'a self, _: i64, _: &'a str, _: i64) -> Fut<'a, Option<Json>> {
        off()
    }
    fn checks_for_ref<'a>(
        &'a self,
        _: i64,
        _: &'a str,
        _: &'a str,
    ) -> Fut<'a, (Vec<Json>, Vec<Json>, Vec<Json>)> {
        off()
    }
    fn list_installations(&self) -> Fut<'_, Vec<(i64, String, bool)>> {
        off()
    }
    fn list_installation_repos(&self, _: i64) -> Fut<'_, Vec<Json>> {
        off()
    }
    fn list_hook_deliveries(&self, _: DateTime<Utc>) -> Fut<'_, Vec<HookDelivery>> {
        off()
    }
    fn redeliver(&self, _: i64) -> Fut<'_, ()> {
        off()
    }
}

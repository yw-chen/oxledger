// SPDX-License-Identifier: GPL-2.0-or-later
//! The built-in AI chat (design 05 §2.12, §3.13, §4.5, M1.10).
//!
//! * [`config`]: endpoints registered by the deployment (Q1), keys from the
//!   environment only, D17 lists, limits;
//! * [`net`]: the P-3 address checks and the resolver (pinning, rebinding);
//! * [`sse`], [`protocol`], [`client`] (and `http` with feature `ai`): one
//!   streamed model request for Anthropic or an OpenAI-compatible endpoint;
//! * [`admin`]: settings and providers (administrators);
//! * [`chat`]: the user's assistant, conversations, messages, re-checks,
//!   retention and D12;
//! * [`runner`]: runs, the tool loop, SSE to the browser, leases, cancel;
//! * [`tools`]: the shared tool registry as the model sees it;
//! * [`budget`]: rate, daily and monthly budgets, notifications.

pub mod admin;
pub mod budget;
pub mod chat;
pub mod client;
pub mod config;
#[cfg(feature = "ai")]
mod http;
pub mod net;
pub mod protocol;
pub mod runner;
pub mod sse;
pub mod tools;

/// The built-in system prompt's version (`ai_runs.prompt_version`, B-33).
pub const PROMPT_VERSION: i32 = 1;

/// The fixed lists 0030 checks (T-M1-124 style: a test compares them with
/// `pg_get_constraintdef`).
pub const RUN_STATES: [&str; 5] = ["queued", "running", "succeeded", "failed", "canceled"];
pub const STOP_REASONS: [&str; 10] = [
    "end_turn",
    "max_tokens",
    "tool_limit",
    "deadline",
    "budget_exhausted",
    "canceled",
    "provider_error",
    "context_unavailable",
    "ai_unavailable",
    "interrupted",
];
pub const PROVIDER_KINDS: [&str; 2] = ["anthropic", "openai_compat"];
pub const PROVIDER_EVENT_KINDS: [&str; 6] = [
    "created",
    "updated",
    "retired",
    "activated",
    "settings_updated",
    "test_run",
];
pub const MESSAGE_ROLES: [&str; 3] = ["user", "assistant", "tool"];
/// `changesets.origin` of agent proposals (0009 reserved `builtin_ai`).
pub const AGENT_ORIGINS: [&str; 2] = ["external_agent", "builtin_ai"];

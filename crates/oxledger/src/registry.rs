// SPDX-License-Identifier: GPL-2.0-or-later
//! The shared registry of application operations (contract 02 §4.6).
//!
//! REST and MCP are adapters over the same services; this table says which
//! operation exists, what it takes and returns (schemars schemas), its effect
//! class, where it is exposed and which credentials may call it. The OpenAPI
//! document is built from it, and tests check that MCP's tool list and the
//! REST router agree with it.
//!
//! MCP registers only `Read` and `Propose`. Not being registered is no
//! safety guarantee: approve/apply services check the session themselves.

use crate::auth::AuthMethod;
use crate::changesets::{
    AgentChangeset, AgentChangesetList, AgentOperation, AgentValidation, ProposeResult, StateView,
    SubmitResult,
};
use crate::citations::{
    GetSourceParams, GetSourceResult, VerifyCitationsParams, VerifyCitationsResult,
};
use crate::describe::{
    AssigneeList, DescribeParams, DescribeResult, IssueActions, IssueKeyPath, ProjectPath,
    ProjectTrackerPath, TrackerSchema,
};
use crate::issue_view::{GetManyParams, GetManyResult, GetParams, IssueView};
use crate::mcp::{
    ChangesetCancelParams, ChangesetGetParams, ChangesetListParams, ChangesetValidateParams,
    OperationGetParams, ProposeOutput, ProposeParams, ResolveParams, ReviseParams, SubmitParams,
    VersionListParams,
};
use crate::service::{IssueDetail, ResolveResult, SearchParams, SearchResult};
use crate::versions::VersionList;
use schemars::{JsonSchema, Schema, SchemaGenerator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    Read,
    Propose,
    Approve,
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPoints {
    Mcp,
    Rest,
    Both,
}

impl EntryPoints {
    pub fn mcp(self) -> bool {
        matches!(self, Self::Mcp | Self::Both)
    }
    pub fn rest(self) -> bool {
        matches!(self, Self::Rest | Self::Both)
    }
}

/// How an operation appears under `/api/v1`.
#[derive(Debug, Clone, Copy)]
pub struct RestRoute {
    pub method: &'static str,
    /// OpenAPI path template, e.g. `/issues/{key}`.
    pub path: &'static str,
    /// The input travels as the JSON body (else as path parameters).
    pub body: bool,
}

type SchemaFn = fn(&mut SchemaGenerator) -> Schema;

pub struct ToolDescriptor {
    /// MCP name (underscores, no dots).
    pub name: &'static str,
    pub contract_version: u16,
    pub summary: &'static str,
    pub input_schema: SchemaFn,
    pub output_schema: SchemaFn,
    pub effect: EffectClass,
    pub entrypoints: EntryPoints,
    pub allowed_methods: &'static [AuthMethod],
    pub rest: Option<RestRoute>,
}

fn schema<T: JsonSchema>(g: &mut SchemaGenerator) -> Schema {
    g.subschema_for::<T>()
}

/// Read tools: people through REST (session), agents and legacy keys through MCP.
const READERS: &[AuthMethod] = &[
    AuthMethod::BrowserSession,
    AuthMethod::AgentToken,
    AuthMethod::LegacyApiKey,
];

/// Agent-only MCP tools (A4: a legacy API key keeps only the four read
/// tools of §3.4: search, get, resolve, describe).
const AGENTS: &[AuthMethod] = &[AuthMethod::AgentToken];

fn agent_tool(
    name: &'static str,
    summary: &'static str,
    effect: EffectClass,
    input_schema: SchemaFn,
    output_schema: SchemaFn,
) -> ToolDescriptor {
    ToolDescriptor {
        name,
        contract_version: 1,
        summary,
        input_schema,
        output_schema,
        effect,
        entrypoints: EntryPoints::Mcp,
        allowed_methods: AGENTS,
        rest: None,
    }
}

/// Read tools added in S6: no legacy API keys (A4).
const SESSION_OR_AGENT: &[AuthMethod] = &[AuthMethod::BrowserSession, AuthMethod::AgentToken];

pub fn tools() -> Vec<ToolDescriptor> {
    let mut v = vec![
        ToolDescriptor {
            name: "search",
            contract_version: 2,
            summary: "Find issues the caller can see (filters, custom field filter, free text).",
            input_schema: schema::<SearchParams>,
            output_schema: schema::<SearchResult>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Both,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "POST",
                path: "/issues/search",
                body: true,
            }),
        },
        ToolDescriptor {
            name: "get",
            contract_version: 2,
            summary: "One issue by key, read from the database (not the index): selected fields, citations, and actions / recent notes / recent events (design 03 §3.6, contract 02 §8.8).",
            input_schema: schema::<GetParams>,
            output_schema: schema::<IssueView>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Mcp,
            allowed_methods: READERS,
            rest: None,
        },
        ToolDescriptor {
            name: "issue_get",
            contract_version: 1,
            summary: "One issue by key, read from the database (not the index).",
            input_schema: schema::<IssueKeyPath>,
            output_schema: schema::<IssueDetail>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Rest,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "GET",
                path: "/issues/{key}",
                body: false,
            }),
        },
        ToolDescriptor {
            name: "get_many",
            contract_version: 1,
            summary: "Up to 10 issues by key, each with the same parameters as get and its own result.",
            input_schema: schema::<GetManyParams>,
            output_schema: schema::<GetManyResult>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Mcp,
            allowed_methods: SESSION_OR_AGENT,
            rest: None,
        },
        ToolDescriptor {
            name: "verify_citations",
            contract_version: 1,
            summary: "Check up to 50 citation_id + quote pairs against the current readable text (design 03 §3.6).",
            input_schema: schema::<VerifyCitationsParams>,
            output_schema: schema::<VerifyCitationsResult>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Mcp,
            allowed_methods: SESSION_OR_AGENT,
            rest: None,
        },
        ToolDescriptor {
            name: "get_source",
            contract_version: 1,
            summary: "Context around a citation and a citation for exactly the quote (design 03 §3.6).",
            input_schema: schema::<GetSourceParams>,
            output_schema: schema::<GetSourceResult>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Mcp,
            allowed_methods: SESSION_OR_AGENT,
            rest: None,
        },
        ToolDescriptor {
            name: "resolve",
            contract_version: 1,
            summary: "Name, login, key or alias to ids the caller can see.",
            input_schema: schema::<ResolveParams>,
            output_schema: schema::<ResolveResult>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Both,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "POST",
                path: "/resolve",
                body: true,
            }),
        },
        ToolDescriptor {
            name: "describe",
            contract_version: 1,
            summary: "What an issue of a project × tracker has (scope project_tracker), or what the caller can do to one issue now (scope issue_actions). Not an authorization: writes re-check.",
            input_schema: schema::<DescribeParams>,
            output_schema: schema::<DescribeResult>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Mcp,
            allowed_methods: READERS,
            rest: None,
        },
        ToolDescriptor {
            name: "project_tracker_schema",
            contract_version: 1,
            summary: "Fields, ops and creation rules of an issue in this project × tracker (describe, static).",
            input_schema: schema::<ProjectTrackerPath>,
            output_schema: schema::<TrackerSchema>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Rest,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "GET",
                path: "/projects/{id}/trackers/{tracker_id}/schema",
                body: false,
            }),
        },
        ToolDescriptor {
            name: "issue_actions",
            contract_version: 1,
            summary: "What the caller can do to this issue now: editable fields, transitions, notes, duplicate (describe, dynamic).",
            input_schema: schema::<IssueKeyPath>,
            output_schema: schema::<IssueActions>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Rest,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "GET",
                path: "/issues/{key}/actions",
                body: false,
            }),
        },
        ToolDescriptor {
            name: "project_assignees",
            contract_version: 1,
            summary: "Users (and groups, when enabled) an issue in this project may be assigned to.",
            input_schema: schema::<ProjectPath>,
            output_schema: schema::<AssigneeList>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Rest,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "GET",
                path: "/projects/{id}/assignees",
                body: false,
            }),
        },
    ];
    v.extend([
        agent_tool(
            "version_list",
            "Milestones of a readable project, by name, and the next version (§17.9, §17.10).",
            EffectClass::Read,
            schema::<VersionListParams>,
            schema::<VersionList>,
        ),
        agent_tool(
            "command_submit",
            "Write directly where the delegation's policy allows, else create a proposal (§16.4).",
            EffectClass::Propose,
            schema::<SubmitParams>,
            schema::<SubmitResult>,
        ),
        agent_tool(
            "changeset_propose",
            "Propose a command document for the delegator to approve in the web app (§8.4).",
            EffectClass::Propose,
            schema::<ProposeParams>,
            schema::<ProposeOutput>,
        ),
        agent_tool(
            "changeset_revise",
            "A complete new version of the agent's proposal (§8.8).",
            EffectClass::Propose,
            schema::<ReviseParams>,
            schema::<ProposeResult>,
        ),
        agent_tool(
            "changeset_validate",
            "Re-run the checks of a proposal version (§8.5), filtered for the agent.",
            EffectClass::Read,
            schema::<ChangesetValidateParams>,
            schema::<AgentValidation>,
        ),
        agent_tool(
            "changeset_get",
            "One of the agent's proposals through the response filter (§8.8).",
            EffectClass::Read,
            schema::<ChangesetGetParams>,
            schema::<AgentChangeset>,
        ),
        agent_tool(
            "changeset_list",
            "The agent's proposals in its work context (§8.8).",
            EffectClass::Read,
            schema::<ChangesetListParams>,
            schema::<AgentChangesetList>,
        ),
        agent_tool(
            "changeset_cancel",
            "Withdraw a proposal still awaiting review.",
            EffectClass::Propose,
            schema::<ChangesetCancelParams>,
            schema::<StateView>,
        ),
        agent_tool(
            "operation_get",
            "What one of the agent's operation_ids did (§8.8).",
            EffectClass::Read,
            schema::<OperationGetParams>,
            schema::<AgentOperation>,
        ),
    ]);
    v
}

/// MCP may only expose reads and proposals (§4.6).
pub fn mcp_tools() -> Vec<ToolDescriptor> {
    tools()
        .into_iter()
        .filter(|t| {
            t.entrypoints.mcp() && matches!(t.effect, EffectClass::Read | EffectClass::Propose)
        })
        .collect()
}

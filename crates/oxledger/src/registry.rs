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
use crate::describe::{
    AssigneeList, DescribeParams, DescribeResult, IssueActions, IssueKeyPath, ProjectPath,
    ProjectTrackerPath, TrackerSchema,
};
use crate::mcp::{GetParams, ResolveParams};
use crate::service::{IssueDetail, ResolveResult, SearchParams, SearchResult};
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

pub fn tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "search",
            contract_version: 1,
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
            contract_version: 1,
            summary: "One issue by key, read from the database (not the index).",
            input_schema: schema::<GetParams>,
            output_schema: schema::<IssueDetail>,
            effect: EffectClass::Read,
            entrypoints: EntryPoints::Both,
            allowed_methods: READERS,
            rest: Some(RestRoute {
                method: "GET",
                path: "/issues/{key}",
                body: false,
            }),
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
    ]
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

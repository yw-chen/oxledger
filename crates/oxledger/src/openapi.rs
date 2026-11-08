// SPDX-License-Identifier: GPL-2.0-or-later
//! The `/api/v1` contract as OpenAPI 3.1 and as TypeScript types (D6:
//! schemars → OpenAPI → TS), generated from the Rust DTOs.
//!
//! `oxledger api-spec` writes `api/openapi.json` and `api/types.ts`; the test
//! `api_spec_is_current` fails when the committed files are out of date.
//! Session-only endpoints are listed in [`session_routes`]; operations shared
//! with MCP come from [`crate::registry`]. A test sends a request to every
//! listed route, so a route listed here but missing from the router fails.

use crate::agents::{AgentClientView, ContextView, DelegationView, IssuedCredential};
use crate::api::{
    Created, CredentialBody, DelegationBody, Done, Empty, LoginBody, LoginContextData, LoginData,
    NewContextBody, PasswordBody, SessionData, SessionItem,
};
use crate::error::ErrorBody;
use crate::registry;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value, json};

/// Who may call a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No session needed (login).
    Anonymous,
    /// A browser session.
    Session,
    /// A browser session that stepped up within 15 minutes.
    StepUp,
}

type SchemaFn = fn(&mut SchemaGenerator) -> Schema;

pub struct RouteDoc {
    pub method: &'static str,
    pub path: &'static str,
    pub summary: &'static str,
    pub access: Access,
    pub body: Option<SchemaFn>,
    pub data: SchemaFn,
}

fn s<T: JsonSchema>(g: &mut SchemaGenerator) -> Schema {
    g.subschema_for::<T>()
}

/// The SPA-only endpoints (login, sessions, delegation management).
pub fn session_routes() -> Vec<RouteDoc> {
    use Access::*;
    let r = |method, path, summary, access, body: Option<SchemaFn>, data: SchemaFn| RouteDoc {
        method,
        path,
        summary,
        access,
        body,
        data,
    };
    vec![
        r(
            "GET",
            "/auth/login-context",
            "Login challenge; sets the __Host-ox_login cookie.",
            Anonymous,
            None,
            s::<LoginContextData>,
        ),
        r(
            "POST",
            "/auth/login",
            "Sign in; sets the __Host-ox_session cookie.",
            Anonymous,
            Some(s::<LoginBody>),
            s::<LoginData>,
        ),
        r(
            "POST",
            "/auth/reauthenticate",
            "Step-up: re-enter the password.",
            Session,
            Some(s::<PasswordBody>),
            s::<Done>,
        ),
        r(
            "GET",
            "/auth/session",
            "Current identity, CSRF token and capabilities.",
            Session,
            None,
            s::<SessionData>,
        ),
        r(
            "POST",
            "/auth/logout",
            "End this session.",
            Session,
            None,
            s::<Done>,
        ),
        r(
            "GET",
            "/auth/sessions",
            "Own sessions (no tokens).",
            Session,
            None,
            s::<Vec<SessionItem>>,
        ),
        r(
            "DELETE",
            "/auth/sessions/{id}",
            "End one of the own sessions.",
            Session,
            None,
            s::<Done>,
        ),
        r(
            "POST",
            "/commands",
            "Run a command document now (human_direct, §6.8). Header Idempotency-Key: a uuid; resending it returns the same result.",
            Session,
            Some(s::<crate::commands::CommandDoc>),
            s::<crate::commands::CommandResult>,
        ),
        r(
            "GET",
            "/projects/{id}/versions",
            "Versions of a project (§17.8). Query status=open,locked (default), include=stats,next.",
            Session,
            None,
            s::<crate::versions::VersionList>,
        ),
        r(
            "POST",
            "/projects/{id}/versions",
            "Create a version (manage_versions). Header Idempotency-Key.",
            Session,
            Some(s::<crate::versions::NewVersion>),
            s::<crate::versions::VersionWriteResult>,
        ),
        r(
            "GET",
            "/versions/{id}",
            "One version with stats and health over the issues you can see.",
            Session,
            None,
            s::<crate::versions::VersionView>,
        ),
        r(
            "PATCH",
            "/versions/{id}",
            "Change a version (manage_versions). Header Idempotency-Key; returns the impact of date or status changes.",
            Session,
            Some(s::<crate::versions::VersionPatch>),
            s::<crate::versions::VersionWriteResult>,
        ),
        r(
            "DELETE",
            "/versions/{id}",
            "Soft-delete a version (manage_versions, R5). Query lock_version; header Idempotency-Key.",
            Session,
            None,
            s::<crate::versions::VersionWriteResult>,
        ),
        r(
            "GET",
            "/versions/{id}/impact",
            "Preview a change (manage_versions). Query effective_date (empty clears), status.",
            Session,
            None,
            s::<crate::versions::ImpactResult>,
        ),
        r(
            "GET",
            "/versions/{id}/events",
            "Version history, oldest first. Query cursor, limit.",
            Session,
            None,
            s::<crate::versions::VersionEvents>,
        ),
        r(
            "GET",
            "/agent-clients",
            "Agent software that can be delegated to.",
            Session,
            None,
            s::<Vec<AgentClientView>>,
        ),
        r(
            "GET",
            "/delegations",
            "Own delegations.",
            Session,
            None,
            s::<Vec<DelegationView>>,
        ),
        r(
            "POST",
            "/delegations",
            "Delegate a project to an agent (creates its first work context if needed).",
            StepUp,
            Some(s::<DelegationBody>),
            s::<DelegationView>,
        ),
        r(
            "POST",
            "/delegations/{id}/revoke",
            "Revoke a delegation (only that project).",
            Session,
            Some(s::<Empty>),
            s::<Done>,
        ),
        r(
            "GET",
            "/agent-contexts",
            "Own agents' work contexts, R and why a context is unusable.",
            Session,
            None,
            s::<Vec<ContextView>>,
        ),
        r(
            "POST",
            "/agent-contexts",
            "New work context (fresh_context_ack: true); closes the old one.",
            StepUp,
            Some(s::<NewContextBody>),
            s::<Created>,
        ),
        r(
            "POST",
            "/agent-contexts/{id}/close",
            "Stop this agent: close the context, revoke delegations and tokens.",
            Session,
            Some(s::<Empty>),
            s::<Done>,
        ),
        r(
            "POST",
            "/agent-contexts/{id}/credentials",
            "New agent token (shown once).",
            StepUp,
            Some(s::<CredentialBody>),
            s::<IssuedCredential>,
        ),
        r(
            "POST",
            "/agent-credentials/{id}/revoke",
            "Revoke an agent token.",
            Session,
            Some(s::<Empty>),
            s::<Done>,
        ),
    ]
}

/// Every documented route: session-only ones and REST-exposed registry tools.
pub fn routes() -> Vec<RouteDoc> {
    let mut all = session_routes();
    for t in registry::tools() {
        if let (true, Some(rest)) = (t.entrypoints.rest(), t.rest) {
            all.push(RouteDoc {
                method: rest.method,
                path: rest.path,
                summary: t.summary,
                access: Access::Session,
                body: rest.body.then_some(t.input_schema),
                data: t.output_schema,
            });
        }
    }
    all.sort_by(|a, b| (a.path, a.method).cmp(&(b.path, b.method)));
    all
}

/// Requests are described as the server deserializes them, responses as it
/// serializes them (fields skipped when empty are optional only in responses).
fn generator(serialize: bool) -> SchemaGenerator {
    let mut settings = SchemaSettings::draft2020_12();
    settings = if serialize {
        settings.for_serialize()
    } else {
        settings.for_deserialize()
    };
    settings.definitions_path = "/components/schemas".into();
    SchemaGenerator::new(settings)
}

/// `POST /issues/{key}` → `post_issues_key` (codegen-safe).
fn operation_id(method: &str, path: &str) -> String {
    let mut id = method.to_lowercase();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        id.push('_');
        id.extend(
            part.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .filter(|c| *c != '_' || !part.starts_with('{')),
        );
    }
    id.replace("__", "_")
}

fn path_params(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}}))
        .collect()
}

/// The OpenAPI 3.1 document.
pub fn document() -> Value {
    let mut g = generator(true);
    let mut req = generator(false);
    let error = g.subschema_for::<ErrorBody>();
    let mut paths = Map::new();
    for r in routes() {
        let data = (r.data)(&mut g);
        let mut op = json!({
            "summary": r.summary,
            "operationId": operation_id(r.method, r.path),
            "x-access": format!("{:?}", r.access).to_lowercase(),
            "responses": {
                "200": {"description": "OK", "content": {"application/json": {"schema": {
                    "type": "object", "required": ["data", "meta"],
                    "properties": {"data": data, "meta": {"$ref": "#/components/schemas/Meta"}}}}}},
                "default": {"description": "Error (§4.3)", "content": {"application/json": {"schema": {
                    "type": "object", "required": ["error"], "properties": {"error": error.clone()}}}}},
            },
        });
        if let Some(b) = r.body {
            op["requestBody"] =
                json!({"required": true, "content": {"application/json": {"schema": b(&mut req)}}});
        }
        let params = path_params(r.path);
        if !params.is_empty() {
            op["parameters"] = Value::Array(params);
        }
        if r.method != "GET" {
            op["x-csrf"] = json!(true);
        }
        let entry = paths.entry(r.path.to_string()).or_insert_with(|| json!({}));
        entry[r.method.to_lowercase()] = op;
    }
    let mut schemas: Map<String, Value> = g.take_definitions(true);
    for (name, schema) in req.take_definitions(true) {
        match schemas.get(&name) {
            Some(existing) if existing != &schema => panic!(
                "schema {name} differs between request and response; give the two directions separate types"
            ),
            _ => {
                schemas.insert(name, schema);
            }
        }
    }
    schemas.insert(
        "Meta".into(),
        json!({"type": "object", "required": ["request_id", "observed_at"], "properties": {
            "request_id": {"type": "string", "format": "uuid"},
            "observed_at": {"type": "string", "format": "date-time"}}}),
    );
    let schemas: Map<String, Value> = {
        let mut v: Vec<_> = schemas.into_iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.into_iter().collect()
    };
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "oxledger SPA API",
            "version": "1",
            "description": "Browser-session API (contract 02 §4). Cookie only: a bearer token is always unauthenticated. Non-GET requests need Content-Type: application/json, the SPA Origin and X-CSRF-Token. Every response is Cache-Control: no-store.",
        },
        "servers": [{"url": "/api/v1"}],
        "paths": paths,
        "components": {"schemas": schemas},
    })
}

// ---------------------------------------------------------------- TypeScript

fn ts_name(r: &str) -> String {
    r.rsplit('/').next().unwrap_or(r).to_string()
}

/// JSON Schema (the subset schemars emits) → a TypeScript type expression.
pub fn ts_type(s: &Value) -> String {
    let Some(o) = s.as_object() else {
        return if s == &Value::Bool(false) {
            "never".into()
        } else {
            "unknown".into()
        };
    };
    if let Some(r) = o.get("$ref").and_then(Value::as_str) {
        return ts_name(r);
    }
    if let Some(c) = o.get("const") {
        return c.to_string();
    }
    if let Some(e) = o.get("enum").and_then(Value::as_array) {
        return e
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(xs) = o.get(key).and_then(Value::as_array) {
            let mut parts: Vec<String> = xs.iter().map(ts_type).collect();
            parts.dedup();
            return parts.join(" | ");
        }
    }
    if let Some(xs) = o.get("allOf").and_then(Value::as_array) {
        return xs.iter().map(ts_type).collect::<Vec<_>>().join(" & ");
    }
    let types: Vec<String> = match o.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect(),
        _ => vec![],
    };
    if types.is_empty() {
        return if o.contains_key("properties") {
            ts_object(o)
        } else {
            "unknown".into()
        };
    }
    types
        .iter()
        .map(|t| match t.as_str() {
            "string" => "string".to_string(),
            "integer" | "number" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "array" => match o.get("items") {
                Some(items) => format!("Array<{}>", ts_type(items)),
                None => "unknown[]".to_string(),
            },
            "object" => ts_object(o),
            _ => "unknown".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn ts_object(o: &Map<String, Value>) -> String {
    let required: Vec<&str> = o
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = o.get("properties").and_then(Value::as_object) else {
        return match o.get("additionalProperties") {
            Some(Value::Bool(false)) => "Record<string, never>".into(),
            Some(v @ Value::Object(_)) => format!("Record<string, {}>", ts_type(v)),
            _ => "Record<string, unknown>".into(),
        };
    };
    let mut out = String::from("{\n");
    for (k, v) in props {
        if let Some(d) = v.get("description").and_then(Value::as_str) {
            out.push_str(&format!(
                "  /** {} */\n",
                d.replace("*/", "* /").replace('\n', " ")
            ));
        }
        let opt = if required.contains(&k.as_str()) {
            ""
        } else {
            "?"
        };
        out.push_str(&format!("  {k}{opt}: {};\n", indent(&ts_type(v))));
    }
    out.push('}');
    out
}

fn indent(t: &str) -> String {
    t.replace('\n', "\n  ")
}

/// `api/types.ts`: one type per component schema plus an endpoint map.
pub fn typescript(doc: &Value) -> String {
    let mut out = String::from(
        "// Generated by `oxledger api-spec` from api/openapi.json. Do not edit.\n// SPDX-License-Identifier: GPL-2.0-or-later\n\n",
    );
    if let Some(schemas) = doc["components"]["schemas"].as_object() {
        for (name, schema) in schemas {
            if let Some(d) = schema.get("description").and_then(Value::as_str) {
                out.push_str(&format!(
                    "/** {} */\n",
                    d.replace("*/", "* /").replace('\n', " ")
                ));
            }
            out.push_str(&format!("export type {name} = {};\n\n", ts_type(schema)));
        }
    }
    out.push_str("/** Success envelope (§4.3). */\nexport interface Ok<T> {\n  data: T;\n  meta: Meta;\n}\n\n");
    out.push_str(
        "/** Error envelope (§4.3). */\nexport interface Err {\n  error: ErrorBody;\n}\n\n",
    );
    out.push_str("/** Every endpoint: request body (or never) and response data. */\nexport interface Endpoints {\n");
    if let Some(paths) = doc["paths"].as_object() {
        for (path, ops) in paths {
            for (method, op) in ops.as_object().into_iter().flatten() {
                let body = op
                    .pointer("/requestBody/content/application~1json/schema")
                    .map(ts_type)
                    .unwrap_or_else(|| "never".into());
                let data = op
                    .pointer("/responses/200/content/application~1json/schema/properties/data")
                    .map(ts_type)
                    .unwrap_or_else(|| "unknown".into());
                out.push_str(&format!(
                    "  \"{} {path}\": {{ body: {}; data: {} }};\n",
                    method.to_uppercase(),
                    indent(&body),
                    indent(&data)
                ));
            }
        }
    }
    out.push_str("}\n");
    out
}

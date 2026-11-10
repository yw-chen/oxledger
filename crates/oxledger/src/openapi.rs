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
            "POST",
            "/commands/validate",
            "Check a command document as POST /commands would, without writing (design 05 §3.6.4). Read-only: CSRF, no Idempotency-Key. Body {command, expected_rules_version?}. Rejections (not_found, forbidden_op, invalid_request, form_rules_changed) come back as errors; anything else as items[].errors with outcome invalid. would_require_review is always false for people.",
            Session,
            Some(s::<crate::forms::ValidateCommandInput>),
            s::<crate::forms::ValidateResult>,
        ),
        r(
            "GET",
            "/projects/{id}/trackers/{t}/form",
            "The issue form (describe_form, design 05 §3.6.1). Query op (create, default; update) and issue (op update: the key). Fields you cannot read are left out, and so are rules about them; layout is a display hint (show_if is evaluated in the browser with api/form-conformance.json); rules_version goes back as expected_rules_version.",
            Session,
            None,
            s::<crate::forms::FormDescription>,
        ),
        r(
            "GET",
            "/admin/forms",
            "Forms of the site (no query) or of a project (query project: key or id). Site: administrators; project: administrators or manage_project in an active project.",
            Session,
            None,
            s::<Vec<crate::forms::FormSummary>>,
        ),
        r(
            "POST",
            "/admin/forms",
            "The form of {tracker_id, project?}, created when missing (naturally idempotent; no Idempotency-Key).",
            Session,
            Some(s::<crate::forms::NewForm>),
            s::<crate::forms::FormDetail>,
        ),
        r(
            "GET",
            "/forms/{id}",
            "A form with its published and draft rules and layout and the version strings.",
            Session,
            None,
            s::<crate::forms::FormDetail>,
        ),
        r(
            "GET",
            "/forms/{id}/versions",
            "Every version of a part (query part: rules, default, or layout), newest first.",
            Session,
            None,
            s::<Vec<crate::forms::FormVersionView>>,
        ),
        r(
            "PUT",
            "/forms/{id}/drafts/{part}",
            "Replace the draft of part rules or layout: {base_version, body}. base_version is the current draft's version, or the published version when there is no draft (conflict with details.current_version otherwise). The new draft gets the next version number. form_invalid (details.kind: unknown_field, restricted_condition_field, condition_too_complex, cycle, hides_required, field_not_applicable, malformed, invalid_rule; details.path, rule_id or section_id).",
            Session,
            Some(s::<crate::forms::DraftInput>),
            s::<crate::forms::FormVersionView>,
        ),
        r(
            "POST",
            "/forms/{id}/drafts/{part}/impact",
            "What publishing the draft would do: for rules, existing issues that break it (scan limit 20000; complete false above it), pending proposals that would fail and agent changes whose undo would stop applying (counts of what you can see); for a layout, layout_warnings only.",
            Session,
            None,
            s::<crate::forms::FormImpact>,
        ),
        r(
            "POST",
            "/forms/{id}/drafts/{part}/publish",
            "Publish the draft (step-up): {draft_version, mode}. Rules: mode block (form_publish_blocked with the impact when any issue breaks the rules or the scan was incomplete) or accept_and_mark (issues that break them get form_rule_marks, cleared by their next write that passes). Layout: no mode; changes layout_version only.",
            StepUp,
            Some(s::<crate::forms::PublishInput>),
            s::<crate::forms::PublishResult>,
        ),
        r(
            "POST",
            "/forms/{id}/drafts/{part}/discard",
            "Drop the draft (not_found when there is none).",
            Session,
            None,
            s::<crate::forms::FormDetail>,
        ),
        r(
            "POST",
            "/forms/{id}/rollback",
            "Publish an earlier version again under a new version number (step-up): {part, to_version, mode?}. Rules run the impact and honour mode as a publish does.",
            StepUp,
            Some(s::<crate::forms::RollbackInput>),
            s::<crate::forms::PublishResult>,
        ),
        r(
            "GET",
            "/admin/concepts",
            "Concepts and their labels for search alias expansion (design 05 §3.9), by preferred label. Every signed-in person; people who are not administrators see active concepts and their active labels that are not hidden. Query: q (part of a label, NFC + casefold), limit (clamped to 1–100, default 50), cursor.",
            Session,
            None,
            s::<crate::concepts::ConceptList>,
        ),
        r(
            "POST",
            "/admin/concepts",
            "Create a concept (administrators; 201): {pref_label, labels:[{label, kind? alt|hidden, status? active|staging}]}. The preferred label is also an active label (kind pref). Labels are 1–60 characters, compared after NFC + casefold. A label active in another concept: concept_label_taken (409, details.concept_id, pref_label, label). invalid_value kinds: label_invalid, label_kind_invalid, label_status_invalid, duplicate_label, too_many_labels (50).",
            Session,
            Some(s::<crate::concepts::NewConcept>),
            s::<crate::concepts::ConceptView>,
        ),
        r(
            "GET",
            "/admin/concepts/{id}",
            "One concept (not_found for a retired one unless you are an administrator).",
            Session,
            None,
            s::<crate::concepts::ConceptView>,
        ),
        r(
            "PATCH",
            "/admin/concepts/{id}",
            "Rename or retire a concept (administrators): {lock_version, pref_label?, status? active|retired}. A retired concept does not expand; its active labels still hold their names. conflict carries current_lock_version; concept_label_taken as for POST; invalid_value kinds: label_invalid, label_exists, concept_status_invalid.",
            Session,
            Some(s::<crate::concepts::ConceptPatch>),
            s::<crate::concepts::ConceptView>,
        ),
        r(
            "POST",
            "/admin/concepts/{id}/labels",
            "Add a label (administrators; 201): {label, kind? alt|hidden, status? active|staging}. invalid_value kinds: label_invalid, label_kind_invalid, label_status_invalid, label_exists, too_many_labels; concept_label_taken when active in another concept.",
            Session,
            Some(s::<crate::concepts::NewLabel>),
            s::<crate::concepts::ConceptView>,
        ),
        r(
            "PATCH",
            "/admin/concepts/{id}/labels/{label}",
            "Change a label's status (active | staging; staging labels do not expand) or kind (alt | hidden). The path label is matched after NFC + casefold. The preferred label cannot change (invalid_value kind pref_label_fixed). Activating a label active in another concept: concept_label_taken.",
            Session,
            Some(s::<crate::concepts::LabelPatch>),
            s::<crate::concepts::ConceptView>,
        ),
        r(
            "GET",
            "/admin/custom-fields",
            "Every custom field with its scope, roles, options, lock_version and state (administrators).",
            Session,
            None,
            s::<Vec<crate::field_admin::FieldAdminView>>,
        ),
        r(
            "POST",
            "/admin/custom-fields",
            "Create a custom field (administrators; 201). The per-field index is built before the answer; index_state pending means filters work without it. invalid_value kinds: name_invalid, name_taken, ftype_invalid, trackers_required, options_invalid, label_invalid, label_taken, builtin_role, unknown_reference.",
            Session,
            Some(s::<crate::field_admin::NewFieldInput>),
            s::<crate::field_admin::FieldAdminView>,
        ),
        r(
            "GET",
            "/admin/custom-fields/{id}",
            "One custom field (id or cf:<id>). Query preview_required=true adds required_impact: the issues the field applies to that have no value.",
            Session,
            None,
            s::<crate::field_admin::FieldDetail>,
        ),
        r(
            "PATCH",
            "/admin/custom-fields/{id}",
            "Change a custom field: {lock_version, set:{name?, description?, is_required?, is_for_all?, project_ids?, tracker_ids?, visible?, role_ids?, position?, deactivated?}}. conflict carries current_lock_version. set.ftype is unsupported_operation (details.suggested conversion). Visibility and scope changes quarantine agent contexts (security_epoch). Making the field required answers with required_impact.",
            Session,
            Some(s::<crate::field_admin::FieldPatch>),
            s::<crate::field_admin::FieldDetail>,
        ),
        r(
            "DELETE",
            "/admin/custom-fields/{id}",
            "Delete a custom field and its values (step-up; query lock_version). 202 with Location: index_state stays dropping until the background delete finishes. conflict (kind conversion_running) while a conversion of it runs.",
            StepUp,
            None,
            s::<crate::field_admin::FieldAdminView>,
        ),
        r(
            "POST",
            "/admin/custom-fields/{id}/options",
            "Add an option to a list field: {label, position?} (201).",
            Session,
            Some(s::<crate::field_admin::NewOption>),
            s::<crate::field_admin::FieldAdminView>,
        ),
        r(
            "PATCH",
            "/admin/custom-fields/{id}/options/{oid}",
            "Rename, reorder or (de)activate an option: {lock_version, label?, position?, active?}. An inactive option stays on issues that have it.",
            Session,
            Some(s::<crate::field_admin::OptionPatch>),
            s::<crate::field_admin::FieldAdminView>,
        ),
        r(
            "DELETE",
            "/admin/custom-fields/{id}/options/{oid}",
            "Delete an option no issue uses (query lock_version); in use: invalid_value (kind option_in_use), deactivate it instead.",
            Session,
            None,
            s::<crate::field_admin::FieldAdminView>,
        ),
        r(
            "POST",
            "/admin/custom-fields/{id}/conversions",
            "Q-S9 type conversion, step 1: {to: a new field definition, mapping}. Creates the new field and the conversion (previewed) and reports how many values convert and which issues would fail (count and sample keys you can see). Nothing is copied yet. Unsupported pairs: unsupported_operation; a bad mapping: invalid_value (kind mapping_invalid).",
            Session,
            Some(s::<crate::field_admin::ConversionInput>),
            s::<crate::field_admin::ConversionPreview>,
        ),
        r(
            "GET",
            "/admin/conversions/{id}",
            "A conversion's state, batches done and report (counts; failing issue ids and keys with reason codes, never values).",
            Session,
            None,
            s::<crate::field_admin::ConversionView>,
        ),
        r(
            "POST",
            "/admin/conversions/{id}/start",
            "Start (or, after failed, resume) copying values in batches of 500 in the background (step-up). Each batch commits its writes with the resume cursor, so a rerun never writes an issue twice; events have system_operation cf_convert:<id>. The old field is not deactivated automatically.",
            StepUp,
            None,
            s::<crate::field_admin::ConversionView>,
        ),
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
            "/filter-fields",
            "What search's `where` and `sort` take (design 05 §3.2): fields, ops, options or options_source, dynamic values, sortable keys, today and week_start. Query project=<id or key> limits to one project (unknown or invisible → not_found).",
            Session,
            None,
            s::<crate::filter::v2::FilterFields>,
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
            "GET",
            "/time-entries",
            "Time entries the caller may read (design 05 §3.5.2, §3.5.4), newest first by (spent_on, created_at, id). Query project (id or key), issue (id or key), user (id or me), activity, from, to (YYYY-MM-DD), cursor, limit (1-200, default 50). Visibility: view_time_entries with time_entries_visibility all, or own and the entry is yours; an entry on an issue you cannot see is left out. Write with POST /commands (log_time, update_time_entry, delete_time_entry).",
            Session,
            None,
            s::<crate::time_reports::TimeEntryPage>,
        ),
        r(
            "GET",
            "/time-entries/summary",
            "Totals of the time the caller may read (design 05 §3.5.2). Query from and to (required, at most 366 days), group_by user|activity|issue|tracker|fixed_version|day|week|month (default user), project, user, issue. scope_note is always only_visible. forbidden_op reason time_entries_not_visible when no visible project gives view_time_entries.",
            Session,
            None,
            s::<crate::time_reports::Summary>,
        ),
        r(
            "GET",
            "/time-entries/{id}",
            "One time entry with can_edit and can_delete (design 05 §3.5.1); not_found when not visible.",
            Session,
            None,
            s::<crate::time_reports::TimeEntryView>,
        ),
        r(
            "GET",
            "/time-entries/{id}/events",
            "History of a visible time entry, oldest first: kind created|updated|deleted|restored, origin, actor, changes {field: {old, new}}.",
            Session,
            None,
            s::<Vec<crate::time_reports::TimeEntryEvent>>,
        ),
        r(
            "GET",
            "/me/timesheet",
            "The caller's own time for one ISO week (design 05 §3.5.2): query week=YYYY-Www (default this week). Rows are (project, issue, activity); days follow org week_start.",
            Session,
            None,
            s::<crate::time_reports::Timesheet>,
        ),
        r(
            "GET",
            "/me/preferences",
            "The caller's own preferences (design 05 §3.8): interface roles (defaults only, never permissions), onboarded, theme, density, left-nav shortcuts, default views per project id (* = every other project). The first read creates them from the general template; no Idempotency-Key. Shortcuts and default views carry available (the project and view can be read now; deleted and not visible read the same, with only the stored id); send them back unchanged to keep them. There is no endpoint for anyone else's preferences.",
            Session,
            None,
            s::<crate::home::PreferencesView>,
        ),
        r(
            "PUT",
            "/me/preferences",
            "Change the caller's preferences. Header Idempotency-Key; lock_version as read; fields left out stay. nav_shortcuts and default_views replace the whole list / map; a default view is a view id or the object GET returned; view ids are stored canonical. conflict carries current_lock_version and current. invalid_value kinds: unknown_role, invalid_theme, invalid_density, too_many_shortcuts, unknown_shortcut_kind, view_required, unexpected_property, duplicate_shortcut, unknown_view, too_many_default_views, duplicate_default_view, invalid_view_id (over 80 characters), unknown_project, invalid_default_view (unknown_view and unknown_project do not tell missing from not visible).",
            Session,
            Some(s::<crate::home::PreferencesPut>),
            s::<crate::home::PreferencesView>,
        ),
        r(
            "GET",
            "/me/home",
            "The caller's home widgets (design 05 §3.8). need_decision is protected. Each widget says available: false when the view it runs or the milestones project cannot be read now (deleted, not visible, project left — the same answer); names only when available. Data comes from the endpoints of §3.8 (POST /views/{id}/run, GET /triage/counts, GET /projects/{id}/versions?include=stats, GET /me/timesheet, GET /home/agent-summary).",
            Session,
            None,
            s::<crate::home::HomeView>,
        ),
        r(
            "PUT",
            "/me/home",
            "Replace the caller's widget list (at most 12). Header Idempotency-Key; lock_version as read from GET /me/home. invalid_value kinds: protected_widget_missing (need_decision cannot be removed), duplicate_protected_widget, too_many_widgets, unknown_widget_type, invalid_widget_id, duplicate_widget_id, width_out_of_range (1–3), limit_out_of_range (view, 3–20), view_required, invalid_view_id (over 80 characters), unexpected_property, unknown_view, unknown_project. A view or project kept unchanged from the stored widget is accepted even if no longer available. conflict carries current_lock_version and current.",
            Session,
            Some(s::<crate::home::HomePut>),
            s::<crate::home::HomeView>,
        ),
        r(
            "POST",
            "/me/home/apply-template",
            "Apply the interface roles' template (Header Idempotency-Key): mode replace takes the template's widgets (the caller's need_decision stays), shortcuts and * default view; merge only adds what is missing (up to 12 widgets). Templates of several roles are taken in turn, at most 8 widgets. Returns the home and the preferences.",
            Session,
            Some(s::<crate::home::ApplyTemplate>),
            s::<crate::home::Applied>,
        ),
        r(
            "GET",
            "/home/templates",
            "The template of roles=pm,designer,engineer (comma list, order matters; none = the general template) as data for \"preview as role\": widgets, nav_shortcuts, default_views. Changes nothing; the browser renders it with the caller's own permissions.",
            Session,
            None,
            s::<crate::home::Template>,
        ),
        r(
            "GET",
            "/time-activities",
            "With project=<id or key>: the activities usable there now and default_id. Without: the site list (id, name, position, active, is_default, in_use).",
            Session,
            None,
            s::<crate::time_reports::ProjectActivities>,
        ),
        r(
            "PUT",
            "/admin/time-activities",
            "Replace the site activity list (administrators). An activity left out is deleted unless entries use it (invalid_value kind activity_in_use: deactivate it instead). At most one default, and it must be active (kinds default_count, default_inactive, name_invalid, name_taken, unknown_id).",
            Session,
            Some(s::<crate::time_reports::ActivitiesPut>),
            s::<Vec<crate::time_reports::Activity>>,
        ),
        r(
            "GET",
            "/projects/{id}/time-activities",
            "A project's time settings: module_enabled, activity overrides, lock_version, the site activities and can_manage.",
            Session,
            None,
            s::<crate::time_reports::ProjectTimeSettings>,
        ),
        r(
            "PUT",
            "/projects/{id}/time-activities",
            "Replace the project's activity overrides and optionally turn the time tracking module on or off (manage_project, active project). conflict carries details.current and current_lock_version. Turning the module on or off changes the project's security epoch.",
            Session,
            Some(s::<crate::time_reports::ProjectTimePut>),
            s::<crate::time_reports::ProjectTimeSettings>,
        ),
        r(
            "GET",
            "/projects/{id}/time-users",
            "Whom the caller may log time for in the project: themself, plus (with log_time_for_other_users) active members who may log time.",
            Session,
            None,
            s::<Vec<crate::time_reports::Named>>,
        ),
        r(
            "GET",
            "/org/timelog-settings",
            "The timelog rules (design 05 §3.5.3): max_hours_per_day (\"0\" = no limit), accept_0_hours, accept_future_dates, accept_closed_issues, require_issue, require_comments, lock_version.",
            Session,
            None,
            s::<crate::time_reports::TimelogSettings>,
        ),
        r(
            "PUT",
            "/org/timelog-settings",
            "Replace the timelog rules (administrators). conflict carries details.current and current_lock_version.",
            Session,
            Some(s::<crate::time_reports::TimelogSettingsPut>),
            s::<crate::time_reports::TimelogSettings>,
        ),
        r(
            "GET",
            "/projects/{id}/triage",
            "The project's Triage inbox (design 05 §3.4): settings {enabled, responsible, decline_status}, can_decide, and the pending issues the caller can see, oldest entered first (none while Triage is off). Query include_snoozed (default false), cursor, limit (1-100, default 50). view=decided: decisions of the last 30 days, newest first, each with decision, decided_by (agents: on_behalf_of), decided_at, self_triage, same_sponsor; filters self_triage=only|exclude|all (only = self_triage or same_sponsor) and decided_by_kind=agent|user|all. Decide with POST /commands (op triage_decide).",
            Session,
            None,
            s::<crate::triage::TriageList>,
        ),
        r(
            "GET",
            "/projects/{id}/triage-settings",
            "Triage settings of a visible project: enabled, entry_rule (creator_without_triage | all_new), responsible, decline_status, lock_version, can_manage.",
            Session,
            None,
            s::<crate::triage::TriageSettings>,
        ),
        r(
            "PUT",
            "/projects/{id}/triage-settings",
            "Replace the Triage settings (manage_project, active project). Header Idempotency-Key. responsible_user_id: an active assignable user of the project; decline_status_id: a done status, required when enabled. invalid_value kinds: triage_entry_rule, triage_decline_status_required, triage_decline_status_invalid, triage_responsible_invalid. conflict carries details.current and current_lock_version. With enabled true, warnings (kind decline_transition_missing) name roles with triage_issues and no workflow transition from a tracker's initial status to the decline status; they do not block.",
            Session,
            Some(s::<crate::triage::TriageSettingsPut>),
            s::<crate::triage::TriageSettings>,
        ),
        r(
            "GET",
            "/triage",
            "Every project where the caller has triage_issues and Triage is on, one inbox (each item has project). Query include_snoozed, cursor, limit.",
            Session,
            None,
            s::<crate::triage::TriageList>,
        ),
        r(
            "GET",
            "/triage/counts",
            "Pending (not snoozed) issues the caller can see per project of GET /triage; count stops at 99 with more true.",
            Session,
            None,
            s::<crate::triage::TriageCounts>,
        ),
        r(
            "GET",
            "/projects/{id}/tabs",
            "The project's shared tab bar (§18): built-in views (timeline, milestones, issues, dev, plan, board, triage; triage hidden by default), saved queries, links and project views (kind view, view_id), the default tab, is_default, lock_version and can_manage. Project-public content.",
            Session,
            None,
            s::<crate::project_tabs::TabSettings>,
        ),
        r(
            "PUT",
            "/projects/{id}/tabs",
            "Replace the whole tab list (manage_project, active project). Header Idempotency-Key. New saved queries, links and view tabs leave id out; keep the id of existing ones. A view tab (kind view) names a project view of this project in view_id; the built-in board may name a project board view. invalid_value kinds: builtin_missing, duplicate_id, unknown_id, tab_limit, tab_title_taken, tab_title_invalid, tab_query_scope, tab_query_syntax (with position), tab_url_invalid, tab_default_invalid, tab_view_invalid. conflict carries the current settings.",
            Session,
            Some(s::<crate::project_tabs::TabsPut>),
            s::<crate::project_tabs::TabSettings>,
        ),
        r(
            "POST",
            "/projects/{id}/tabs/reset",
            "Back to the seven built-in tabs (triage hidden), default timeline, is_default true (manage_project). Header Idempotency-Key.",
            Session,
            Some(s::<crate::project_tabs::TabsReset>),
            s::<crate::project_tabs::TabSettings>,
        ),
        r(
            "GET",
            "/projects/{id}/tabs/{tab}/issues",
            "Run a saved query tab as an ordinary search with your own visibility (R-T6): no stored results, no total. Query cursor.",
            Session,
            None,
            s::<crate::service::SearchResult>,
        ),
        r(
            "GET",
            "/views",
            "Views you can open (design 05 §3.3): built-ins (without project: assigned_to_me, reported_by_me, involving_me; with project: project_issues and project_board of that project), your personal views, and the project views of visible projects. Query project (id or key), scope (all, builtin, personal, project), cursor, limit.",
            Session,
            None,
            s::<crate::views::ViewList>,
        ),
        r(
            "POST",
            "/views",
            "Save a view. Header Idempotency-Key. scope personal (yours; project_id optional) or project (manage_public_views in an active project; project-public, no restricted fields, R-VD3). Errors: invalid_view (details.path, reason: unknown_field, invalid_shape, invalid_value, duplicate_value, kind_mismatch, lanes_required, restricted_field, project_scope, too_large), invalid_filter (in def.query.where), invalid_value (kinds view_name_invalid, view_limit, project_required, invalid_kind, invalid_scope), view_name_taken (details.view_id), forbidden_op (manage_public_views).",
            Session,
            Some(s::<crate::views::NewView>),
            s::<crate::views::ViewDetail>,
        ),
        r(
            "POST",
            "/views/preview",
            "Run an unsaved definition with the checks of saving it as scope (personal or project) in project_id. Same result as run; not saved, no Idempotency-Key.",
            Session,
            Some(s::<crate::views::PreviewInput>),
            s::<crate::views::ViewRun>,
        ),
        r(
            "GET",
            "/views/{id}",
            "One view: uuid or built-in id (builtin:assigned_to_me, builtin:reported_by_me, builtin:involving_me, builtin:project_issues:<project>, builtin:project_board:<project>). Someone else's personal view, or a project view of a project you cannot see, is not_found.",
            Session,
            None,
            s::<crate::views::ViewDetail>,
        ),
        r(
            "PATCH",
            "/views/{id}",
            "Rename and/or replace the definition (def replaces the whole def; the kind stays). Header Idempotency-Key. lock_version required; conflict carries details.current and details.current_lock_version. Built-in views: forbidden_op (builtin_view). Project views: manage_public_views.",
            Session,
            Some(s::<crate::views::ViewPatch>),
            s::<crate::views::ViewDetail>,
        ),
        r(
            "DELETE",
            "/views/{id}",
            "Delete a view (soft). Header Idempotency-Key, query lock_version (required). A project view a project tab shows is view_in_use (details.tabs: the tab titles); remove the tab first.",
            Session,
            None,
            s::<crate::views::ViewDeleted>,
        ),
        r(
            "POST",
            "/views/{id}/copy",
            "Copy any view you can open (built-ins too, except builtin:involving_me: forbidden_op not_copyable) into a new independent personal or project view. Header Idempotency-Key.",
            Session,
            Some(s::<crate::views::ViewCopy>),
            s::<crate::views::ViewDetail>,
        ),
        r(
            "POST",
            "/views/{id}/run",
            "Run a view with your own visibility (K3). List: items with values, groups and totals over every visible match; board: lanes with their items, counts (at most 999, capped), wip and next_cursor (a board cursor goes with its lane). Counts and totals are null when out of the 2 s budget or with free text. override.q replaces the view's q; override.where_and narrows. A saved condition on a field you cannot read is invalid_filter (field_unavailable). POST so long cursors fit.",
            Session,
            Some(s::<crate::views::RunInput>),
            s::<crate::views::ViewRun>,
        ),
        r(
            "GET",
            "/views/{id}/events",
            "The view's change history, newest first (whoever can open the view). Redacted events show [已遮蔽]. Query cursor.",
            Session,
            None,
            s::<crate::views::ViewEvents>,
        ),
        r(
            "GET",
            "/projects/{id}/setting-events",
            "The settings history, newest first (manage_project). PR hide/unhide and PR-based revocations are only listed for a system administrator. Query cursor.",
            Session,
            None,
            s::<crate::project_tabs::SettingEvents>,
        ),
        r(
            "GET",
            "/projects/{id}/repos",
            "GitHub repositories linked to the project (§19.7): full_name, is_private, repo_state (inaccessible = the App lost access; data is as of synced_at), sync_state (backfilling, ok, paused), synced_at, paused_until.",
            Session,
            None,
            s::<crate::github::links::RepoLinks>,
        ),
        r(
            "GET",
            "/projects/{id}/repos/linkable",
            "Repositories you may link here (manage_project): opened by an administrator, reachable by the App, not linked yet.",
            Session,
            None,
            s::<crate::github::links::LinkableRepos>,
        ),
        r(
            "POST",
            "/projects/{id}/repos",
            "Link a repository (manage_project, active project). Header Idempotency-Key. repo_not_linkable (unknown, not opened, or unreachable: one shape), repo_already_linked, repo_link_limit (20). Starts with sync_state backfilling.",
            Session,
            Some(s::<crate::github::links::LinkInput>),
            s::<crate::github::links::RepoLink>,
        ),
        r(
            "DELETE",
            "/projects/{id}/repos/{link_id}",
            "Unlink (manage_project or administrator; works when the repository is gone or closed for linking). Header Idempotency-Key. Removes that link's refs to this project's issues only.",
            Session,
            None,
            s::<crate::github::links::Unlinked>,
        ),
        r(
            "GET",
            "/projects/{id}/dev",
            "The development tab: issues of this project with PRs, newest PR activity first, only issues you can see, never a total. Query filter (all, ci_failure, review, merged_open), include_hidden (managers), cursor.",
            Session,
            None,
            s::<crate::github::read::ProjectDev>,
        ),
        r(
            "POST",
            "/projects/{id}/dev/prs/{pr_id}/hide",
            "Hide a PR in this project only (manage_project). Header Idempotency-Key. not_found unless the PR names an issue of this project you can see. Not a redaction: agents that read it stay valid.",
            Session,
            None,
            s::<crate::github::links::HideResult>,
        ),
        r(
            "DELETE",
            "/projects/{id}/dev/prs/{pr_id}/hide",
            "Show a hidden PR again (manage_project). Header Idempotency-Key.",
            Session,
            None,
            s::<crate::github::links::HideResult>,
        ),
        r(
            "POST",
            "/projects/{id}/dev/revoke",
            "Revoke GitHub content (§19.9; manage_project or administrator; step-up): link_id unlinks a whole link, pr_ids hides PRs you can see. Bumps the project's security_epoch, so every agent context that read the project is quarantined. Header Idempotency-Key.",
            Session,
            Some(s::<crate::github::links::RevokeInput>),
            s::<crate::github::links::Revoked>,
        ),
        r(
            "POST",
            "/issues/{key}/notes/{note_id}/redact",
            "Redact a note (not_found when you cannot read it). §7.5 D12 redaction (manage_project of the project or a system administrator; human session; step-up within 15 minutes). One transaction: the project's security_epoch + 1 (agent contexts that read it are quarantined), the text replaced by [已遮蔽], a redaction record without the original, and the derived copies (every proposal content and group label of proposals to or from the project, execution before/after values). Already redacted → changed=false. reason_code: personal_data, confidential, legal, other; reason 1–500 characters, never the original text. Header Idempotency-Key.",
            Session,
            Some(s::<crate::redaction::RedactInput>),
            s::<crate::redaction::Redacted>,
        ),
        r(
            "POST",
            "/issues/{key}/events/{event_id}/redact",
            "Redact an issue event (its payload); event_id comes from recent_events. §7.5 D12 redaction (manage_project of the project or a system administrator; human session; step-up within 15 minutes). One transaction: the project's security_epoch + 1 (agent contexts that read it are quarantined), the text replaced by [已遮蔽], a redaction record without the original, and the derived copies (every proposal content and group label of proposals to or from the project, execution before/after values). Already redacted → changed=false. reason_code: personal_data, confidential, legal, other; reason 1–500 characters, never the original text. Header Idempotency-Key.",
            Session,
            Some(s::<crate::redaction::RedactInput>),
            s::<crate::redaction::Redacted>,
        ),
        r(
            "POST",
            "/versions/{id}/events/{seq}/redact",
            "Redact a version event (§17.13). §7.5 D12 redaction (manage_project of the project or a system administrator; human session; step-up within 15 minutes). One transaction: the project's security_epoch + 1 (agent contexts that read it are quarantined), the text replaced by [已遮蔽], a redaction record without the original, and the derived copies (every proposal content and group label of proposals to or from the project, execution before/after values). Already redacted → changed=false. reason_code: personal_data, confidential, legal, other; reason 1–500 characters, never the original text. Header Idempotency-Key.",
            Session,
            Some(s::<crate::redaction::RedactInput>),
            s::<crate::redaction::Redacted>,
        ),
        r(
            "POST",
            "/projects/{id}/setting-events/{event_id}/redact",
            "Redact a project setting event (§18.2; pr_hidden / pr_unhidden: administrators only). §7.5 D12 redaction (manage_project of the project or a system administrator; human session; step-up within 15 minutes). One transaction: the project's security_epoch + 1 (agent contexts that read it are quarantined), the text replaced by [已遮蔽], a redaction record without the original, and the derived copies (every proposal content and group label of proposals to or from the project, execution before/after values). Already redacted → changed=false. reason_code: personal_data, confidential, legal, other; reason 1–500 characters, never the original text. Header Idempotency-Key.",
            Session,
            Some(s::<crate::redaction::RedactInput>),
            s::<crate::redaction::Redacted>,
        ),
        r(
            "POST",
            "/time-entries/{id}/redact",
            "Redact a time entry's comments (design 05 §2.5 D12). Same rules as the other redactions: manage_project of the project or a system administrator; human session; step-up within 15 minutes; an entry you cannot read is not_found. One transaction: the project's security_epoch + 1, the comments and every event that holds them replaced by [已遮蔽] (time_entry.redacted = true), one redaction record per such event, and the derived copies (as for notes, plus execution_time_entry_changes). Already redacted → changed=false. Header Idempotency-Key.",
            Session,
            Some(s::<crate::redaction::RedactInput>),
            s::<crate::redaction::Redacted>,
        ),
        r(
            "GET",
            "/issues/{key}/dev",
            "Pull requests of one issue (R-G9: at most 20, open then draft then the rest, newest first) with review and CI roll-ups, merged_open (R-G5) and synced_at. not_found like the issue when you cannot see it.",
            Session,
            None,
            s::<crate::github::read::IssueDev>,
        ),
        r(
            "GET",
            "/admin/github",
            "Installations of the GitHub App and their repositories, with the linkable switch (administrators).",
            Session,
            None,
            s::<crate::github::links::AdminGithub>,
        ),
        r(
            "PUT",
            "/admin/github",
            "Open or close repositories for linking (administrators). Closing keeps existing links.",
            Session,
            Some(s::<crate::github::links::AdminGithubPut>),
            s::<crate::github::links::AdminGithub>,
        ),
        r(
            "GET",
            "/projects/{id}/timeline",
            "The schedule view (§17.8): one lane per version (by date, deleted ones only when you see an issue in them) plus the lane without a version. Query from, to (YYYY-MM-DD, at most 400 days; one end alone keeps the computed other), versions (ids or none), include_closed, cursor (pages one lane, 200 items). Only issues you can see count anywhere.",
            Session,
            None,
            s::<crate::versions::Timeline>,
        ),
        r(
            "GET",
            "/projects/{id}/schedule-settings",
            "The health thresholds in use (§17.12): effective values, source org or project, both layers and lock_version.",
            Session,
            None,
            s::<crate::versions::ScheduleSettings>,
        ),
        r(
            "PUT",
            "/projects/{id}/schedule-settings",
            "Change the project's thresholds (manage_versions). Header Idempotency-Key. lock_version must match (conflict). With inherit_org true leave both values out; otherwise send both, null turns a rule off.",
            Session,
            Some(s::<crate::versions::ScheduleSettingsInput>),
            s::<crate::versions::ScheduleSettings>,
        ),
        r(
            "GET",
            "/org/schedule-settings",
            "The organization's default health thresholds and their lock_version.",
            Session,
            None,
            s::<crate::versions::OrgScheduleSettings>,
        ),
        r(
            "PUT",
            "/org/schedule-settings",
            "Change the organization defaults (a system administrator). Header Idempotency-Key. lock_version must match (conflict).",
            Session,
            Some(s::<crate::versions::OrgScheduleSettingsInput>),
            s::<crate::versions::OrgScheduleSettings>,
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
            "/changesets",
            "Your proposals, newest first (§8), 50 per page. Query bucket=awaiting_me (default: proposed or failed) or all; state; group (a group id); cursor. Proposed rows carry the field diff, warnings and review_modes (queue when it may be ticked, §15.3).",
            Session,
            None,
            s::<crate::changesets::ChangesetList>,
        ),
        r(
            "GET",
            "/changesets/{id}",
            "A proposal: state, its current version, the last execution (owner only).",
            Session,
            None,
            s::<crate::changesets::ChangesetView>,
        ),
        r(
            "GET",
            "/changesets/{id}/versions/{v}",
            "One version of a proposal (owner only).",
            Session,
            None,
            s::<crate::changesets::ChangesetView>,
        ),
        r(
            "POST",
            "/changesets/{id}/validations",
            "Check the current version now, without writing (§8.5).",
            Session,
            Some(s::<crate::changesets::ValidateInput>),
            s::<crate::changesets::Validation>,
        ),
        r(
            "POST",
            "/changesets/{id}/approve-and-apply",
            "Approve and apply a version (§8.6). Header Idempotency-Key. 200 with the execution (also when it failed); 202 with Location while the result is not known: resend the same request.",
            Session,
            Some(s::<crate::changesets::ApproveInput>),
            s::<crate::changesets::ExecutionView>,
        ),
        r(
            "POST",
            "/changesets/{id}/reject",
            "Reject the current version (final). Header Idempotency-Key.",
            Session,
            Some(s::<crate::changesets::DecisionInput>),
            s::<crate::changesets::StateView>,
        ),
        r(
            "POST",
            "/changesets/{id}/cancel",
            "Cancel a proposal not yet applied. Header Idempotency-Key.",
            Session,
            Some(s::<crate::changesets::CancelInput>),
            s::<crate::changesets::StateView>,
        ),
        r(
            "POST",
            "/changesets/{id}/revisions",
            "Write a new version yourself within what the agent was allowed (§8.5). Header Idempotency-Key.",
            Session,
            Some(s::<crate::changesets::RevisionInput>),
            s::<crate::changesets::ChangesetView>,
        ),
        r(
            "POST",
            "/changesets/{id}/revision-previews",
            "Resolve a conflict field by field (§15.4): keep the issue's current value or the proposed one. Returns the complete revised document and a short-lived revision_review_token. Writes nothing.",
            Session,
            Some(s::<crate::changesets::PreviewInput>),
            s::<crate::changesets::RevisionPreview>,
        ),
        r(
            "POST",
            "/changesets/{id}/revise-and-apply",
            "Store the previewed revision as your version, approve and apply it in one step (§15.4). Header Idempotency-Key. Answers like approve-and-apply; review_outdated (409) when an issue changed since the preview.",
            Session,
            Some(s::<crate::changesets::ReviseAndApplyInput>),
            s::<crate::changesets::ExecutionView>,
        ),
        r(
            "GET",
            "/executions/{id}",
            "An execution of your approval. Never starts anything.",
            Session,
            None,
            s::<crate::changesets::ExecutionView>,
        ),
        r(
            "GET",
            "/executions/{id}/undo-preview",
            "What undoing one of your agent's executions (or your undo) would restore now (§16.7): per field before, after, current and whether it can be restored (reasons changed_since, value_no_longer_valid, permission, redacted, expired); notes to retract, relations to remove, transitions to choose. Writes nothing.",
            Session,
            None,
            s::<crate::undo::UndoPreview>,
        ),
        r(
            "POST",
            "/executions/{id}/undo",
            "Undo what is still restorable, re-checked under the locks (§16.7). Header Idempotency-Key. A new human_direct execution with reverts_execution_id. undo_not_applicable (409) when nothing can be restored or the subset breaks a rule.",
            Session,
            Some(s::<crate::undo::UndoInput>),
            s::<crate::undo::UndoResult>,
        ),
        r(
            "GET",
            "/agents/{id}/activity",
            "Your agent's direct writes, applied proposals, refusals and undos, newest first (§16.10). Query from, to (RFC 3339), issue (a key). Only issues you can read now.",
            Session,
            None,
            s::<crate::undo::Activity>,
        ),
        r(
            "POST",
            "/agents/{id}/incident-recovery/preview",
            "Freeze the list of your agent's executions since a time (§16.7): newest first, each marked automatic or needs_choice. Send one undo per item, in order; pausing the agent first is advised.",
            Session,
            Some(s::<crate::api::IncidentInput>),
            s::<crate::undo::IncidentPreview>,
        ),
        r(
            "GET",
            "/home/agent-summary",
            "The home cards (§16.10): issues each of your agents changed today (as you can see them now) and contexts the breaker suspended.",
            Session,
            None,
            s::<crate::undo::AgentSummary>,
        ),
        r(
            "GET",
            "/operations/{id}",
            "What one of your Idempotency-Keys did.",
            Session,
            None,
            s::<crate::changesets::OperationView>,
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
            "PUT",
            "/delegations/{id}/read-own-time",
            "Let the agent read your own time or stop it (design 05 §3.5.4 A1, §6.14). enabled true needs ack true (invalid_value kind ack_required). The response carries read_own_time_warning, the wizard's warning text.",
            Session,
            Some(s::<crate::api::ReadOwnTimeBody>),
            s::<DelegationView>,
        ),
        r(
            "GET",
            "/delegations/{id}/policies",
            "A delegation's write policies (direct / review / off per command and field) and operations that could be added.",
            Session,
            None,
            s::<crate::agents::PolicyView>,
        ),
        r(
            "PUT",
            "/delegations/{id}/policies",
            "Change modes or add commands / fields (needs a step-up; policy_version + 1).",
            Session,
            Some(s::<crate::agents::PolicyInput>),
            s::<crate::agents::PolicyView>,
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
            "/agent-contexts/{id}/suspend",
            "Pause the agent's writes in this context (reads go on).",
            Session,
            Some(s::<Empty>),
            s::<Done>,
        ),
        r(
            "POST",
            "/agent-contexts/{id}/resume",
            "Resume a paused context (needs a step-up; re-checks the context first).",
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

/// Routes answering 201 Created instead of 200 (design 05 §3.3).
pub const CREATED_ROUTES: &[(&str, &str)] = &[
    ("POST", "/views"),
    ("POST", "/views/{id}/copy"),
    ("POST", "/admin/custom-fields"),
    ("POST", "/admin/custom-fields/{id}/options"),
    ("POST", "/admin/custom-fields/{id}/conversions"),
    ("POST", "/admin/concepts"),
    ("POST", "/admin/concepts/{id}/labels"),
];

/// The OpenAPI 3.1 document.
pub fn document() -> Value {
    let mut g = generator(true);
    let mut req = generator(false);
    let error = g.subschema_for::<ErrorBody>();
    let mut paths = Map::new();
    for r in routes() {
        let data = (r.data)(&mut g);
        let (status, text) = if CREATED_ROUTES.contains(&(r.method, r.path)) {
            ("201", "Created")
        } else {
            ("200", "OK")
        };
        let mut op = json!({
            "summary": r.summary,
            "operationId": operation_id(r.method, r.path),
            "x-access": format!("{:?}", r.access).to_lowercase(),
            "responses": {
                status: {"description": text, "content": {"application/json": {"schema": {
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
                    .or_else(|| {
                        op.pointer(
                            "/responses/201/content/application~1json/schema/properties/data",
                        )
                    })
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

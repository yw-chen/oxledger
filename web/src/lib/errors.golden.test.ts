// SPDX-License-Identifier: GPL-2.0-or-later
// G7 (1) / G9: the whole code → wording table of errors.ts as a file snapshot
// (__snapshots__/errors.snap.md). Review a diff of that file like any other
// wording change; `npx vitest run -u` rewrites it.
import { describe, expect, it } from 'vitest';
import type { ErrorCode } from '$api/types';
import spec from '$api/openapi.json';
import { ApiError } from './api';
import { describeError, describeFailure, type Shown } from './errors';

// Every ErrorCode the API can send (api/types.ts, generated from
// api/openapi.json). `satisfies` makes svelte-check fail when a code is added
// or removed there; the first test checks the same against openapi.json.
const CODES = {
	invalid_request: true,
	unauthenticated: true,
	invalid_credentials: true,
	reauthentication_required: true,
	csrf_failed: true,
	human_session_required: true,
	forbidden_op: true,
	not_found: true,
	invalid_value: true,
	invalid_argument: true,
	invalid_filter: true,
	invalid_cursor: true,
	ambiguous_reference: true,
	context_unavailable: true,
	delegation_unavailable: true,
	rate_limited: true,
	temporarily_unavailable: true,
	conflict: true,
	precondition_failed: true,
	idempotency_key_reused: true,
	execution_in_progress: true,
	operation_result_expired: true,
	unsupported_operation: true,
	unsupported_execution_mode: true,
	required_field_missing: true,
	readonly_field: true,
	invalid_transition: true,
	relation_not_allowed: true,
	triage_state_invalid: true,
	version_not_open: true,
	version_in_use: true,
	version_name_taken: true,
	version_superseded: true,
	approval_invalidated: true,
	review_outdated: true,
	undo_not_applicable: true,
	repo_not_linkable: true,
	repo_already_linked: true,
	repo_link_limit: true,
	already_applied: true,
	changeset_not_executable: true,
	proposal_unavailable: true,
	changeset_expired: true,
	audience_expansion_forbidden: true,
	source_unverifiable: true,
	invalid_citation: true,
	citation_mismatch: true,
	quota_exceeded: true,
	agent_suspended: true,
	invalid_query: true,
	search_unavailable: true,
	invalid_consistency_token: true,
	consistency_epoch_changed: true,
	search_not_caught_up: true,
	field_definition_changed: true,
	query_timeout: true,
	invalid_view: true,
	view_name_taken: true,
	view_in_use: true,
	form_rules_changed: true,
	form_invalid: true,
	form_publish_blocked: true,
	concept_label_taken: true,
	runtime_context_closed: true,
	runtime_client_taken: true,
	ai_unavailable: true,
	ai_provider_error: true,
	ai_model_disallowed: true,
	ai_endpoint_invalid: true,
	ai_conversation_read_only: true,
	ai_provider_changed: true,
	ai_run_in_progress: true,
	payload_too_large: true,
	wiki_page_exists: true,
	wiki_has_children: true,
	wiki_title_invalid: true
} satisfies Record<ErrorCode, true>;

/** The server's message is English; a code that reaches `default` shows it. */
const MSG = 'SERVER MESSAGE (no wording of its own)';
const withStatus = (status: number, code: string, details: unknown = {}) =>
	new ApiError(status, { code, details, message: MSG, next_actions: [], retryable: false, request_id: 'r' } as never, null);

function block(label: string, s: Shown): string {
	const lines = [`## ${label}`, `title:   ${s.title}`, `changed: ${s.changed}`];
	if (s.next !== undefined) lines.push(`next:    ${s.next}`);
	if (s.action !== undefined) lines.push(`action:  ${s.action}`);
	return lines.join('\n');
}

type Variant = [code: string, label: string, details: unknown];
const each = (code: string, key: string, values: string[], extra: Record<string, unknown> = {}): Variant[] =>
	values.map((v) => [code, `${key}=${v}${Object.keys(extra).length ? `, ${Object.entries(extra).map(([k, x]) => `${k}=${x}`).join(', ')}` : ''}`, { [key]: v, ...extra }]);

/** Codes whose wording depends on `details`: one entry per branch. */
const VARIANTS: Variant[] = [
	['context_unavailable', 'kind=closed', { kind: 'closed' }],
	['invalid_transition', 'allowed=[進行中, 已完成]', { allowed: [{ name: '進行中' }, { name: '已完成' }] }],
	['invalid_value', 'kind=max_hours_per_day, no remaining', { kind: 'max_hours_per_day', max_hours_per_day: 8 }],
	['invalid_value', 'kind=max_hours_per_day, remaining=1.5', { kind: 'max_hours_per_day', max_hours_per_day: 8, remaining: 1.5 }],
	...each('invalid_value', 'kind', ['future_date', 'closed_issue', 'hours_invalid', 'activity_inactive', 'snooze_range', 'activity_in_use']),
	...each(
		'invalid_value',
		'kind',
		['tab_title_invalid', 'tab_title_taken', 'tab_url_invalid', 'tab_query_scope', 'tab_query_syntax', 'tab_default_invalid', 'tab_limit', 'builtin_missing', 'duplicate_id', 'unknown_id', 'unknown_kind'],
		{ path: 'tabs[5].url' }
	),
	['invalid_value', 'kind=tab_limit, no path', { kind: 'tab_limit' }],
	['invalid_value', 'allowed=[a, b]', { allowed: ['a', 'b'] }],
	['required_field_missing', 'field=subject', { field: 'subject' }],
	['required_field_missing', 'fields=[hours, activity_id, cf:7]', { fields: ['hours', 'activity_id', 'cf:7'] }],
	['version_in_use', 'keys=[AUTH-1, AUTH-2]', { keys: ['AUTH-1', 'AUTH-2'] }],
	...each('invalid_filter', 'reason', ['project_scope_required', 'no_next_version', 'unknown_version']),
	['invalid_filter', 'path=version', { path: 'version' }],
	['invalid_filter', 'path=status_id', { path: 'status_id' }],
	['invalid_request', 'path=project, allowed=[AUTH, WEB]', { path: 'project', allowed: ['AUTH', 'WEB'] }],
	['invalid_argument', 'path=assignee', { path: 'assignee' }],
	['invalid_request', 'path=body.subject', { path: 'body.subject' }],
	['ambiguous_reference', 'candidates=[AUTH-1, AUTH-10]', { candidates: ['AUTH-1', 'AUTH-10'] }],
	...each(
		'invalid_view',
		'reason',
		['unknown_field', 'invalid_shape', 'invalid_value', 'kind_mismatch', 'lanes_required', 'restricted_field', 'project_scope', 'too_large', 'duplicate_value'],
		{ path: 'columns' }
	),
	['view_in_use', 'tabs=[看板, 本週]', { tabs: ['看板', '本週'] }],
	...each('form_invalid', 'kind', ['unknown_field', 'restricted_condition_field', 'condition_too_complex', 'cycle', 'hides_required', 'field_not_applicable']),
	['concept_label_taken', 'label=登錄, pref_label=登入', { label: '登錄', pref_label: '登入' }],
	['runtime_client_taken', 'label=筆電', { label: '筆電' }],
	['ai_unavailable', 'kind=circuit_open', { kind: 'circuit_open' }],
	['ai_provider_error', 'kind=timeout', { kind: 'timeout' }],
	['ai_endpoint_invalid', 'kind=no_key', { kind: 'no_key' }],
	['ai_endpoint_invalid', 'kind=endpoint_blocked', { kind: 'endpoint_blocked' }],
	['ai_provider_changed', 'provider=Anthropic (claude-x)', { provider: { name: 'Anthropic', model: 'claude-x' } }],
	['ai_provider_changed', 'provider=Anthropic, no model', { provider: { name: 'Anthropic' } }],
	// M2.2 wiki (design 06 §5)
	...each('forbidden_op', 'reason', ['permission', 'wiki_protected', 'project_not_writable']),
	...each('invalid_value', 'kind', ['wiki_cycle', 'wiki_depth', 'wiki_version_limit', 'wiki_page_limit', 'wiki_comment', 'wiki_body', 'start_page_invalid', 'redacted_source'], { path: 'ops[0].parent' }),
	['wiki_has_children', 'children=3', { page_id: 'p', children: 3 }],
	['wiki_title_invalid', 'reason=length', { reason: 'length', path: 'ops[0].target.page' }],
	['wiki_title_invalid', 'reason=leading_dot', { reason: 'leading_dot' }],
	['wiki_title_invalid', 'reason=char, char=#', { reason: 'char', char: '#', codepoint: 'U+0023' }],
	['wiki_title_invalid', 'reason=char, char=U+202E', { reason: 'char', char: '‮', codepoint: 'U+202E' }],
	['wiki_title_invalid', 'reason=char, char=U+200B', { reason: 'char', char: '​', codepoint: 'U+200B' }]
];

describe('error wording table (golden)', () => {
	it('lists exactly the codes in api/openapi.json', () => {
		const schema = (spec as unknown as { components: { schemas: { ErrorCode: { oneOf: { const: string }[] } } } }).components.schemas.ErrorCode;
		expect(Object.keys(CODES).sort()).toEqual(schema.oneOf.map((c) => c.const).sort());
	});

	it('matches __snapshots__/errors.snap.md', async () => {
		const out: string[] = [
			'# describeError: code → wording (src/lib/errors.ts)',
			'',
			'Generated by src/lib/errors.golden.test.ts. Every code with empty details (status 409), then each details branch.',
			`A title "${MSG}" means the code falls through to the default branch.`,
			'',
			'# Every code, empty details',
			''
		];
		for (const code of Object.keys(CODES).sort()) out.push(block(code, describeError(withStatus(409, code))), '');
		out.push('# Not from the API', '');
		out.push(block('(not an ApiError)', describeError(new Error('x'))), '');
		out.push(block('network (status 0, no body)', describeError(new ApiError(0, null, null))), '');
		out.push(block('internal_error (status 500, no body)', describeError(new ApiError(500, null, null))), '');
		out.push(block('unknown code, status 500', describeError(withStatus(500, 'something_new'))), '');
		out.push(block('unknown code, status 409', describeError(withStatus(409, 'something_new'))), '');
		out.push('# Details branches', '');
		for (const [code, label, details] of VARIANTS) out.push(block(`${code}: ${label}`, describeError(withStatus(409, code, details))), '');
		out.push('# describeFailure (stored execution errors)', '');
		out.push(`## null\n${describeFailure(null)}`, '');
		out.push(`## conflict\n${describeFailure({ code: 'conflict', message: MSG })}`, '');
		out.push(`## agent_suspended\n${describeFailure({ code: 'agent_suspended', message: MSG })}`, '');
		await expect(out.join('\n')).toMatchFileSnapshot('./__snapshots__/errors.snap.md');
	});
});

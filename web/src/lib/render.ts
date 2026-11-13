// SPDX-License-Identifier: GPL-2.0-or-later
// Field values as people read them (§15.3: names, not ids). Command
// documents carry ids and typed custom values (`{"<type>": v}`); the issue
// view carries names. Both go through here.

import type { Assignee, FieldSchema, SpaIssue, VersionView } from '$api/types';
import { fieldLabel, fmtValue } from './format';

export type Lookups = { fields: FieldSchema[]; assignees: Assignee[]; versions: VersionView[] };

/** `{"list": "<id>"}` → `"<id>"`; anything else as it is. */
export function untyped(v: unknown): unknown {
	if (v && typeof v === 'object' && !Array.isArray(v)) {
		const e = Object.entries(v as Record<string, unknown>);
		if (e.length === 1 && ['int', 'float', 'date', 'string', 'text', 'list', 'user', 'bool', 'link'].includes(e[0][0])) return e[0][1];
	}
	return v;
}

export function labelOf(key: string, l: Lookups): string {
	return l.fields.find((f) => f.key === key)?.name ?? fieldLabel(key);
}

/** One value of field `key` for display. */
export function show(key: string, value: unknown, l: Lookups): string {
	const v = untyped(value);
	if (typeof v === 'string') {
		const f = l.fields.find((x) => x.key === key);
		const o = f?.options?.find((x) => x.id === v);
		if (o) return o.label;
		const a = l.assignees.find((x) => x.id === v);
		if (a) return a.name;
		const ver = l.versions.find((x) => x.id === v);
		if (ver) return ver.name;
	}
	return fmtValue(v);
}

/** The values a command document would compare against: ids for
 *  references, plain values for custom fields. */
export function issueValues(v: SpaIssue, fields: FieldSchema[]): Record<string, unknown> {
	const vals: Record<string, unknown> = {
		subject: v.subject,
		description: v.description,
		assigned_to_id: v.assigned_to_id,
		priority_id: v.priority_id,
		fixed_version_id: v.fixed_version?.id ?? null,
		start_date: v.start_date,
		due_date: v.due_date,
		estimated_hours: v.estimated_hours ?? null,
		is_private: v.is_private
	};
	for (const f of fields) {
		if (!f.key.startsWith('cf:')) continue;
		const cf = v.custom_fields?.find((c) => c.name === f.name);
		const val = cf?.value as unknown;
		vals[f.key] = val && typeof val === 'object' && 'id' in (val as object) ? (val as { id: string }).id : (val ?? null);
	}
	return vals;
}

/** Same value? (typed vs plain, number vs decimal string, '' vs null). */
export function sameValue(a: unknown, b: unknown): boolean {
	const n = (x: unknown) => {
		const u = untyped(x);
		if (u === '' || u === undefined) return null;
		if (typeof u === 'number') return String(u);
		if (typeof u === 'string' && u !== '' && !Number.isNaN(Number(u)) && /^-?\d+(\.\d+)?$/.test(u)) return String(Number(u));
		return u;
	};
	return JSON.stringify(n(a)) === JSON.stringify(n(b));
}

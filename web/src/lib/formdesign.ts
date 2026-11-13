// SPDX-License-Identifier: GPL-2.0-or-later
// Form designer data (contract 05 §6.10, §3.6): the field library of a form
// (fields of its tracker, with options), the condition catalog, rule
// sentences and layout bodies.

import type { FieldAdminView, FormDescription, FormDetail, FormField } from '$api/types';
import { api } from './api';
import { CORE_COND, type CondCatalog, catalogOf, condName, describeWhen } from './formcond';
import { fieldLabel } from './format';

export type LibField = { key: string; name: string; type: string; options?: { id: string; label: string }[] | null };
export type Rule = { id: string; field: string; kind: string; when: unknown; values?: unknown[] };
export type LField = { key: string; span?: number | null; help?: string | null; show_if?: unknown };
export type Section = { id: string; title: string; columns: number; show_if?: unknown; fields: LField[] };
export type LayoutBody = { modes: { create: { sections: Section[] }; update?: 'same_as_create' | { sections: Section[] } } };

export const RULE_KIND: Record<string, string> = { required_if: '必填', allowed_values_if: '只能是…', readonly_if: '不能改' };

/** Rule kinds a field can take (forms.rs `parse_rule`). */
export function kindsFor(f: LibField | undefined): string[] {
	if (!f) return [];
	const out = ['readonly_if'];
	if (f.key !== 'subject') out.unshift('required_if');
	if (f.key !== 'subject' && f.key !== 'description' && f.type !== 'text') out.splice(1, 0, 'allowed_values_if');
	return out;
}

export const libName = (lib: LibField[], key: string) => lib.find((f) => f.key === key)?.name ?? fieldLabel(key);

/** 「當 嚴重度 是 低 時，根本原因 必填」. */
export function ruleSentence(r: Rule, lib: LibField[], cat: CondCatalog): string {
	const f = lib.find((x) => x.key === r.field);
	let what = RULE_KIND[r.kind] ?? r.kind;
	if (r.kind === 'allowed_values_if') {
		const vals = (r.values ?? []).map((v) => {
			const u = v && typeof v === 'object' ? Object.values(v as object)[0] : v;
			return f?.options?.find((o) => o.id === u)?.label ?? String(u);
		});
		what = `只能是 ${vals.join('、') || '…'}`;
	}
	return `當 ${describeWhen(r.when, cat)} 時，${libName(lib, r.field)} ${what}`;
}

/** Next free id like r1, r2 / s1, s2. */
export function nextId(prefix: string, used: string[]): string {
	let i = 1;
	while (used.includes(`${prefix}${i}`)) i++;
	return `${prefix}${i}`;
}

/** Deep copy that also works on Svelte $state proxies (structuredClone throws on them). */
const clone = <T>(x: T): T => JSON.parse(JSON.stringify(x));

export function rulesOf(body: unknown): Rule[] {
	const r = (body as { rules?: Rule[] } | null)?.rules;
	return Array.isArray(r) ? clone(r) : [];
}
export function layoutOf(body: unknown, lib: LibField[]): LayoutBody {
	const b = body as LayoutBody | null;
	if (b?.modes?.create?.sections) return clone(b);
	return { modes: { create: { sections: [{ id: 's1', title: '基本', columns: 2, fields: lib.filter((f) => f.key === 'subject' || f.key === 'description').map((f) => ({ key: f.key, span: 2 })) }] }, update: 'same_as_create' } };
}

/** Drops empty optional parts so the body is what the server expects. */
export function cleanLayout(b: LayoutBody): LayoutBody {
	const sec = (s: Section): Section => ({
		id: s.id,
		title: s.title,
		columns: s.columns,
		...(s.show_if ? { show_if: s.show_if } : {}),
		fields: s.fields.map((f) => ({ key: f.key, ...(f.span && f.span > 1 ? { span: f.span } : {}), ...(f.help?.trim() ? { help: f.help.trim() } : {}), ...(f.show_if ? { show_if: f.show_if } : {}) }))
	});
	const u = b.modes.update;
	return { modes: { create: { sections: b.modes.create.sections.map(sec) }, update: !u || u === 'same_as_create' ? 'same_as_create' : { sections: u.sections.map(sec) } } };
}

const fromDescribe = (f: FormField): LibField => ({ key: f.key, name: f.name ?? fieldLabel(f.key), type: f.type, options: f.options?.map((o) => ({ id: o.id, label: o.label })) ?? null });

export type Library = { fields: LibField[]; cat: CondCatalog; restricted: Set<string>; projectId: string | null; describe: FormDescription | null };

/**
 * The fields of the form's tracker. A project form reads its project's
 * form description; a site form reads the first visible project that has
 * the tracker, plus (administrators) every custom field of the tracker.
 */
export async function loadLibrary(form: FormDetail, projectIds: string[], isAdmin: boolean): Promise<Library> {
	const tryIds = form.project_id ? [form.project_id] : projectIds;
	let describe: FormDescription | null = null;
	let projectId: string | null = null;
	for (const pid of tryIds) {
		try {
			describe = await api('GET /projects/{id}/trackers/{t}/form', { path: { id: pid, t: form.tracker_id }, query: { op: 'create' } });
			projectId = pid;
			break;
		} catch {
			/* the tracker is not used there */
		}
	}
	const [admin, ff] = await Promise.all([isAdmin ? api('GET /admin/custom-fields').catch(() => [] as FieldAdminView[]) : Promise.resolve([] as FieldAdminView[]), api('GET /filter-fields', { query: projectId ? { project: projectId } : {} }).catch(() => null)]);
	const fields: LibField[] = (describe?.fields ?? []).filter((f) => f.key !== 'is_private').map(fromDescribe);
	const restricted = new Set<string>();
	for (const c of admin) {
		if (!c.visible) restricted.add(c.key);
		if (form.project_id || c.deactivated || c.index_state === 'dropping' || !c.tracker_ids.includes(form.tracker_id)) continue;
		if (!fields.some((f) => f.key === c.key)) fields.push({ key: c.key, name: c.name, type: c.ftype, options: c.ftype === 'list' ? c.options.filter((o) => o.active).map((o) => ({ id: o.id, label: o.label })) : null });
	}
	for (const f of ff?.fields ?? []) if (f.restricted) restricted.add(f.field);
	const extra: Record<string, { id: string; label: string }[]> = {};
	for (const k of ['tracker', 'status', 'priority', 'assignee', 'fixed_version']) {
		const o = ff?.fields.find((x) => x.field === k)?.options;
		if (o?.length) extra[k] = o.map((x) => ({ id: x.id, label: x.label }));
	}
	const cat = catalogOf(
		fields.filter((f) => !restricted.has(f.key)),
		extra
	);
	return { fields, cat, restricted, projectId, describe };
}

/** Condition fields offered in the editor: core fields and non-restricted custom fields. */
export function condFields(cat: CondCatalog): { key: string; name: string }[] {
	return [...cat.entries()].map(([key, v]) => ({ key, name: v.name }));
}
export { CORE_COND, condName };

// SPDX-License-Identifier: GPL-2.0-or-later
// The form condition language (contract 05 §3.6.2): evaluate `show_if` /
// `required_if` in the browser (§9 B-10), and describe or build conditions
// in the form designer. The evaluator must agree with the server on every
// vector of `api/form-conformance.json`.

import type { FieldSchema } from '$api/types';

export type Cond = { field: string; op: string; value?: unknown };
export type Group = { all: Item[] } | { any: Item[] };
export type Item = Cond | Group;

/** Core condition fields (§3.6.2) and their kinds. */
export const CORE_COND: Record<string, { name: string; kind: Kind }> = {
	tracker: { name: '類型', kind: 'ref' },
	status: { name: '狀態', kind: 'ref' },
	status_category: { name: '狀態類別', kind: 'category' },
	priority: { name: '優先度', kind: 'ref' },
	assignee: { name: '指派對象', kind: 'ref' },
	fixed_version: { name: '里程碑', kind: 'ref' },
	start_date: { name: '開始日', kind: 'date' },
	due_date: { name: '到期日', kind: 'date' },
	estimated_hours: { name: '預估工時', kind: 'number' },
	is_private: { name: '私有', kind: 'bool' }
};
export type Kind = 'ref' | 'category' | 'date' | 'number' | 'bool' | 'text';

/** Command-document field keys → condition field names. */
const COND_NAME: Record<string, string> = { assigned_to_id: 'assignee', priority_id: 'priority', fixed_version_id: 'fixed_version' };
export const condName = (key: string) => COND_NAME[key] ?? key;

/** A custom field type → condition kind. */
export function cfKind(t: string): Kind {
	if (t === 'list' || t === 'user') return 'ref';
	if (t === 'int' || t === 'float') return 'number';
	if (t === 'date') return 'date';
	if (t === 'bool') return 'bool';
	return 'text';
}

export const OPS: Record<Kind, string[]> = {
	ref: ['eq', 'ne', 'in', 'is_set', 'not_set'],
	category: ['eq', 'ne', 'in'],
	date: ['eq', 'ne', 'lt', 'le', 'gt', 'ge', 'is_set', 'not_set'],
	number: ['eq', 'ne', 'lt', 'le', 'gt', 'ge', 'is_set', 'not_set'],
	bool: ['eq', 'ne', 'is_set', 'not_set'],
	text: ['eq', 'ne', 'in', 'is_set', 'not_set']
};
export const OP_LABEL: Record<string, string> = { eq: '是', ne: '不是', in: '是其中之一', is_set: '有值', not_set: '沒有值', lt: '早於／小於', le: '不晚於／不大於', gt: '晚於／大於', ge: '不早於／不小於' };
export const CATEGORY_LABEL: Record<string, string> = { todo: '待辦', doing: '進行中', done: '完成' };

export const isGroup = (x: Item): x is Group => !!x && typeof x === 'object' && ('all' in x || 'any' in x);
export const items = (g: Group): Item[] => ('all' in g ? g.all : g.any);

function unwrap(v: unknown): unknown {
	if (v && typeof v === 'object' && !Array.isArray(v)) {
		const vals = Object.values(v as object);
		return vals.length ? vals[0] : null;
	}
	return v;
}
function tagOf(v: unknown): string | null {
	if (v && typeof v === 'object' && !Array.isArray(v)) return Object.keys(v as object)[0] ?? null;
	return null;
}
function isEmpty(v: unknown): boolean {
	const u = unwrap(v);
	return u === null || u === undefined || u === '';
}

type Cmp = 'id' | 'num' | 'date' | 'text' | 'bool';
function cmpOf(field: string, value: unknown, have: unknown): Cmp {
	const core = CORE_COND[field];
	if (core) return core.kind === 'ref' ? 'id' : core.kind === 'number' ? 'num' : core.kind === 'date' ? 'date' : core.kind === 'bool' ? 'bool' : 'text';
	const t = tagOf(Array.isArray(value) ? value[0] : value) ?? tagOf(have);
	if (t === 'list' || t === 'user') return 'id';
	if (t === 'int' || t === 'float') return 'num';
	if (t === 'date') return 'date';
	if (t === 'bool') return 'bool';
	return 'text';
}
function norm(c: Cmp, v: unknown): string | number | boolean | null {
	const u = unwrap(v);
	if (u === null || u === undefined || u === '') return null;
	if (c === 'id') return String(u).toLowerCase();
	if (c === 'num') {
		const n = Number(u);
		return Number.isFinite(n) ? n : null;
	}
	if (c === 'bool') return u === true || u === 'true';
	return String(u);
}

function holdsCond(c: Cond, values: Record<string, unknown>): boolean {
	const have = values[c.field];
	if (c.op === 'is_set') return !isEmpty(have);
	if (c.op === 'not_set') return isEmpty(have);
	const k = cmpOf(c.field, c.value, have);
	const h = norm(k, have);
	if (c.op === 'in') {
		if (h === null || !Array.isArray(c.value)) return false;
		return c.value.some((x) => norm(k, x) === h);
	}
	const w = norm(k, c.value);
	if (c.op === 'eq') return h !== null && h === w;
	if (c.op === 'ne') return !(h !== null && h === w);
	if (h === null || w === null) return false;
	switch (c.op) {
		case 'lt':
			return h < w;
		case 'le':
			return h <= w;
		case 'gt':
			return h > w;
		case 'ge':
			return h >= w;
	}
	return false;
}

/** Does `when` hold for `values` (keyed by condition field name)? */
export function evalWhen(when: unknown, values: Record<string, unknown>): boolean {
	if (!when || typeof when !== 'object') return true;
	const w = when as Item;
	if (isGroup(w)) {
		const list = items(w);
		return 'all' in w ? list.every((x) => evalWhen(x, values)) : list.some((x) => evalWhen(x, values));
	}
	return holdsCond(w as Cond, values);
}

/** Form values (command-document keys, plain values as FieldInput keeps
 *  them) plus the issue's state → values keyed by condition field name. */
export function condValues(form: Record<string, unknown>, fields: { key: string; type: string }[], state: { tracker?: string | null; status?: string | null; status_category?: string | null; is_private?: boolean | null } = {}): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(state)) if (v !== null && v !== undefined) out[k] = v;
	for (const [k, v] of Object.entries(form)) {
		if (v === null || v === undefined || v === '') continue;
		if (k.startsWith('cf:')) {
			const t = fields.find((f) => f.key === k)?.type ?? 'string';
			out[k] = { [t]: t === 'bool' ? v : String(v) };
		} else out[condName(k)] = typeof v === 'number' ? String(v) : v;
	}
	return out;
}

/** Fields of a form description required now: always, or by a holding
 *  `required_if` rule. */
export function requiredNow(f: { required: boolean; required_if?: { when: unknown }[] }, values: Record<string, unknown>): boolean {
	return f.required || !!f.required_if?.some((r) => evalWhen(r.when, values));
}

// ---- describing conditions (designer, banners) ----

export type CondCatalog = Map<string, { name: string; kind: Kind; tag?: string; options?: { id: string; label: string }[] }>;

function showValue(cat: CondCatalog, field: string, v: unknown): string {
	const info = cat.get(field);
	const u = unwrap(v);
	if (u === null || u === undefined) return '（空）';
	if (field === 'status_category') return CATEGORY_LABEL[String(u)] ?? String(u);
	if (typeof u === 'boolean') return u ? '是' : '否';
	const o = info?.options?.find((x) => x.id.toLowerCase() === String(u).toLowerCase());
	return o ? o.label : String(u);
}

/** 「嚴重度 是 低」、「（狀態類別 是 完成 或 指派對象 有值）」. */
export function describeWhen(when: unknown, cat: CondCatalog, depth = 0): string {
	if (!when || typeof when !== 'object') return '（永遠）';
	const w = when as Item;
	if (isGroup(w)) {
		const list = items(w);
		if (!list.length) return '（永遠）';
		const s = list.map((x) => describeWhen(x, cat, depth + 1)).join('all' in w ? ' 且 ' : ' 或 ');
		return depth > 0 && list.length > 1 ? `（${s}）` : s;
	}
	const c = w as Cond;
	const name = cat.get(c.field)?.name ?? c.field;
	if (c.op === 'is_set' || c.op === 'not_set') return `${name} ${OP_LABEL[c.op]}`;
	const v = Array.isArray(c.value) ? c.value.map((x) => showValue(cat, c.field, x)).join('、') : showValue(cat, c.field, c.value);
	return `${name} ${OP_LABEL[c.op] ?? c.op} ${v}`;
}

/** Wrap a plain value as a condition value for `field`. */
export function wrapValue(cat: CondCatalog, field: string, v: unknown): unknown {
	const tag = cat.get(field)?.tag;
	if (v === null || v === undefined || v === '') return v;
	if (!tag) return v;
	return { [tag]: tag === 'bool' ? v === true || v === 'true' : String(v) };
}

/** Structural limits checked before sending (§3.6.2): two levels, ≤ 5 per group. */
export function whenProblem(when: unknown, depth = 1): string | null {
	if (!when || typeof when !== 'object') return null;
	const w = when as Item;
	if (!isGroup(w)) {
		const c = w as Cond;
		if (!c.field) return '有條件還沒選欄位';
		if (c.op !== 'is_set' && c.op !== 'not_set' && (c.value === undefined || c.value === null || c.value === '' || (Array.isArray(c.value) && !c.value.length))) return '有條件還沒填值';
		return null;
	}
	const list = items(w);
	if (!list.length) return '有一組條件是空的（至少要有一條）';
	if (depth > 2) return '條件最多兩層';
	if (list.length > 5) return '每一組最多 5 條';
	for (const x of list) {
		const p = whenProblem(x, isGroup(x) ? depth + 1 : depth);
		if (p) return p;
	}
	return null;
}

/** The catalog from a form description's fields (condition names, kinds,
 *  options) plus the core fields. Restricted fields never reach a reader. */
export function catalogOf(fields: Pick<FieldSchema, 'key' | 'type' | 'name'>[] & { options?: { id: string; label: string }[] | null }[], extra: Partial<Record<string, { id: string; label: string }[]>> = {}): CondCatalog {
	const cat: CondCatalog = new Map();
	for (const [k, v] of Object.entries(CORE_COND)) cat.set(k, { ...v, options: extra[k] });
	for (const f of fields as ({ key: string; type: string; name?: string | null; options?: { id: string; label: string }[] | null })[]) {
		const n = condName(f.key);
		if (n in CORE_COND) {
			const c = cat.get(n)!;
			if (f.options?.length && !c.options) cat.set(n, { ...c, options: f.options });
			continue;
		}
		if (!f.key.startsWith('cf:')) continue;
		cat.set(f.key, { name: f.name ?? f.key, kind: cfKind(f.type), tag: f.type, options: f.options ?? undefined });
	}
	return cat;
}

/** Layout sections as `describe_form` gives them. */
export type LayoutLike = { sections: { id: string; title: string; columns?: number | null; show_if?: unknown; fields: { key: string; span?: number | null; help?: string | null; show_if?: unknown }[] }[] };

/** Keys the layout hides right now (section or field `show_if` false). */
export function hiddenKeys(layout: LayoutLike | null | undefined, values: Record<string, unknown>): Set<string> {
	const out = new Set<string>();
	for (const s of layout?.sections ?? []) {
		const sec = s.show_if ? evalWhen(s.show_if, values) : true;
		for (const f of s.fields) if (!sec || (f.show_if && !evalWhen(f.show_if, values))) out.add(f.key);
	}
	return out;
}

// SPDX-License-Identifier: GPL-2.0-or-later
// Filter AST v2 (contract 05 §3.2) as the condition editor works on it:
// one group at the top (all / any), conditions and at most one level of
// sub-groups (two levels, §6.2). The server normalizes names to ids; the
// editor sends plain values (ids, `me`, relative dates).

import type { FilterFieldInfo } from '$api/types';
import { untyped } from './render';

export type Cond = { field: string; op: string; value?: unknown };
export type Group = { mode: 'and' | 'or'; items: (Cond | Group)[] };
export type Where = Cond | { and: Where[] } | { or: Where[] } | null;

export const MAX_DEPTH = 2;
export const MAX_CONDS = 30;

export const OP_LABEL: Record<string, string> = {
	eq: '等於',
	ne: '不等於',
	in: '是其中之一',
	not_in: '不是其中之一',
	ever_in: '曾經是',
	lt: '早於',
	le: '不晚於',
	gt: '晚於',
	ge: '不早於',
	between: '介於',
	within: '在…之內',
	is_null: '沒有值',
	not_null: '有值'
};
/** Numbers read better with comparison words than with 早／晚. */
const NUM_OP: Record<string, string> = { lt: '小於', le: '小於等於', gt: '大於', ge: '大於等於' };
export function opLabel(op: string, type?: string): string {
	if (type && ['number', 'int', 'float'].includes(type) && NUM_OP[op]) return NUM_OP[op];
	return OP_LABEL[op] ?? op;
}
export const NO_VALUE = new Set(['is_null', 'not_null']);
export const MULTI = new Set(['in', 'not_in', 'ever_in']);

export const RANGE_LABEL: Record<string, string> = {
	this_week: '本週',
	last_week: '上週',
	next_week: '下週',
	this_month: '本月',
	last_month: '上月',
	last_n_days: '最近 N 天',
	next_n_days: '未來 N 天'
};
export const CATEGORY_LABEL: Record<string, string> = { todo: '待處理', doing: '進行中', done: '完成' };
export const TRIAGE_LABEL: Record<string, string> = {
	pending: '待分流',
	snoozed: '稍後處理',
	accepted: '已接受',
	declined: '已拒絕',
	duplicate: '重複',
	none: '不在 Triage'
};

const isGroupNode = (w: unknown): w is { and: Where[] } | { or: Where[] } =>
	!!w && typeof w === 'object' && ('and' in (w as object) || 'or' in (w as object));

/** Server AST → editor group (the top is always a group). */
export function toGroup(w: unknown): Group {
	if (!w) return { mode: 'and', items: [] };
	if (isGroupNode(w)) {
		const mode = 'and' in w ? 'and' : 'or';
		const list = ((w as Record<string, unknown>)[mode] as unknown[]) ?? [];
		return { mode, items: list.map((x) => (isGroupNode(x) ? toGroup(x) : { ...(x as Cond) })) };
	}
	return { mode: 'and', items: [{ ...(w as Cond) }] };
}

/** Editor group → AST (`null` when empty; a single condition is sent bare). */
export function fromGroup(g: Group): Where {
	const items = g.items
		.map((x) => ('mode' in x ? fromGroup(x) : x))
		.filter((x): x is Exclude<Where, null> => x !== null);
	if (!items.length) return null;
	if (items.length === 1 && g.mode === 'and') return items[0];
	return g.mode === 'and' ? { and: items } : { or: items };
}

export function depth(g: Group): number {
	return 1 + Math.max(0, ...g.items.map((x) => ('mode' in x ? depth(x) : 0)));
}
export function countConds(g: Group): number {
	return g.items.reduce((n, x) => n + ('mode' in x ? countConds(x) : 1), 0);
}

/** Conditions that cannot be sent yet (no value where one is needed). */
export function incomplete(c: Cond): boolean {
	if (!c.field || !c.op) return true;
	if (NO_VALUE.has(c.op)) return false;
	const v = c.value;
	if (v === undefined || v === null || v === '') return true;
	if (Array.isArray(v)) return v.length === 0 || (c.op === 'between' && v.some((x) => x === '' || x == null));
	return false;
}
export function groupIncomplete(g: Group): boolean {
	return g.items.some((x) => ('mode' in x ? groupIncomplete(x) : incomplete(x)));
}

/** "2026-11-13" + n days, in plain calendar arithmetic (the org date is given). */
export function addDays(iso: string, n: number): string {
	const d = new Date(`${iso}T00:00:00Z`);
	d.setUTCDate(d.getUTCDate() + n);
	return d.toISOString().slice(0, 10);
}
const md = (iso: string) => `${Number(iso.slice(5, 7))}/${iso.slice(8, 10)}`;

/** The dates a relative value means today (§6.2: 「最近 7 天（11/03–11/09）」). */
export function rangeDates(v: { range: string; n?: number }, today: string, weekStart = 1): [string, string] | null {
	const dow = ((new Date(`${today}T00:00:00Z`).getUTCDay() + 6) % 7) + 1; // 1 = Monday
	const startOfWeek = addDays(today, -((dow - weekStart + 7) % 7));
	const n = Math.max(1, Math.min(366, v.n ?? 7));
	switch (v.range) {
		case 'this_week':
			return [startOfWeek, addDays(startOfWeek, 6)];
		case 'last_week':
			return [addDays(startOfWeek, -7), addDays(startOfWeek, -1)];
		case 'next_week':
			return [addDays(startOfWeek, 7), addDays(startOfWeek, 13)];
		case 'this_month': {
			const s = `${today.slice(0, 8)}01`;
			const e = new Date(Date.UTC(Number(today.slice(0, 4)), Number(today.slice(5, 7)), 0)).toISOString().slice(0, 10);
			return [s, e];
		}
		case 'last_month': {
			const y = Number(today.slice(0, 4));
			const m = Number(today.slice(5, 7)) - 1; // 0-based this month
			const s = new Date(Date.UTC(y, m - 1, 1)).toISOString().slice(0, 10);
			const e = new Date(Date.UTC(y, m, 0)).toISOString().slice(0, 10);
			return [s, e];
		}
		case 'last_n_days':
			return [addDays(today, -(n - 1)), today];
		case 'next_n_days':
			return [today, addDays(today, n - 1)];
	}
	return null;
}

export type Labeler = (field: string, id: string) => string | undefined;

/** One value as people read it. */
export function valueText(v: unknown, f: FilterFieldInfo | undefined, today: string, label: Labeler, weekStart = 1): string {
	const u = untyped(v);
	if (Array.isArray(u)) return u.map((x) => valueText(x, f, today, label, weekStart)).join('、');
	if (u && typeof u === 'object') {
		const o = u as Record<string, unknown>;
		if (o.me) return '我';
		if (o.next) return '下一個里程碑';
		if (o.rel === 'today') {
			const off = Number(o.offset ?? 0);
			const word = off === 0 ? '今天' : off < 0 ? `${-off} 天前` : `${off} 天後`;
			return `${word}（${md(addDays(today, off))}）`;
		}
		if (typeof o.range === 'string') {
			const name = (RANGE_LABEL[o.range] ?? o.range).replace('N', String(o.n ?? 7));
			const r = rangeDates(o as { range: string; n?: number }, today, weekStart);
			return r ? `${name}（${md(r[0])}–${md(r[1])}）` : name;
		}
	}
	if (u === 'me') return '我';
	if (typeof u === 'boolean') return u ? '是' : '否';
	if (typeof u === 'string') {
		if (f?.field === 'status_category') return CATEGORY_LABEL[u] ?? u;
		if (f?.field === 'triage') return TRIAGE_LABEL[u] ?? u;
		const o = f?.options?.find((x) => x.id === u || x.key === u);
		if (o) return o.label;
		return (f && label(f.field, u)) ?? u;
	}
	return u == null ? '—' : String(u);
}

/** 「狀態：待處理、進行中」 for a condition chip. */
export function condText(c: Cond, fields: FilterFieldInfo[], today: string, label: Labeler, weekStart = 1): string {
	const f = fields.find((x) => x.field === c.field);
	const name = f?.label ?? c.field;
	if (NO_VALUE.has(c.op)) return `${name}${opLabel(c.op, f?.type)}`;
	const val = c.op === 'between' && Array.isArray(c.value) ? (c.value as unknown[]).map((x) => valueText(x, f, today, label, weekStart)).join(' 到 ') : valueText(c.value, f, today, label, weekStart);
	if (c.op === 'eq' || c.op === 'in') return `${name}：${val}`;
	return `${name} ${opLabel(c.op, f?.type)} ${val}`;
}

/** Chips for the top level: each condition, or 「（任一：…）」 for a sub-group. */
export function chips(g: Group, fields: FilterFieldInfo[], today: string, label: Labeler, weekStart = 1): string[] {
	const one = (x: Cond | Group): string =>
		'mode' in x ? `（${x.mode === 'and' ? '全部' : '任一'}：${x.items.map(one).join('；')}）` : condText(x, fields, today, label, weekStart);
	if (g.mode === 'or' && g.items.length > 1) return [`任一：${g.items.map(one).join('；')}`];
	return g.items.map(one);
}

/** The default value when a field or op changes. */
export function defaultValue(f: FilterFieldInfo | undefined, op: string, today: string): unknown {
	if (!f || NO_VALUE.has(op)) return undefined;
	if (op === 'within') return { range: 'last_n_days', n: 7 };
	if (op === 'between') return [today, today];
	if (MULTI.has(op)) return [];
	if (f.type === 'bool') return true;
	if (['date', 'datetime'].includes(f.type)) return today;
	return '';
}

/** One cell of a view run (`values` / `card`): people, statuses and options are objects. */
export function cellText(v: unknown): string {
	if (v === null || v === undefined || v === '') return '—';
	if (typeof v === 'boolean') return v ? '是' : '否';
	if (Array.isArray(v)) return v.map(cellText).join('、');
	if (typeof v === 'object') {
		const o = v as Record<string, unknown>;
		if (typeof o.name === 'string') return o.kind === 'agent' ? `${o.name}〔agent〕` : o.kind === 'group' ? `${o.name}（群組）` : o.name;
		if (typeof o.label === 'string') return o.active === false ? `${o.label}（停用）` : o.label;
		if (typeof o.key === 'string') return o.key;
		return JSON.stringify(untyped(v));
	}
	if (typeof v === 'string' && /^\d{4}-\d{2}-\d{2}T/.test(v)) {
		return new Date(v).toLocaleString('zh-TW', { timeZone: 'Asia/Taipei', month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false });
	}
	return String(v);
}
/** 「6」「999+」「—」 for counts (R-VD6). */
export function countText(c: { value: number; capped: boolean } | null | undefined): string {
	if (!c) return '—';
	return c.capped ? `${c.value}+` : String(c.value);
}

/** The structured part of an issue search as a view's `where` (「存成視圖」).
 *  Values stay as typed (names, `me`); the server resolves them and answers
 *  `invalid_view` for one it does not know. `open` is todo + doing. */
export function searchToWhere(
	p: { tracker?: string | null; status?: string | null; status_category?: string | null; assignee?: string | null; version?: string | null; overdue?: boolean | null },
	projectId?: string | null
): Where {
	const c: Cond[] = [];
	if (projectId) c.push({ field: 'project', op: 'eq', value: projectId });
	if (p.tracker) c.push({ field: 'tracker', op: 'eq', value: p.tracker });
	if (p.status) c.push({ field: 'status', op: 'eq', value: p.status });
	if (p.status_category) c.push(p.status_category === 'open' ? { field: 'status_category', op: 'in', value: ['todo', 'doing'] } : { field: 'status_category', op: 'eq', value: p.status_category });
	if (p.assignee) c.push({ field: 'assignee', op: 'eq', value: p.assignee });
	if (p.version) c.push({ field: 'fixed_version', op: 'eq', value: p.version });
	if (p.overdue) c.push({ field: 'overdue', op: 'eq', value: true });
	return c.length === 0 ? null : c.length === 1 ? c[0] : { and: c };
}

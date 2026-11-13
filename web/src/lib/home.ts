// SPDX-License-Identifier: GPL-2.0-or-later
// Home widgets (contract 05 §3.8, §6.12): titles, editing moves and the body
// `PUT /me/home` takes.

import type { Widget, WidgetInput } from '$api/types';

export const MAX_WIDGETS = 12;
export const PROTECTED = 'need_decision';

export const WIDGET_LABEL: Record<string, string> = {
	need_decision: '需要你決定',
	view: '視圖',
	triage_count: 'Triage 計數',
	milestones: '里程碑',
	my_time_week: '本週工時',
	agent_summary: 'Agent 摘要'
};
/** Types the 「＋ 小工具」 list offers (need_decision is there exactly once). */
export const ADDABLE = ['view', 'triage_count', 'milestones', 'my_time_week', 'agent_summary'];

/** Built-in views a template names by id only (templates carry no names). */
export const BUILTIN_VIEW: Record<string, string> = { 'builtin:assigned_to_me': '指派給我', 'builtin:involving_me': '與我相關', 'builtin:reported_by_me': '我回報的' };

export function widgetTitle(w: Widget): string {
	if (w.type === 'view') return w.view_name ?? BUILTIN_VIEW[w.view_id ?? ''] ?? WIDGET_LABEL.view;
	if (w.type === 'milestones' && w.project_key) return `里程碑・${w.project_key}`;
	return WIDGET_LABEL[w.type] ?? w.type;
}

/** Only what the server accepts for each type (others answer unexpected_property). */
export function toInput(w: Widget): WidgetInput {
	const out: WidgetInput = { id: w.id, type: w.type, width: w.width };
	if (w.type === 'view') {
		out.view_id = w.view_id;
		out.limit = w.limit ?? 8;
	}
	if (w.type === 'milestones' && w.project_id) out.project_id = w.project_id;
	return out;
}

/** Moves item `from` to position `to` (drag and drop, or ↑↓). */
export function moveTo<T>(list: T[], from: number, to: number): T[] {
	if (from === to || from < 0 || to < 0 || from >= list.length || to >= list.length) return list;
	const out = [...list];
	const [x] = out.splice(from, 1);
	out.splice(to, 0, x);
	return out;
}

export function newWidget(type: string, extra: Partial<Widget> = {}): Widget {
	const id = `w_${type}_${Math.random().toString(36).slice(2, 8)}`;
	const base: Widget = { id, type, width: 1, available: true, protected: false };
	if (type === 'view') base.limit = 8;
	return { ...base, ...extra };
}

/** Problems the editor shows before saving (the server checks again). */
export function homeProblem(ws: Widget[]): string {
	if (ws.filter((w) => w.type === PROTECTED).length !== 1) return '「需要你決定」必須恰好一個';
	if (ws.length > MAX_WIDGETS) return `最多 ${MAX_WIDGETS} 個小工具`;
	for (const w of ws) {
		if (w.width < 1 || w.width > 3) return '寬度是 1 到 3 欄';
		if (w.type === 'view' && !w.view_id) return '視圖小工具要選一個視圖';
		if (w.type === 'view' && (w.limit ?? 8) < 3) return '視圖小工具至少列 3 筆';
		if (w.type === 'view' && (w.limit ?? 8) > 20) return '視圖小工具最多列 20 筆';
	}
	return '';
}

/** The generic `*` (no interface role chosen) would hide every project's own default tab; only role templates and per-project choices count. */
export function myDefaults<T>(p: { default_views: Record<string, T>; interface_roles: string[] } | null): Record<string, T> {
	const d = { ...(p?.default_views ?? {}) };
	if (!p?.interface_roles.length) delete d['*'];
	return d;
}

const BUILTIN_TAB: Record<string, string> = { 'builtin:board': 'board', 'builtin:list': 'issues', 'builtin:timeline': 'timeline' };
/**
 * The project tab a person's default view points at (`default_views` keyed by
 * project id, or `*` for every project); null when no visible tab shows it.
 */
export function personalTab(defaults: Record<string, { view_id: string; available?: boolean | null }>, projectId: string, tabs: { id: string; view_id?: string | null; hidden?: boolean }[]): string | null {
	const d = defaults[projectId] ?? defaults['*'];
	if (!d || d.available === false) return null;
	const shown = tabs.filter((t) => !t.hidden);
	const want = BUILTIN_TAB[d.view_id];
	const t = want ? shown.find((x) => x.id === want) : shown.find((x) => x.view_id === d.view_id);
	return t?.id ?? null;
}

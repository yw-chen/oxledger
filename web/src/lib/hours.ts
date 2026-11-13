// SPDX-License-Identifier: GPL-2.0-or-later
// Time input (contract 05 §6.6): people type 1.5, 1:30 or 1h30m; the wire
// takes a decimal string with two decimals ("1.50", §3.10.2).

import type { CommandDoc, OpInput, TimeEntrySet, TimeFields } from '$api/types';
import { newOperationId } from './api';

/** "1.5" | "1:30" | "1h30m" | "90m" | "2h" → "1.50"; null when it is not hours. */
export function parseHours(input: string): string | null {
	const s = input.trim().toLowerCase().replace(/\s+/g, '').replace(/[，,]/g, '.').replace('：', ':');
	if (!s) return null;
	let minutes: number;
	let m: RegExpExecArray | null;
	// Decimal hours, exact (no float rounding): "1.5", ".5", "1.5h".
	if ((m = /^(\d{0,4})(?:\.(\d{1,2}))?h?$/.exec(s)) && (m[1] || m[2])) {
		const frac = (m[2] ?? '').padEnd(2, '0');
		return `${Number(m[1] || 0)}.${frac}`;
	} else if ((m = /^(\d{1,4}):([0-5]?\d)$/.exec(s))) {
		minutes = Number(m[1]) * 60 + Number(m[2]);
	} else if ((m = /^(?:(\d{1,4})h)?(?:(\d{1,4})m(?:in)?)?$/.exec(s)) && (m[1] || m[2])) {
		minutes = Number(m[1] ?? 0) * 60 + Number(m[2] ?? 0);
	} else {
		return null;
	}
	// Minutes → hours with two decimals, rounded half up (1:20 → 1.33).
	const cents = Math.round((minutes * 100) / 60);
	return `${Math.floor(cents / 100)}.${String(cents % 100).padStart(2, '0')}`;
}

/** "1.50" → "1.5"; "8.00" → "8"; null/"" → "". */
export function showHours(h: string | null | undefined): string {
	if (h === null || h === undefined || h === '') return '';
	const n = Number(h);
	if (!Number.isFinite(n)) return h;
	return String(Math.round(n * 100) / 100);
}

/** Sum of decimal strings, exact to the cent. */
export function addHours(...hs: (string | null | undefined)[]): string {
	let cents = 0;
	for (const h of hs) if (h) cents += Math.round(Number(h) * 100);
	return `${Math.floor(cents / 100)}.${String(cents % 100).padStart(2, '0')}`;
}

// ------------------------------------------------------------- dates and weeks

const pad = (n: number) => String(n).padStart(2, '0');
const ymd = (d: Date) => `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
const parse = (s: string) => new Date(`${s}T00:00:00Z`);

export function addDays(day: string, n: number): string {
	const d = parse(day);
	d.setUTCDate(d.getUTCDate() + n);
	return ymd(d);
}

/** ISO week of a YYYY-MM-DD day: "2026-W46". */
export function isoWeek(day: string): string {
	const d = parse(day);
	const wd = d.getUTCDay() || 7;
	d.setUTCDate(d.getUTCDate() + 4 - wd); // the week's Thursday decides its year
	const y = d.getUTCFullYear();
	const w = Math.ceil(((d.getTime() - Date.UTC(y, 0, 1)) / 86400000 + 1) / 7);
	return `${y}-W${pad(w)}`;
}

/** The Monday of an ISO week. */
export function weekMonday(week: string): string {
	const m = /^(\d{4})-W(\d{2})$/.exec(week);
	if (!m) throw new Error(`not an ISO week: ${week}`);
	const jan4 = new Date(Date.UTC(Number(m[1]), 0, 4));
	const wd = jan4.getUTCDay() || 7;
	jan4.setUTCDate(jan4.getUTCDate() - wd + 1 + (Number(m[2]) - 1) * 7);
	return ymd(jan4);
}

export function shiftWeek(week: string, n: number): string {
	return isoWeek(addDays(weekMonday(week), n * 7));
}

/** Today in the organization's zone (F11 replaces the fixed zone). */
export function todayLocal(): string {
	return new Date().toLocaleDateString('sv-SE', { timeZone: 'Asia/Taipei' });
}

/** Report periods (§6.8). `week_start`: 1 = Monday … 7 = Sunday. */
export function period(kind: 'this_week' | 'last_week' | 'this_month' | 'last_month', today: string, weekStart = 1): { from: string; to: string } {
	const d = parse(today);
	if (kind === 'this_week' || kind === 'last_week') {
		const wd = d.getUTCDay() || 7; // 1..7
		const back = (wd - weekStart + 7) % 7;
		const from = addDays(today, -back - (kind === 'last_week' ? 7 : 0));
		return { from, to: addDays(from, 6) };
	}
	const y = d.getUTCFullYear();
	const mo = d.getUTCMonth() - (kind === 'last_month' ? 1 : 0);
	const first = new Date(Date.UTC(y, mo, 1));
	const last = new Date(Date.UTC(y, mo + 1, 0));
	return { from: ymd(first), to: ymd(last) };
}

const WD = ['日', '一', '二', '三', '四', '五', '六'];
/** "11/9（一）" */
export function dayLabel(day: string): string {
	const d = parse(day);
	return `${d.getUTCMonth() + 1}/${d.getUTCDate()}（${WD[d.getUTCDay()]}）`;
}

/** Snooze presets (§6.4), in the browser's own clock, as RFC 3339. */
export function snoozePreset(kind: 'tomorrow' | 'monday', now = new Date()): string {
	const t = new Date(now);
	t.setHours(9, 0, 0, 0);
	if (kind === 'tomorrow') t.setDate(t.getDate() + 1);
	else {
		const add = ((8 - t.getDay()) % 7) || 7; // next Monday, never today
		t.setDate(t.getDate() + add);
	}
	return t.toISOString();
}

// ------------------------------------------------------------- command documents


export type LogInput = {
	issueKey?: string | null;
	projectId?: string | null;
	spent_on: string;
	hours: string;
	activity_id?: string | null;
	comments?: string | null;
	user_id?: string | null;
};

/** The `log_time` op (§3.10.2): it may ride along an issue document. */
export function logTimeOp(i: LogInput): OpInput {
	const fields: TimeFields = { spent_on: i.spent_on, hours: i.hours };
	if (i.activity_id) fields.activity_id = i.activity_id;
	if (i.comments?.trim()) fields.comments = i.comments.trim();
	if (i.user_id) fields.user_id = i.user_id;
	return {
		kind: 'log_time',
		op_id: newOperationId(),
		...(i.issueKey ? { issue: { type: 'issue', key: i.issueKey } } : {}),
		...(i.projectId && !i.issueKey ? { project_id: i.projectId } : {}),
		fields
	};
}

/** `log_time` alone needs no precondition. */
export function logTimeDoc(i: LogInput): CommandDoc {
	return { command_schema_version: 1, ops: [logTimeOp(i)], preconditions: [], title: `記工時 ${i.hours} 小時${i.issueKey ? `・${i.issueKey}` : ''}` };
}

/** update / delete: one op alone, with the entry's lock_version (§3.10.2). */
export function updateTimeDoc(id: string, lock: number, set: TimeEntrySet): CommandDoc {
	return {
		command_schema_version: 1,
		ops: [{ kind: 'update_time_entry', op_id: newOperationId(), target: { type: 'time_entry', id }, set }],
		preconditions: [{ target: { type: 'time_entry', id }, lock_version: lock }],
		title: '修改工時'
	};
}
export function deleteTimeDoc(id: string, lock: number): CommandDoc {
	return {
		command_schema_version: 1,
		ops: [{ kind: 'delete_time_entry', op_id: newOperationId(), target: { type: 'time_entry', id } }],
		preconditions: [{ target: { type: 'time_entry', id }, lock_version: lock }],
		title: '刪除工時'
	};
}

// SPDX-License-Identifier: GPL-2.0-or-later
// 待審佇列 (contract 05 §6.14, 02 §15.3): readable lines for the diff rows
// that are not plain fields (`time`, `triage`), the reasons a row cannot be
// ticked, and which new operations an existing delegation could add (§4.3).
// Pure functions, unit-tested in review.test.ts.

import type { DiffRow, PolicyMode } from '$api/types';
import { fmtShort, fmtValue } from './format';

/** Decimal hours with two decimals, exact for decimal strings ("1.5" → "1.50"). */
export function twoDecimals(h: unknown): string {
	if (typeof h === 'number' && Number.isFinite(h)) return h.toFixed(2);
	if (typeof h !== 'string') return '';
	const m = /^(\d+)(?:\.(\d*))?$/.exec(h.trim());
	if (!m) return h;
	return `${Number(m[1])}.${(m[2] ?? '').padEnd(2, '0').slice(0, 2)}`;
}

type TimeAfter = { hours?: unknown; spent_on?: unknown; activity?: unknown; comments?: unknown };

/** 「記工時 1.50 小時（2026-11-12，開發）」 from a `time` row's `after` (review_queue.rs `time_row`). */
export function timeText(after: unknown): string {
	const a = (after ?? {}) as TimeAfter;
	const hours = twoDecimals(a.hours);
	const parts: string[] = [];
	if (typeof a.spent_on === 'string' && a.spent_on) parts.push(a.spent_on);
	const act = a.activity == null ? '' : fmtValue(a.activity);
	if (act) parts.push(act);
	return `記工時 ${hours || '？'} 小時${parts.length ? `（${parts.join('，')}）` : ''}`;
}

/** The time entry's comment, shortened for the list (the full text is on the detail page). */
export function timeComment(after: unknown, max = 80): string {
	const c = (after as TimeAfter | null)?.comments;
	if (typeof c !== 'string' || !c.trim()) return '';
	const t = c.trim();
	return [...t].length > max ? `${[...t].slice(0, max).join('')}…` : t;
}

export const TRIAGE_DECISION: Record<string, string> = {
	accept: '接受',
	decline: '拒絕',
	duplicate: '標為重複',
	snooze: '稍後處理',
	unsnooze: '取消稍後'
};

type TriageAfter = { decision?: string; snooze_until?: string | null; duplicate_of?: string | null };

/** 「Triage：接受，指派給 Bob」 from a `triage` row's `after` and, for accept, the assignee proposed with it. */
export function triageText(after: unknown, assignee?: unknown): string {
	const a = (after ?? {}) as TriageAfter;
	const dec = a.decision ?? '';
	let t = `Triage：${TRIAGE_DECISION[dec] ?? (dec || '？')}`;
	if (dec === 'snooze' && a.snooze_until) t += `（到 ${fmtShort(a.snooze_until)}）`;
	if (assignee != null && assignee !== '') t += `，指派給 ${fmtValue(assignee)}`;
	return t;
}

/** One line of the queue's diff table: a plain field row, or a summary line (time, triage, note). */
export type DiffLine =
	| { kind: 'field'; row: DiffRow }
	| { kind: 'summary'; row: DiffRow; text: string; detail?: string };

/**
 * The diff rows as the queue shows them (§6.14 「工時與 Triage 提案的摘要列」):
 * time and triage rows become one readable line; an accept's assignment on the
 * same issue is folded into the triage line instead of its own row.
 */
export function diffLines(diff: DiffRow[]): DiffLine[] {
	const acceptOn = new Set(diff.filter((d) => d.field === 'triage' && (d.after as TriageAfter | null)?.decision === 'accept').map((d) => d.target.id));
	const assignee = new Map<string, unknown>();
	for (const d of diff) if (d.field === 'assigned_to_id' && acceptOn.has(d.target.id) && d.after != null) assignee.set(d.target.id, d.after);
	const out: DiffLine[] = [];
	for (const d of diff) {
		if (d.field === 'time') {
			const c = timeComment(d.after);
			out.push({ kind: 'summary', row: d, text: timeText(d.after), ...(c ? { detail: `說明：${c}` } : {}) });
		} else if (d.field === 'triage') {
			out.push({ kind: 'summary', row: d, text: triageText(d.after, assignee.get(d.target.id)) });
		} else if (d.field === 'note') {
			out.push({ kind: 'summary', row: d, text: `新增${(d.after as { is_private?: boolean } | null)?.is_private ? '私有 ' : ' '}note（內容請打開看）` });
		} else if (d.field === 'assigned_to_id' && assignee.has(d.target.id)) {
			continue;
		} else {
			out.push({ kind: 'field', row: d });
		}
	}
	return out;
}

/** §3.12.4 `agent_chain` (`depth` or `writes`; no counts are shown). */
export const AGENT_CHAIN_REASON = 'agent 連續寫入上限';
/** 「agent 連續寫入上限（換手過多）」／「（寫入次數過多）」, never with numbers (§3.12.4, appendix D V21). */
export function agentChainReason(v: string | null | undefined): string {
	const why = v === 'depth' ? '（換手過多）' : v === 'writes' ? '（寫入次數過多）' : '';
	return AGENT_CHAIN_REASON + why;
}
export const AGENT_CHAIN_TIP = '多個 agent 在這張議題上連續寫入，請逐份確認';
export const RULES_CHANGED = '規則已變，需要重新檢視';

type QueueRow = {
	state: string;
	agent_chain?: string | null;
	rules_changed: boolean;
	warnings: string[];
	validation: { status?: string | null } | null;
};

/** Why a proposed row cannot be ticked for batch approval ('' when it is not proposed). */
export function whyNotQueue(c: QueueRow, warningLabel: (w: string) => string = (w) => w): string {
	if (c.state !== 'proposed') return '';
	if (c.agent_chain) return AGENT_CHAIN_TIP;
	if (c.rules_changed) return `${RULES_CHANGED}：請打開看`;
	if (c.warnings.length) return c.warnings.map(warningLabel).join('、') + '：請打開看';
	if (c.validation?.status && c.validation.status !== 'passed') return c.validation.status === 'conflict' ? '有衝突：請打開處理' : '檢查沒通過：請打開看';
	return '內容需要打開看（長文、note、建立、關係或私有設定）';
}

/** A row the client never lets you tick, whatever `review_modes` says. */
export const neverTickable = (c: Pick<QueueRow, 'agent_chain' | 'rules_changed'>) => !!c.agent_chain || c.rules_changed;

// ---------------------------------------------------------------- new operations (§4.3)

/** §4.3: the mode a newly added operation starts with (time commands need approval, A1(b)). */
export function defaultModeFor(op: string): PolicyMode {
	return op === 'log_time' || op === 'update_time_entry' ? 'review' : 'direct';
}

/** Operations and fields the delegation does not have yet (rule 1: never added by themselves). */
export function newOperations(p: { commands: Record<string, PolicyMode>; fields: Record<string, PolicyMode>; available_commands: string[]; available_fields: string[] }): { commands: string[]; fields: string[] } {
	return {
		commands: p.available_commands.filter((c) => !(c in p.commands)),
		fields: p.available_fields.filter((f) => !(f in p.fields))
	};
}

/** The `PUT /delegations/{id}/policies` body that adds them with their default modes, keeping the rest. */
export function withNewOperations(
	cur: { commands: Record<string, PolicyMode>; fields: Record<string, PolicyMode> },
	add: { commands: string[]; fields: string[] }
): { commands: Record<string, PolicyMode>; fields: Record<string, PolicyMode> } {
	const commands = { ...cur.commands };
	for (const c of add.commands) commands[c] = defaultModeFor(c);
	const fields = { ...cur.fields };
	for (const f of add.fields) fields[f] = 'direct';
	return { commands, fields };
}

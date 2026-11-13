// SPDX-License-Identifier: GPL-2.0-or-later
// Notification wording (contract 05 §6.15). Pure functions; the bell and the
// notification centre both use them.

import type { NotificationEvent, NotificationItem, UnreadCount } from '$api/types';
import { fieldLabel, fmtValue } from './format';

/** The five fixed reason labels (§6.15 文案). */
export const REASON: Record<string, string> = {
	watching: '關注中',
	assigned: '指派給你',
	author: '你建立的',
	agent_on_behalf: '你的 agent',
	triage_decided: '分流結果'
};
export const reasonLabels = (rs: string[]) => rs.map((r) => REASON[r]).filter(Boolean);

const TRIAGE: Record<string, string> = { pending: '待分流', snoozed: '稍後處理', accepted: '接受', declined: '拒絕', duplicate: '重複' };

/** 「claude-code@bob〔agent〕」; people as they are. */
export function actorName(a: NotificationEvent['actor']): string {
	return a.kind === 'agent' ? `${a.name}〔agent〕` : a.name;
}

const label = (k: string) => (k.startsWith('cf:') ? '自訂欄位' : fieldLabel(k));
/** Only short plain values are worth quoting; ids and objects are not. */
function plain(v: unknown): string | null {
	if (v === null || v === undefined || v === '') return '（空）';
	if (typeof v === 'object' && !Array.isArray(v)) {
		const o = v as Record<string, unknown>;
		if (typeof o.name === 'string' || typeof o.label === 'string') return fmtValue(v);
		return null;
	}
	const s = fmtValue(v);
	return s.length > 40 || /^[0-9a-f]{8}-[0-9a-f]{4}-/.test(s) ? null : s;
}

/** One line: 「Carol Hsu 留言：已在 Safari 17 重現…」. */
export function eventLine(ev: NotificationEvent): string {
	// 「Carol Hsu 留言」but 「claude-code@bob〔agent〕把…」 (the bracket already separates).
	const who = actorName(ev.actor) + (ev.actor.kind === "agent" ? "" : " ");
	const s = (ev.summary ?? {}) as Record<string, unknown>;
	if (ev.redacted) return `${who}做了一項變更（內容已遮蔽）`;
	switch (ev.kind) {
		case 'note_added': {
			if (s.retracted) return `${who}留了一則 note（已撤回）`;
			if (s.redacted) return `${who}留了一則 note（內容已遮蔽）`;
			const ex = typeof s.note_excerpt === 'string' ? s.note_excerpt : '';
			return `${who}${s.private ? '留了私有 note' : '留言'}${ex ? `：${ex}` : ''}`;
		}
		case 'status_changed':
			return `${who}把狀態從 ${fmtValue(s.old)} 改成 ${fmtValue(s.new)}`;
		case 'fields_changed': {
			const ch = (s.changes ?? {}) as Record<string, { old?: unknown; new?: unknown }>;
			const keys = Object.keys(ch);
			if (keys.length === 1) {
				const k = keys[0];
				const o = plain(ch[k]?.old);
				const n = plain(ch[k]?.new);
				if (o !== null && n !== null) return `${who}把${label(k)}從 ${o} 改成 ${n}`;
			}
			return `${who}修改了${keys.map(label).join('、') || '欄位'}`;
		}
		case 'created':
			return `${who}建立了這張單`;
		case 'triage_changed':
			return `${who}分流：${TRIAGE[String(s.new)] ?? s.new}`;
		case 'duplicate_added':
			return `${who}標為和 ${s.other_key ?? '另一張單'} 重複`;
		case 'duplicate_removed':
			return `${who}取消和 ${s.other_key ?? '另一張單'} 的重複`;
		default:
			return `${who}更新了這張單`;
	}
}

/** 「還有 2 則更新」: events past the first one. */
export const moreCount = (n: NotificationItem) => Math.max(0, n.events.length - 1) + (n.more_events ?? 0);

/** The bell badge: nothing for 0 or an unknown count (fingerprint changed, §3.11.3). */
export function badge(c: UnreadCount | null): string {
	if (!c || c.count === null || c.count <= 0) return '';
	return c.capped || c.count > 99 ? '99+' : String(c.count);
}
export function bellLabel(c: UnreadCount | null): string {
	const b = badge(c);
	return b ? `通知，${b} 則未讀` : '通知';
}

/** 「AI 本月用量已達 80%（$80.12／$100.00）」. */
export function budgetLine(n: NotificationItem): string {
	const b = n.ai_budget;
	if (!b) return 'AI 用量提醒';
	return `AI 本月用量已達 ${b.threshold}%（$${b.spent_usd}${b.budget_usd ? `／$${b.budget_usd}` : ''}）`;
}

/** 「3 分鐘前」 style, falling back to the date. */
export function ago(iso: string, now = Date.now()): string {
	const t = new Date(iso).getTime();
	if (Number.isNaN(t)) return iso;
	const s = Math.max(0, Math.round((now - t) / 1000));
	if (s < 60) return '剛剛';
	if (s < 3600) return `${Math.floor(s / 60)} 分鐘前`;
	if (s < 86400) return `${Math.floor(s / 3600)} 小時前`;
	if (s < 86400 * 7) return `${Math.floor(s / 86400)} 天前`;
	return new Date(iso).toLocaleDateString('zh-TW', { timeZone: 'Asia/Taipei' });
}

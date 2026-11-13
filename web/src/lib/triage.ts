// SPDX-License-Identifier: GPL-2.0-or-later
// Triage decisions (contract 05 §3.10.1, §6.4): one command document per
// decision, `triage_decide` first, then what the decision may carry.

import type { CommandDoc, OpInput, TriageDecision } from '$api/types';
import { api, newOperationId } from './api';
import { doc, issueRef } from './commands';

export type Decision = {
	decision: TriageDecision;
	/** accept: assign to (user/group id); '' or undefined = leave as is. */
	assignee?: string | null;
	/** accept (optional), decline and duplicate (required): status to move to. */
	toStatus?: string | null;
	note?: string | null;
	/** duplicate: the original and the lock_version it was read at. */
	dupOf?: { key: string; lock: number } | null;
	/** snooze: RFC 3339. */
	snoozeUntil?: string | null;
};

export const DECLINE_PREFIX = '（Triage 拒絕）';
export const dupNote = (key: string) => `和 ${key} 重複。`;

const TITLE: Record<TriageDecision, string> = {
	accept: '接受',
	decline: '拒絕',
	duplicate: '標為重複',
	snooze: '稍後處理',
	unsnooze: '取消稍後'
};

/** The note counts only when the person wrote more than the preset prefix. */
export function noteText(note: string | null | undefined): string | null {
	const t = (note ?? '').trim();
	if (!t || t === DECLINE_PREFIX) return null;
	return t;
}

export function triageDoc(key: string, lock: number, d: Decision): CommandDoc {
	const target = issueRef(key);
	const ops: OpInput[] = [
		{
			kind: 'triage_decide',
			op_id: newOperationId(),
			target,
			decision: d.decision,
			...(d.decision === 'duplicate' && d.dupOf ? { duplicate_of: issueRef(d.dupOf.key) } : {}),
			...(d.decision === 'snooze' ? { snooze_until: d.snoozeUntil ?? null } : {})
		}
	];
	if (d.decision === 'accept' && d.assignee) {
		ops.push({ kind: 'update_fields', op_id: newOperationId(), target, set: { assigned_to_id: d.assignee } });
	}
	if ((d.decision === 'accept' || d.decision === 'decline' || d.decision === 'duplicate') && d.toStatus) {
		ops.push({ kind: 'transition', op_id: newOperationId(), target, to_status_id: d.toStatus });
	}
	const note = noteText(d.note);
	if (note) ops.push({ kind: 'add_note', op_id: newOperationId(), target, body: note, is_private: false });
	const pre = [{ key, lock }];
	if (d.decision === 'duplicate' && d.dupOf) pre.push({ key: d.dupOf.key, lock: d.dupOf.lock });
	const title = d.decision === 'duplicate' && d.dupOf ? `${key} 標為 ${d.dupOf.key} 的重複` : `${key} 分流：${TITLE[d.decision]}`;
	return doc(ops, pre, title);
}

/** Status id → category (todo | doing | done), from the filter fields. */
let statusCats: Promise<Map<string, { label: string; category: string }>> | null = null;
export function statusCategories() {
	if (!statusCats) {
		statusCats = api('GET /filter-fields')
			.then((ff) => {
				const m = new Map<string, { label: string; category: string }>();
				for (const o of ff.fields.find((f) => f.field === 'status')?.options ?? []) m.set(o.id, { label: o.label, category: o.category ?? '' });
				return m;
			})
			.catch((e) => {
				statusCats = null;
				throw e;
			});
	}
	return statusCats;
}

/** "3 小時前" */
export function ago(at: string, now = Date.now()): string {
	const s = Math.max(0, Math.round((now - new Date(at).getTime()) / 1000));
	if (s < 60) return '剛剛';
	if (s < 3600) return `${Math.floor(s / 60)} 分鐘前`;
	if (s < 86400) return `${Math.floor(s / 3600)} 小時前`;
	if (s < 86400 * 30) return `${Math.floor(s / 86400)} 天前`;
	return new Date(at).toLocaleDateString('zh-TW', { timeZone: 'Asia/Taipei' });
}

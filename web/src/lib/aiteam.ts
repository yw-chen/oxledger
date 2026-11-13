// SPDX-License-Identifier: GPL-2.0-or-later
// AI 團隊 (contract 05 §3.12, §6.18): labels, the policy table's helpers and
// the fixed texts. Pure functions, unit-tested in aiteam.test.ts.

import type { PolicyMode, SharedRuntimeWarning } from '$api/types';
import { COMMAND_LABELS } from './format';

export const TEMPLATE_LABEL: Record<string, string> = {
	triage: '分流',
	pm_assistant: 'PM 助理',
	design_review: '設計審查',
	qa: 'QA',
	release: '發布管家',
	personal: '個人 agent',
	custom: '自訂',
	builtin_chat: '內建 AI 助理'
};
export const TEMPLATE_DUTY: Record<string, string> = {
	triage: '分流新單、補優先度、標重複',
	pm_assistant: '整理需求、起草新單與描述',
	design_review: '在議題上留下審查意見',
	qa: '回報測試結果、建議轉狀態',
	release: '整理里程碑與發布內容',
	personal: '以你的名義處理日常工作',
	custom: '從全部不允許開始，自己決定'
};
/** The wizard's template cards (`personal` is the 02 path; not offered for a new role). */
export const WIZARD_TEMPLATES = ['triage', 'pm_assistant', 'design_review', 'qa', 'release', 'custom'];

export const STATUS_LABEL: Record<string, { t: string; c: string; tip?: string }> = {
	active: { t: '運作中', c: 'c-ok' },
	paused: { t: '已暫停', c: 'c-warn' },
	quarantined: { t: '已隔離', c: 'c-bad', tip: '這個角色所在的執行環境有專案權限改變，要由 sponsor 建新工作脈絡' },
	retired: { t: '已退役', c: 'c-dim' }
};

const MORE_COMMANDS: Record<string, string> = {
	triage_decide: 'Triage 決定',
	log_time: '記工時',
	update_time_entry: '修改工時'
};
export const commandLabel = (op: string) => COMMAND_LABELS[op] ?? MORE_COMMANDS[op] ?? op;

export const MODES: PolicyMode[] = ['direct', 'review', 'off'];
const RANK: Record<string, number> = { off: 0, review: 1, direct: 2 };
/** A project manager may only make a policy stricter (direct → review → off). */
export const isStricter = (from: PolicyMode, to: PolicyMode) => (RANK[to] ?? 0) <= (RANK[from] ?? 0);

export type Policies = { commands: Record<string, PolicyMode>; fields: Record<string, PolicyMode> };

/** Every command and field not `off` set to `mode` (「全部需要核准」keeps what is not allowed out). */
export function setAll(p: Policies, mode: PolicyMode, includeOff = false): Policies {
	const map = (o: Record<string, PolicyMode>) => Object.fromEntries(Object.entries(o).map(([k, v]) => [k, v === 'off' && !includeOff ? 'off' : mode])) as Record<string, PolicyMode>;
	return { commands: map(p.commands), fields: map(p.fields) };
}
/** Only the entries that differ from the template (`RoleDelegation.policies` is a change list). */
export function changesFrom(template: Policies, p: Policies): Policies {
	const d = (t: Record<string, PolicyMode>, o: Record<string, PolicyMode>) => Object.fromEntries(Object.entries(o).filter(([k, v]) => t[k] !== v)) as Record<string, PolicyMode>;
	return { commands: d(template.commands, p.commands), fields: d(template.fields, p.fields) };
}
/** Commands by mode, for the summaries (direct／review columns). */
export function byMode(p: Policies): Record<PolicyMode, string[]> {
	const out: Record<PolicyMode, string[]> = { direct: [], review: [], off: [] };
	for (const [k, v] of Object.entries(p.commands)) out[v]?.push(k);
	return out;
}
/** At least one command can write (directly or for review). */
export const anyAllowed = (p: Policies) => Object.values(p.commands).some((v) => v !== 'off');

/** §3.12.3 `shared_runtime` warning, with the time sentence when someone there reads your time (V10). */
export function sharedRuntimeText(w: Pick<SharedRuntimeWarning, 'roles' | 'sources' | 'time_readers'>): string {
	const roles = w.roles.join('、') || '其他角色';
	const src = w.sources.length ? `它只能寫到讀者不多於 ${w.sources.join('、')} 的地方；` : '';
	let t = `這個角色會和 ${roles} 共用來源限制：${src}任一專案權限改變時這些角色會一起停。`;
	if (w.time_readers.length) t += `這個執行環境裡 ${w.time_readers.join('、')} 可讀你的工時，新角色也可能從共用記憶取得你的工時說明。`;
	return t;
}

/** §6.14 read_own_time warning, word for word (A1(c)). */
export const TIME_WARNING = {
	title: '這個 agent 可以讀你自己的工時紀錄，包括說明文字。',
	body: [
		'工時說明通常只有你和有權看工時的人看得到。agent 讀了之後，可能把說明抄進議題留言或欄位，讓專案所有成員都看得到。oxledger 不會攔下這種搬移（這是你同意承擔的例外，其他內容仍會攔）。',
		'agent 讀不到別人的工時，也讀不到工時合計。之後關掉這個開關，agent 已經讀過的內容也收不回來。'
	],
	ack: '我了解，仍要開啟'
};

/** `role_key` the server accepts: a lowercase letter, then up to 29 lowercase letters, digits or `_`. */
export const ROLE_KEY = /^[a-z][a-z0-9_]{0,29}$/;

/** The activity filter: a full issue key (`AUTH-12`) goes to the server's `issue`, anything else is '' (filters only what is loaded). */
export function fullIssueKey(s: string): string {
	const t = s.trim();
	return /^[A-Za-z][A-Za-z0-9_]*-\d+$/.test(t) ? t.toUpperCase() : '';
}

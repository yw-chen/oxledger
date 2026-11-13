import { describe, expect, it } from 'vitest';
import type { DiffRow } from '$api/types';
import { agentChainReason, AGENT_CHAIN_TIP, defaultModeFor, diffLines, newOperations, neverTickable, timeComment, timeText, triageText, twoDecimals, whyNotQueue, withNewOperations } from './review';

const target = (id: string, key = 'AUTH-1') => ({ id, key, subject: 's' });
const row = (field: string, after: unknown, id = 'i1', before: unknown = null): DiffRow => ({ field, label: field, before, after, original: null, has_original: false, target: target(id) });

describe('twoDecimals', () => {
	it('pads and keeps decimal strings exact', () => {
		expect(twoDecimals('1.5')).toBe('1.50');
		expect(twoDecimals('8')).toBe('8.00');
		expect(twoDecimals('0.25')).toBe('0.25');
		expect(twoDecimals(2)).toBe('2.00');
		expect(twoDecimals(null)).toBe('');
	});
});

describe('time rows (review_queue.rs time_row)', () => {
	const after = { hours: '1.5', spent_on: '2026-11-12', activity: { id: 'a', name: '開發' }, comments: '修 SSO', user_id: 'u' };
	it('reads like the contract example', () => {
		expect(timeText(after)).toBe('記工時 1.50 小時（2026-11-12，開發）');
	});
	it('leaves out what is missing', () => {
		expect(timeText({ hours: '2', spent_on: '2026-11-12', activity: null })).toBe('記工時 2.00 小時（2026-11-12）');
		expect(timeText({ hours: '2' })).toBe('記工時 2.00 小時');
	});
	it('shortens the comment', () => {
		expect(timeComment(after)).toBe('修 SSO');
		expect(timeComment({ comments: '工'.repeat(90) }, 80)).toBe('工'.repeat(80) + '…');
		expect(timeComment({ comments: '  ' })).toBe('');
	});
});

describe('triage rows', () => {
	it('names the decision', () => {
		expect(triageText({ decision: 'accept' })).toBe('Triage：接受');
		expect(triageText({ decision: 'decline' })).toBe('Triage：拒絕');
		expect(triageText({ decision: 'duplicate', duplicate_of: 'x' })).toBe('Triage：標為重複');
		expect(triageText({ decision: 'unsnooze' })).toBe('Triage：取消稍後');
	});
	it('snooze shows until when', () => {
		expect(triageText({ decision: 'snooze', snooze_until: '2026-11-20T01:00:00Z' })).toMatch(/^Triage：稍後處理（到 .*11.*20.*）$/);
	});
	it('accept with an assignee', () => {
		expect(triageText({ decision: 'accept' }, { id: 'b', name: 'Bob' })).toBe('Triage：接受，指派給 Bob');
	});
});

describe('diffLines', () => {
	it('folds an accept assignment into the triage line on the same issue only', () => {
		const lines = diffLines([
			row('triage', { decision: 'accept' }, 'i1', 'pending'),
			row('assigned_to_id', { id: 'b', name: 'Bob' }, 'i1'),
			row('assigned_to_id', { id: 'c', name: 'Carol' }, 'i2'),
			row('status', { id: 's', name: '處理中' }, 'i1')
		]);
		expect(lines.map((l) => (l.kind === 'summary' ? l.text : l.row.field))).toEqual(['Triage：接受，指派給 Bob', 'assigned_to_id', 'status']);
		expect(lines[1].row.target.id).toBe('i2');
	});
	it('keeps the assignment row for a decision other than accept', () => {
		const lines = diffLines([row('triage', { decision: 'decline' }), row('assigned_to_id', { name: 'Bob' })]);
		expect(lines.map((l) => l.kind)).toEqual(['summary', 'field']);
	});
	it('time rows carry the comment as detail; notes stay summary', () => {
		const [t, n] = diffLines([row('time', { hours: '1', spent_on: '2026-11-12', comments: 'x' }), row('note', { is_private: true })]);
		expect(t).toMatchObject({ kind: 'summary', text: '記工時 1.00 小時（2026-11-12）', detail: '說明：x' });
		expect(n).toMatchObject({ kind: 'summary', text: '新增私有 note（內容請打開看）' });
	});
});

describe('whyNotQueue', () => {
	const base = { state: 'proposed', rules_changed: false, warnings: [] as string[], validation: null, agent_chain: null as string | null };
	it('agent_chain comes first and is never tickable', () => {
		const c = { ...base, agent_chain: 'writes', warnings: ['evidence_changed'] };
		expect(whyNotQueue(c)).toBe(AGENT_CHAIN_TIP);
		expect(neverTickable(c)).toBe(true);
	});
	it('names the chain reason without numbers', () => {
		expect(agentChainReason('depth')).toBe('agent 連續寫入上限（換手過多）');
		expect(agentChainReason('writes')).toBe('agent 連續寫入上限（寫入次數過多）');
		expect(agentChainReason('other')).toBe('agent 連續寫入上限');
	});
	it('rules_changed', () => {
		const c = { ...base, rules_changed: true };
		expect(whyNotQueue(c)).toBe('規則已變，需要重新檢視：請打開看');
		expect(neverTickable(c)).toBe(true);
	});
	it('warnings, validation, default; nothing when not proposed', () => {
		expect(whyNotQueue({ ...base, warnings: ['evidence_changed'] }, () => '根據已變動')).toBe('根據已變動：請打開看');
		expect(whyNotQueue({ ...base, validation: { status: 'conflict' } })).toBe('有衝突：請打開處理');
		expect(whyNotQueue(base)).toMatch(/^內容需要打開看/);
		expect(whyNotQueue({ ...base, state: 'applied', agent_chain: 'depth' })).toBe('');
		expect(neverTickable(base)).toBe(false);
	});
});

describe('new operations (§4.3)', () => {
	const p = {
		commands: { update_fields: 'direct' as const, transition: 'review' as const },
		fields: { priority_id: 'direct' as const },
		available_commands: ['triage_decide', 'log_time', 'update_time_entry', 'transition'],
		available_fields: ['estimated_hours']
	};
	it('lists what the delegation lacks', () => {
		expect(newOperations(p)).toEqual({ commands: ['triage_decide', 'log_time', 'update_time_entry'], fields: ['estimated_hours'] });
	});
	it('time commands default to review, others direct', () => {
		expect(defaultModeFor('log_time')).toBe('review');
		expect(defaultModeFor('update_time_entry')).toBe('review');
		expect(defaultModeFor('triage_decide')).toBe('direct');
	});
	it('adds them and keeps the current policy', () => {
		const next = withNewOperations(p, newOperations(p));
		expect(next.commands).toEqual({ update_fields: 'direct', transition: 'review', triage_decide: 'direct', log_time: 'review', update_time_entry: 'review' });
		expect(next.fields).toEqual({ priority_id: 'direct', estimated_hours: 'direct' });
	});
});

import { describe, expect, it } from 'vitest';
import { ago, DECLINE_PREFIX, noteText, triageDoc } from './triage';

const kinds = (d: ReturnType<typeof triageDoc>) => d.ops.map((o) => o.kind);

describe('triageDoc (contract 05 §3.10.1)', () => {
	it('accept carries assignment, transition and note, triage_decide first', () => {
		const d = triageDoc('AUTH-7', 2, { decision: 'accept', assignee: 'u1', toStatus: 's2', note: '先看 CSP' });
		expect(kinds(d)).toEqual(['triage_decide', 'update_fields', 'transition', 'add_note']);
		expect(d.ops[1]).toMatchObject({ set: { assigned_to_id: 'u1' } });
		expect(d.preconditions).toEqual([{ target: { type: 'issue', key: 'AUTH-7' }, lock_version: 2 }]);
	});
	it('accept with nothing else is the decision alone', () => {
		expect(kinds(triageDoc('AUTH-7', 0, { decision: 'accept', assignee: '', toStatus: '', note: '' }))).toEqual(['triage_decide']);
	});
	it('decline drops a note that is only the preset prefix', () => {
		expect(kinds(triageDoc('AUTH-7', 0, { decision: 'decline', toStatus: 'closed', note: DECLINE_PREFIX }))).toEqual(['triage_decide', 'transition']);
		expect(kinds(triageDoc('AUTH-7', 0, { decision: 'decline', toStatus: 'closed', note: `${DECLINE_PREFIX}重複回報` }))).toEqual(['triage_decide', 'transition', 'add_note']);
	});
	it('duplicate names the original and locks both ends; no separate duplicate_of op', () => {
		const d = triageDoc('AUTH-9', 1, { decision: 'duplicate', toStatus: 'closed', dupOf: { key: 'AUTH-3', lock: 14 }, note: '和 AUTH-3 重複。' });
		expect(d.ops[0]).toMatchObject({ kind: 'triage_decide', decision: 'duplicate', duplicate_of: { type: 'issue', key: 'AUTH-3' } });
		expect(kinds(d)).not.toContain('duplicate_of');
		expect(d.preconditions!.map((p) => p.lock_version)).toEqual([1, 14]);
	});
	it('snooze carries the time and never a transition', () => {
		const d = triageDoc('AUTH-9', 1, { decision: 'snooze', snoozeUntil: '2026-11-14T01:00:00.000Z', toStatus: 'x', assignee: 'u' });
		expect(kinds(d)).toEqual(['triage_decide']);
		expect(d.ops[0]).toMatchObject({ snooze_until: '2026-11-14T01:00:00.000Z' });
	});
	it('note text and relative time', () => {
		expect(noteText('  ')).toBeNull();
		expect(noteText(DECLINE_PREFIX)).toBeNull();
		const now = Date.parse('2026-11-13T12:00:00Z');
		expect(ago('2026-11-13T09:00:00Z', now)).toBe('3 小時前');
		expect(ago('2026-11-13T11:59:30Z', now)).toBe('剛剛');
		expect(ago('2026-11-11T12:00:00Z', now)).toBe('2 天前');
	});
});

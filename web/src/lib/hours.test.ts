import { describe, expect, it } from 'vitest';
import { addDays, addHours, dayLabel, deleteTimeDoc, isoWeek, logTimeDoc, logTimeOp, parseHours, period, shiftWeek, showHours, snoozePreset, updateTimeDoc, weekMonday } from './hours';
import { ApiError } from './api';
import { describeError } from './errors';

describe('parseHours', () => {
	it.each([
		['1.5', '1.50'],
		['1', '1.00'],
		['0.25', '0.25'],
		['1:30', '1.50'],
		['1:20', '1.33'],
		['0:05', '0.08'],
		['1h30m', '1.50'],
		['2h', '2.00'],
		['90m', '1.50'],
		['45min', '0.75'],
		[' 1 h 30 m ', '1.50'],
		['0', '0.00'],
		['0:01', '0.02'],
		['0:02', '0.03'],
		['.5', '0.50'],
		['1,5', '1.50'],
		['1.5h', '1.50']
	])('%s → %s', (i, o) => expect(parseHours(i)).toBe(o));
	it.each(['', 'abc', '1.234', '1:75', '-1', 'h', '1.', '.'])('rejects %j', (i) => expect(parseHours(i)).toBeNull());
});

describe('hour sums and display', () => {
	it('adds exactly to the cent', () => {
		expect(addHours('0.10', '0.20', null, '1.50')).toBe('1.80');
		expect(addHours()).toBe('0.00');
	});
	it('shows without trailing zeros', () => {
		expect(showHours('8.00')).toBe('8');
		expect(showHours('1.50')).toBe('1.5');
		expect(showHours(null)).toBe('');
	});
});

describe('weeks', () => {
	it('ISO week and its Monday', () => {
		expect(isoWeek('2026-11-13')).toBe('2026-W46');
		expect(weekMonday('2026-W46')).toBe('2026-11-09');
		// The first days of January can belong to the previous year's week.
		expect(isoWeek('2027-01-01')).toBe('2026-W53');
		expect(isoWeek('2026-01-01')).toBe('2026-W01');
		expect(shiftWeek('2026-W53', 1)).toBe('2027-W01');
		expect(shiftWeek('2026-W01', -1)).toBe('2025-W52');
	});
	it('periods follow week_start', () => {
		expect(period('this_week', '2026-11-13', 1)).toEqual({ from: '2026-11-09', to: '2026-11-15' });
		expect(period('this_week', '2026-11-13', 7)).toEqual({ from: '2026-11-08', to: '2026-11-14' });
		expect(period('last_week', '2026-11-09', 1)).toEqual({ from: '2026-11-02', to: '2026-11-08' });
		expect(period('this_month', '2026-02-10')).toEqual({ from: '2026-02-01', to: '2026-02-28' });
		expect(period('last_month', '2026-01-15')).toEqual({ from: '2025-12-01', to: '2025-12-31' });
	});
	it('labels and day math', () => {
		expect(dayLabel('2026-11-09')).toBe('11/9（一）');
		expect(addDays('2026-12-31', 1)).toBe('2027-01-01');
	});
	it('snooze presets are 9:00 local, never today', () => {
		const fri = new Date(2026, 10, 13, 15, 0); // Friday
		const t = new Date(snoozePreset('tomorrow', fri));
		expect([t.getDate(), t.getHours(), t.getMinutes()]).toEqual([14, 9, 0]);
		const m = new Date(snoozePreset('monday', fri));
		expect([m.getDay(), m.getDate(), m.getHours()]).toEqual([1, 16, 9]);
		const mon = new Date(2026, 10, 16, 8, 0); // a Monday morning: next Monday
		expect(new Date(snoozePreset('monday', mon)).getDate()).toBe(23);
	});
});

describe('time command documents (contract 05 §3.10.2)', () => {
	it('log_time with an issue needs no project and no precondition', () => {
		const d = logTimeDoc({ issueKey: 'AUTH-1', projectId: 'p1', spent_on: '2026-11-13', hours: '1.50', activity_id: 'a1', comments: '  x ', user_id: null });
		expect(d.preconditions).toEqual([]);
		expect(d.ops[0]).toMatchObject({ kind: 'log_time', issue: { type: 'issue', key: 'AUTH-1' }, fields: { spent_on: '2026-11-13', hours: '1.50', activity_id: 'a1', comments: 'x' } });
		expect(d.ops[0]).not.toHaveProperty('project_id');
		expect((d.ops[0] as { fields: object }).fields).not.toHaveProperty('user_id');
	});
	it('log_time without an issue names the project', () => {
		const op = logTimeOp({ projectId: 'p1', spent_on: '2026-11-13', hours: '2.00', user_id: 'u2' });
		expect(op).toMatchObject({ project_id: 'p1', fields: { user_id: 'u2' } });
		expect(op).not.toHaveProperty('issue');
	});
	it('update and delete carry the entry lock_version', () => {
		expect(updateTimeDoc('t1', 3, { hours: '2.00' })).toMatchObject({ ops: [{ kind: 'update_time_entry', target: { type: 'time_entry', id: 't1' }, set: { hours: '2.00' } }], preconditions: [{ target: { type: 'time_entry', id: 't1' }, lock_version: 3 }] });
		expect(deleteTimeDoc('t1', 4)).toMatchObject({ ops: [{ kind: 'delete_time_entry' }], preconditions: [{ lock_version: 4 }] });
	});
});

describe('time rule errors (§6.6 wording)', () => {
	const err = (details: object, code = 'invalid_value') => new ApiError(400, { code, message: 'x', details } as never, null);
	it('daily limit says what is left when the server tells', () => {
		expect(describeError(err({ kind: 'max_hours_per_day', max_hours_per_day: '8.00', remaining: '0.50' })).title).toBe('這天已經記了 7.5 小時，上限是 8 小時（還能記 0.5）');
		expect(describeError(err({ kind: 'max_hours_per_day', max_hours_per_day: '8.00' })).title).toBe('超過每天可記的上限（8 小時）');
	});
	it('future date, closed issue, required fields', () => {
		expect(describeError(err({ kind: 'future_date' })).title).toBe('不能記未來的日期');
		expect(describeError(err({ kind: 'closed_issue' })).title).toBe('這張單已結案，不接受記工時');
		expect(describeError(err({ fields: ['comments'] }, 'required_field_missing')).title).toBe('缺少必填欄位：說明');
	});
});

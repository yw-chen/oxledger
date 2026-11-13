import { describe, expect, it } from 'vitest';
import type { FilterFieldInfo } from '$api/types';
import {
	addDays,
	cellText,
	chips,
	condText,
	countConds,
	countText,
	defaultValue,
	depth,
	fromGroup,
	groupIncomplete,
	incomplete,
	opLabel,
	rangeDates,
	searchToWhere,
	toGroup,
	valueText
} from './filter';

const F = (field: string, type: string, label: string, options?: { id: string; label: string }[]) =>
	({ field, type, label, ops: [], options: options ?? null }) as unknown as FilterFieldInfo;
const FIELDS = [
	F('status_category', 'enum', '狀態分類'),
	F('assignee', 'principal', '指派'),
	F('priority', 'ref', '優先權', [{ id: 'p1', label: '高' }]),
	F('due_date', 'date', '到期日'),
	F('estimated_hours', 'number', '預估工時'),
	F('triage', 'enum', '分流')
];
const none = () => undefined;
const TODAY = '2026-11-13'; // a Friday

describe('AST ⇄ editor group', () => {
	it('round-trips nested groups and sends one condition bare', () => {
		const w = { and: [{ field: 'a', op: 'eq', value: 1 }, { or: [{ field: 'b', op: 'eq', value: 2 }, { field: 'c', op: 'is_null' }] }] };
		const g = toGroup(w);
		expect(g.mode).toBe('and');
		expect(depth(g)).toBe(2);
		expect(countConds(g)).toBe(3);
		expect(fromGroup(g)).toEqual(w);
		expect(fromGroup(toGroup({ field: 'a', op: 'eq', value: 1 }))).toEqual({ field: 'a', op: 'eq', value: 1 });
	});
	it('empty is null both ways, and empty sub-groups are dropped', () => {
		expect(toGroup(null)).toEqual({ mode: 'and', items: [] });
		expect(fromGroup({ mode: 'and', items: [] })).toBeNull();
		expect(fromGroup({ mode: 'and', items: [{ mode: 'or', items: [] }, { field: 'a', op: 'is_null' }] })).toEqual({ field: 'a', op: 'is_null' });
	});
	it('keeps an "any" group even with one item', () => {
		expect(fromGroup({ mode: 'or', items: [{ field: 'a', op: 'is_null' }] })).toEqual({ or: [{ field: 'a', op: 'is_null' }] });
	});
});

describe('incomplete conditions', () => {
	it('needs a value except for is_null / not_null', () => {
		expect(incomplete({ field: 'a', op: 'eq', value: '' })).toBe(true);
		expect(incomplete({ field: 'a', op: 'in', value: [] })).toBe(true);
		expect(incomplete({ field: 'a', op: 'between', value: ['2026-01-01', ''] })).toBe(true);
		expect(incomplete({ field: 'a', op: 'is_null' })).toBe(false);
		expect(incomplete({ field: 'a', op: 'eq', value: false })).toBe(false);
		expect(incomplete({ field: '', op: 'eq', value: 1 })).toBe(true);
		expect(groupIncomplete({ mode: 'and', items: [{ mode: 'or', items: [{ field: 'a', op: 'eq', value: null }] }] })).toBe(true);
	});
});

describe('dates', () => {
	it('adds days across month ends', () => {
		expect(addDays('2026-11-30', 1)).toBe('2026-12-01');
		expect(addDays('2026-03-01', -1)).toBe('2026-02-28');
	});
	it('computes ranges with the org week start', () => {
		expect(rangeDates({ range: 'this_week' }, TODAY, 1)).toEqual(['2026-11-09', '2026-11-15']);
		expect(rangeDates({ range: 'this_week' }, TODAY, 7)).toEqual(['2026-11-08', '2026-11-14']);
		expect(rangeDates({ range: 'last_week' }, TODAY, 1)).toEqual(['2026-11-02', '2026-11-08']);
		expect(rangeDates({ range: 'this_month' }, TODAY)).toEqual(['2026-11-01', '2026-11-30']);
		expect(rangeDates({ range: 'last_month' }, '2026-01-15')).toEqual(['2025-12-01', '2025-12-31']);
		expect(rangeDates({ range: 'last_n_days', n: 7 }, TODAY)).toEqual(['2026-11-07', '2026-11-13']);
		expect(rangeDates({ range: 'next_n_days', n: 3 }, TODAY)).toEqual(['2026-11-13', '2026-11-15']);
		expect(rangeDates({ range: 'nope' }, TODAY)).toBeNull();
	});
});

describe('words', () => {
	it('labels values', () => {
		const f = FIELDS[2];
		expect(valueText('me', FIELDS[1], TODAY, none)).toBe('我');
		expect(valueText({ me: true }, FIELDS[1], TODAY, none)).toBe('我');
		expect(valueText('p1', f, TODAY, none)).toBe('高');
		expect(valueText('x', f, TODAY, (_, id) => (id === 'x' ? '某人' : undefined))).toBe('某人');
		expect(valueText(['todo', 'doing'], FIELDS[0], TODAY, none)).toBe('待處理、進行中');
		expect(valueText({ rel: 'today', offset: -3 }, FIELDS[3], TODAY, none)).toBe('3 天前（11/10）');
		expect(valueText({ range: 'last_n_days', n: 7 }, FIELDS[3], TODAY, none)).toBe('最近 7 天（11/07–11/13）');
		expect(valueText('pending', FIELDS[5], TODAY, none)).toBe('待分流');
		expect(valueText(true, undefined, TODAY, none)).toBe('是');
	});
	it('writes conditions and chips', () => {
		expect(condText({ field: 'status_category', op: 'in', value: ['todo'] }, FIELDS, TODAY, none)).toBe('狀態分類：待處理');
		expect(condText({ field: 'assignee', op: 'is_null' }, FIELDS, TODAY, none)).toBe('指派沒有值');
		expect(condText({ field: 'estimated_hours', op: 'gt', value: 4 }, FIELDS, TODAY, none)).toBe('預估工時 大於 4');
		expect(condText({ field: 'due_date', op: 'between', value: ['2026-11-01', '2026-11-30'] }, FIELDS, TODAY, none)).toBe('到期日 介於 2026-11-01 到 2026-11-30');
		const g = toGroup({ and: [{ field: 'assignee', op: 'eq', value: 'me' }, { or: [{ field: 'priority', op: 'eq', value: 'p1' }, { field: 'due_date', op: 'is_null' }] }] });
		expect(chips(g, FIELDS, TODAY, none)).toEqual(['指派：我', '（任一：優先權：高；到期日沒有值）']);
		expect(chips(toGroup({ or: [{ field: 'assignee', op: 'eq', value: 'me' }, { field: 'assignee', op: 'is_null' }] }), FIELDS, TODAY, none)).toEqual(['任一：指派：我；指派沒有值']);
	});
	it('uses comparison words for numbers only', () => {
		expect(opLabel('lt', 'number')).toBe('小於');
		expect(opLabel('lt', 'date')).toBe('早於');
		expect(opLabel('weird')).toBe('weird');
	});
});

describe('defaults and cells', () => {
	it('picks a default value per op', () => {
		expect(defaultValue(FIELDS[3], 'within', TODAY)).toEqual({ range: 'last_n_days', n: 7 });
		expect(defaultValue(FIELDS[3], 'between', TODAY)).toEqual([TODAY, TODAY]);
		expect(defaultValue(FIELDS[1], 'in', TODAY)).toEqual([]);
		expect(defaultValue(FIELDS[3], 'eq', TODAY)).toBe(TODAY);
		expect(defaultValue(FIELDS[1], 'is_null', TODAY)).toBeUndefined();
	});
	it('renders cells and counts', () => {
		expect(cellText(null)).toBe('—');
		expect(cellText({ name: 'Bot', kind: 'agent' })).toBe('Bot〔agent〕');
		expect(cellText({ label: '舊版', active: false })).toBe('舊版（停用）');
		expect(cellText([{ name: 'A' }, { name: 'B' }])).toBe('A、B');
		expect(cellText('2026-11-13T02:05:00Z')).toMatch(/11\/13.*10:05/);
		expect(countText({ value: 999, capped: true })).toBe('999+');
		expect(countText({ value: 6, capped: false })).toBe('6');
		expect(countText(null)).toBe('—');
	});
});

describe('search → view', () => {
	it('maps the structured part', () => {
		expect(searchToWhere({}, null)).toBeNull();
		expect(searchToWhere({ assignee: 'me' })).toEqual({ field: 'assignee', op: 'eq', value: 'me' });
		expect(searchToWhere({ status_category: 'open', version: 'v1', overdue: true }, 'pid')).toEqual({
			and: [
				{ field: 'project', op: 'eq', value: 'pid' },
				{ field: 'status_category', op: 'in', value: ['todo', 'doing'] },
				{ field: 'fixed_version', op: 'eq', value: 'v1' },
				{ field: 'overdue', op: 'eq', value: true }
			]
		});
		expect(searchToWhere({ status_category: 'done', tracker: 'Bug', status: 'New' })).toEqual({
			and: [
				{ field: 'tracker', op: 'eq', value: 'Bug' },
				{ field: 'status', op: 'eq', value: 'New' },
				{ field: 'status_category', op: 'eq', value: 'done' }
			]
		});
	});
});

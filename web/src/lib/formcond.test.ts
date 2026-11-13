import { describe, expect, it } from 'vitest';
import conformance from '../../../api/form-conformance.json';
import { catalogOf, condValues, describeWhen, evalWhen, requiredNow, whenProblem, wrapValue } from './formcond';

describe('form condition conformance vectors (T-M1-56)', () => {
	it.each(conformance.vectors.map((v) => [v.name, v] as const))('%s', (_, v) => {
		expect(evalWhen(v.when, v.values as Record<string, unknown>)).toBe(v.expected);
	});
});

describe('form values → condition values', () => {
	const fields = [
		{ key: 'cf:1', type: 'list' },
		{ key: 'cf:6', type: 'bool' },
		{ key: 'cf:2', type: 'float' }
	];
	it('renames core keys and types custom fields', () => {
		expect(condValues({ assigned_to_id: 'u1', priority_id: 'p1', fixed_version_id: 'v1', 'cf:1': 'o1', 'cf:6': false, 'cf:2': 1.5, subject: '', due_date: null }, fields, { tracker: 't1', status_category: 'todo' })).toEqual({
			tracker: 't1',
			status_category: 'todo',
			assignee: 'u1',
			priority: 'p1',
			fixed_version: 'v1',
			'cf:1': { list: 'o1' },
			'cf:6': { bool: false },
			'cf:2': { float: '1.5' }
		});
	});
	it('required_if follows the form as it is filled', () => {
		const f = { required: false, required_if: [{ rule_id: 'r1', when: { all: [{ field: 'cf:1', op: 'eq', value: { list: 'o1' } }] } }] };
		expect(requiredNow(f, condValues({ 'cf:1': 'o2' }, fields))).toBe(false);
		expect(requiredNow(f, condValues({ 'cf:1': 'O1' }, fields))).toBe(true);
		expect(requiredNow({ required: true }, {})).toBe(true);
	});
});

describe('describing and checking conditions', () => {
	const cat = catalogOf([{ key: 'cf:1', type: 'list', name: '嚴重度', options: [{ id: 'o1', label: '低' }] } as never]);
	it('reads like a sentence', () => {
		expect(describeWhen({ all: [{ field: 'cf:1', op: 'eq', value: { list: 'o1' } }] }, cat)).toBe('嚴重度 是 低');
		expect(describeWhen({ all: [{ field: 'status_category', op: 'eq', value: 'done' }, { any: [{ field: 'assignee', op: 'is_set' }, { field: 'due_date', op: 'lt', value: '2026-12-01' }] }] }, cat)).toBe('狀態類別 是 完成 且 （指派對象 有值 或 到期日 早於／小於 2026-12-01）');
	});
	it('wraps custom field values with their type', () => {
		expect(wrapValue(cat, 'cf:1', 'o1')).toEqual({ list: 'o1' });
		expect(wrapValue(cat, 'status', 's1')).toBe('s1');
	});
	it('finds structural problems before sending', () => {
		expect(whenProblem({ all: [{ field: '', op: 'eq', value: 'x' }] })).toBe('有條件還沒選欄位');
		expect(whenProblem({ all: [{ field: 'status', op: 'eq', value: '' }] })).toBe('有條件還沒填值');
		expect(whenProblem({ all: Array.from({ length: 6 }, () => ({ field: 'status', op: 'is_set' })) })).toBe('每一組最多 5 條');
		expect(whenProblem({ all: [{ any: [{ all: [{ field: 'status', op: 'is_set' }] }] }] })).toBe('條件最多兩層');
		expect(whenProblem({ all: [{ field: 'status', op: 'is_set' }, { any: [{ field: 'due_date', op: 'not_set' }] }] })).toBeNull();
	});
});

describe('layout visibility', () => {
	it('hides a section and a field by show_if', async () => {
		const { hiddenKeys } = await import('./formcond');
		const layout = {
			sections: [
				{ id: 's1', title: 'a', fields: [{ key: 'subject' }, { key: 'cf:2', show_if: { all: [{ field: 'status_category', op: 'eq', value: 'done' }] } }] },
				{ id: 's2', title: 'b', show_if: { all: [{ field: 'cf:1', op: 'eq', value: { list: 'o1' } }] }, fields: [{ key: 'cf:7' }] }
			]
		};
		expect([...hiddenKeys(layout, {})].sort()).toEqual(['cf:2', 'cf:7']);
		expect([...hiddenKeys(layout, { 'cf:1': { list: 'o1' }, status_category: 'done' })]).toEqual([]);
	});
});

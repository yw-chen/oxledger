import { describe, expect, it } from 'vitest';
import { convertTargets, fieldState, mappingKind } from './fieldadmin';
import { requiredNow } from './formcond';

describe('type conversion pairs (field_admin.rs rule_for)', () => {
	it('offers what the server accepts', () => {
		expect(convertTargets('int').sort()).toEqual(['float', 'link', 'string', 'text']);
		expect(convertTargets('string').sort()).toEqual(['bool', 'date', 'float', 'int', 'link', 'list', 'text']);
		expect(convertTargets('list').sort()).toEqual(['link', 'list', 'string', 'text']);
		expect(convertTargets('text').sort()).toEqual(['bool', 'date', 'float', 'int', 'link', 'list', 'string']);
		expect(convertTargets('bool')).toEqual([]);
		expect(convertTargets('user')).toEqual([]);
	});
	it('names the mapping each pair needs', () => {
		expect(mappingKind('list', 'list')).toBe('options');
		expect(mappingKind('string', 'list')).toBe('values');
		expect(mappingKind('string', 'int')).toBe('none');
	});
	it('list states', () => {
		const f = { deactivated: false, index_state: 'ready' } as never;
		expect(fieldState(f).label).toBe('正常');
		expect(fieldState({ ...(f as object), index_state: 'pending' } as never).label).toBe('索引建立中');
		expect(fieldState({ ...(f as object), deactivated: true } as never).label).toBe('停用');
		expect(fieldState({ ...(f as object), index_state: 'dropping', deactivated: true } as never).label).toBe('刪除中');
	});
});

describe('required_if with several rules', () => {
	it('any holding rule makes the field required', () => {
		const f = {
			required: false,
			required_if: [
				{ when: { all: [{ field: 'status_category', op: 'eq', value: 'done' }] } },
				{ when: { all: [{ field: 'priority', op: 'eq', value: 'p1' }] } }
			]
		};
		expect(requiredNow(f, { priority: 'p1', status_category: 'todo' })).toBe(true);
		expect(requiredNow(f, { priority: 'p2', status_category: 'todo' })).toBe(false);
	});
});

import { describe, expect, it } from 'vitest';
import { whoName } from './who';

describe('whoName', () => {
	const me = { user_id: 'u-me' };
	const people = [
		{ id: 'u-alice', name: 'Alice', kind: 'user' },
		{ id: 'g-dev', name: '開發組', kind: 'group' },
		{ id: 'u-bob', name: 'Bob' }
	];
	it('says 你 for the signed-in user', () => expect(whoName('u-me', me, people)).toBe('你'));
	it('uses a known person name', () => {
		expect(whoName('u-alice', me, people)).toBe('Alice');
		expect(whoName('u-bob', me, people)).toBe('Bob');
	});
	it('never takes a group for a person', () => expect(whoName('g-dev', me, people)).toBe('使用者 gdev'));
	it('falls back to a short id', () =>
		expect(whoName('0192f3a4-5b6c-7d8e-9f00-112233445566', null)).toBe('使用者 0192f3a4'));
	it('is empty without an id', () => {
		expect(whoName(null, me)).toBe('');
		expect(whoName(undefined, me)).toBe('');
	});
});

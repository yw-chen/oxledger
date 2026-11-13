import { describe, expect, it } from 'vitest';
import { counts, triageLabel } from './counts.svelte';

const p = (count: number, more = false) => ({ count, more, project: { id: 'x', key: 'X', name: 'X' } });

describe('triageLabel (contract 05 §3.4 counts)', () => {
	it('sums projects; empty when nothing waits', () => {
		counts.triage = [];
		expect(triageLabel()).toBe('');
		counts.triage = [p(0)];
		expect(triageLabel()).toBe('');
		counts.triage = [p(3), p(4)];
		expect(triageLabel()).toBe('7');
	});
	it('99+ when a project has more, or the sum passes 99', () => {
		counts.triage = [p(99, true)];
		expect(triageLabel()).toBe('99+');
		counts.triage = [p(60), p(40)];
		expect(triageLabel()).toBe('99+');
	});
});

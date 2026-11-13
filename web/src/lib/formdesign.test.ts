import { describe, expect, it } from 'vitest';
import { layoutOf, rulesOf } from './formdesign';

describe('rulesOf / layoutOf take reactive state', () => {
	// Svelte $state values are Proxies; structuredClone throws DataCloneError on them.
	const proxied = <T extends object>(x: T): T => new Proxy(x, {});
	it('copies a proxied rules body', () => {
		const body = proxied({ rules: proxied([proxied({ id: 'r1', field: 'cf:7', kind: 'required_if', when: { all: [] } })]) });
		const out = rulesOf(body);
		expect(out).toEqual([{ id: 'r1', field: 'cf:7', kind: 'required_if', when: { all: [] } }]);
		out[0].field = 'x';
		expect(body.rules[0].field).toBe('cf:7');
	});
	it('copies a proxied layout body', () => {
		const body = proxied({ modes: proxied({ create: proxied({ sections: [{ id: 's1', title: 'A', columns: 1, fields: [] }] }), update: 'same_as_create' }) });
		expect(layoutOf(body, []).modes.create.sections[0].title).toBe('A');
	});
});

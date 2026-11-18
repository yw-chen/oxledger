// SPDX-License-Identifier: GPL-2.0-or-later
import { describe, expect, it } from 'vitest';
import { defaultTimeZone, googleErrorText, googleStartUrl, setupProblems } from './google.js';

describe('Google sign-in helpers', () => {
	it('has a text for every refusal the server sends, and one for the rest', () => {
		for (const code of ['not_configured', 'cancelled', 'state', 'exchange', 'unverified', 'domain', 'locked', 'other_account', 'setup_not_allowed', 'signed_out']) {
			const t = googleErrorText(code);
			expect(t, code).toBeTruthy();
			expect(t, code).not.toBe(googleErrorText('something_new'));
		}
		expect(googleErrorText('something_new')).toContain('Google');
		expect(googleErrorText(null)).toBeNull();
		expect(googleErrorText('')).toBeNull();
		expect(googleErrorText('locked')).toContain('管理員');
	});

	it('builds the start link with an encoded way back', () => {
		expect(googleStartUrl('/')).toBe('/api/v1/auth/google/start');
		expect(googleStartUrl('/issue/AUTH-1?tab=wiki')).toBe('/api/v1/auth/google/start?next=%2Fissue%2FAUTH-1%3Ftab%3Dwiki');
		expect(googleStartUrl('/agents', true)).toBe('/api/v1/auth/google/start?reauth=1&next=%2Fagents');
		expect(googleStartUrl('/', true)).toBe('/api/v1/auth/google/start?reauth=1');
	});

	it('checks the setup form before it is sent', () => {
		const ok = { org_name: '奧克斯軟體', login: 'owner', name: '王小明', timezone: 'Asia/Taipei' };
		expect(setupProblems(ok)).toEqual({});
		expect(Object.keys(setupProblems({ org_name: ' ', login: '', name: '', timezone: '' })).sort()).toEqual(['login', 'name', 'org_name', 'timezone']);
		expect(setupProblems({ ...ok, login: 'ow ner' }).login).toContain('空白');
		expect(setupProblems({ ...ok, org_name: '字'.repeat(101) }).org_name).toContain('100');
		expect(setupProblems({ ...ok, org_name: '字'.repeat(100) })).toEqual({});
	});

	it('takes the browser time zone only when it is on the list', () => {
		const here = Intl.DateTimeFormat().resolvedOptions().timeZone;
		expect(defaultTimeZone([], 'Asia/Taipei')).toBe(here || 'Asia/Taipei');
		expect(defaultTimeZone(['Mars/Olympus'], 'Asia/Taipei')).toBe('Asia/Taipei');
		expect(defaultTimeZone([here], 'Asia/Taipei')).toBe(here);
	});
});

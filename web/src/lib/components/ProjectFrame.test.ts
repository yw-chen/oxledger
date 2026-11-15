// SPDX-License-Identifier: GPL-2.0-or-later
import { afterEach, describe, expect, it } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import { setFetch } from '#lib/api.js';
import ProjectFrame from './ProjectFrame.svelte';

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const tabs = { can_manage: false, default_tab: 'timeline', is_default: true, lock_version: 1, tabs: [{ id: 'issues', kind: 'builtin', title: '議題', hidden: false }, { id: 'wiki', kind: 'builtin', title: 'Wiki', hidden: false }] };
const notFound = () => json(404, { error: { code: 'not_found', message: 'no such wiki page', details: {}, next_actions: [], retryable: false, request_id: 'r' } });

/** Wiki settings answers held until the test lets each one go. */
function server() {
	const pending = new Map<string, ((r: Response) => void)[]>();
	setFetch(async (url) => {
		const m = /\/projects\/([^/]+)\/(tabs|wiki\/settings)/.exec(url);
		if (m?.[2] === 'tabs') return json(200, { data: tabs });
		if (m?.[2] === 'wiki/settings') return new Promise<Response>((res) => pending.set(m[1], [...(pending.get(m[1]) ?? []), res]));
		return json(404, {});
	});
	return {
		answer(key: string, r: Response) {
			const list = pending.get(key) ?? [];
			pending.set(key, []);
			for (const f of list) f(r);
		}
	};
}
const children = createRawSnippet(() => ({ render: () => '<div></div>' }));
const wikiTab = () => screen.queryByRole('link', { name: 'Wiki' });

describe('ProjectFrame: the Wiki tab (design 06 R-W1)', () => {
	afterEach(() => setFetch((i, init) => fetch(i, init)));

	it('hides the tab at once when switching to another project, and ignores the old answer', async () => {
		const s = server();
		const { rerender } = render(ProjectFrame, { projectKey: 'AUTH', children });
		await screen.findByRole('link', { name: '議題' });
		expect(wikiTab()).toBeNull();
		s.answer('AUTH', json(200, { data: { can_manage: false, lock_version: 1, start_page: null, start_page_id: null } }));
		await waitFor(() => expect(wikiTab()).not.toBeNull());

		// To a project whose wiki is not readable: gone before its answer comes back.
		await rerender({ projectKey: 'SECRET', children });
		expect(wikiTab()).toBeNull();
		s.answer('SECRET', notFound());
		await new Promise((r) => setTimeout(r, 20));
		expect(wikiTab()).toBeNull();
	});

	it('a late answer for the project left behind does not show the tab on the new one', async () => {
		const s = server();
		const { rerender } = render(ProjectFrame, { projectKey: 'AUTH', children });
		await screen.findByRole('link', { name: '議題' });
		await rerender({ projectKey: 'SECRET', children });
		// SECRET answers first (not readable), then AUTH's older probe says readable.
		s.answer('SECRET', notFound());
		await new Promise((r) => setTimeout(r, 20));
		s.answer('AUTH', json(200, { data: { can_manage: false, lock_version: 1, start_page: null, start_page_id: null } }));
		await new Promise((r) => setTimeout(r, 20));
		expect(wikiTab()).toBeNull();
	});
});

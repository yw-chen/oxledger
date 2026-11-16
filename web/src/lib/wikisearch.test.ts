// SPDX-License-Identifier: GPL-2.0-or-later
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';
import { setFetch } from './api';
import { parse, toParams } from './query';
import { clearWikiWrites, forgetWikiWrite, latestWikiToken, pendingWikiWrites, rememberWikiWrite, wikiHitHref, wikiQuery } from './wikisearch';
import WikiSearch from './components/WikiSearch.svelte';

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const fromBox = (s: string) => wikiQuery(toParams(parse(s), s));

describe('the Wiki tab reads the search box (design 06 §6.7)', () => {
	it('takes the free text and project:', () => {
		expect(fromBox('project:AUTH 部署 流程')).toEqual({ q: '部署 流程', project: 'AUTH', ignored: [] });
		expect(fromBox('deploy')).toEqual({ q: 'deploy', project: null, ignored: [] });
	});
	it('names the issue-only filters instead of searching something else', () => {
		expect(fromBox('status:open assignee:me 部署')).toEqual({ q: '部署', project: null, ignored: ['status:', 'assignee:'] });
	});
	it('has no query when only filters were typed', () => {
		expect(fromBox('project:AUTH').q).toBeNull();
		expect(fromBox('   ').q).toBeNull();
	});
});

describe('result links (R-W2)', () => {
	it('uses the server url_title as is (already encoded)', () => {
		const t = '%E9%83%A8%E7%BD%B2_%E8%88%87_%E5%9B%9E%E6%BB%BE';
		expect(wikiHitHref({ project: { id: 'p', key: 'AUTH', name: 'Auth' }, url_title: t })).toBe(`/project/AUTH/wiki/${t}`);
	});
});

describe('pages this tab just wrote (03 Q-S10)', () => {
	beforeEach(() => sessionStorage.clear());

	it('stay pending until a search returns them, newest token first', () => {
		rememberWikiWrite({ pageId: 'a', projectKey: 'auth', title: 'A', after: 'c1.1.10' });
		rememberWikiWrite({ pageId: 'b', projectKey: 'WEB', title: 'B', after: 'c1.1.11' });
		expect(latestWikiToken()).toBe('c1.1.11');
		expect(pendingWikiWrites([], 'AUTH').map((c) => c.pageId)).toEqual(['a']);
		expect(pendingWikiWrites(['b'], null).map((c) => c.pageId)).toEqual(['a']);
		// b was seen: forgotten.
		expect(pendingWikiWrites([], null).map((c) => c.pageId)).toEqual(['a']);
		expect(latestWikiToken()).toBe('c1.1.10');
	});
	it('one entry per page; a delete or an epoch change drops them', () => {
		rememberWikiWrite({ pageId: 'a', projectKey: 'AUTH', title: 'A', after: 'c1.1.10' });
		rememberWikiWrite({ pageId: 'a', projectKey: 'AUTH', title: 'A2', after: 'c1.1.12' });
		expect(pendingWikiWrites([], null)).toMatchObject([{ pageId: 'a', title: 'A2', after: 'c1.1.12' }]);
		forgetWikiWrite('a');
		expect(pendingWikiWrites([], null)).toEqual([]);
		rememberWikiWrite({ pageId: 'c', projectKey: 'AUTH', title: 'C', after: 'c1.1.13' });
		clearWikiWrites();
		expect(latestWikiToken()).toBeNull();
	});
	it('ignores a broken store', () => {
		sessionStorage.setItem('ox.wikiJustWritten', '{"not":"a list"}');
		expect(pendingWikiWrites([], null)).toEqual([]);
	});
});

describe('WikiSearch', () => {
	const urls: string[] = [];
	beforeEach(() => {
		sessionStorage.clear();
		urls.length = 0;
	});
	afterEach(() => setFetch((i, init) => fetch(i, init)));

	const item = {
		project: { id: 'p1', key: 'AUTH', name: 'Auth' },
		page_id: 'w1',
		title: '部署流程',
		url_title: '%E9%83%A8%E7%BD%B2%E6%B5%81%E7%A8%8B',
		snippet: [
			{ text: '今天<b>討論</b>了', hit: false },
			{ text: '部署', hit: true },
			{ text: '的細節', hit: false }
		],
		updated_at: '2026-11-16T03:04:05Z'
	};
	const ok = (data: unknown) => json(200, { data });

	it('renders the card with the matched words bold, as text', async () => {
		setFetch(async (u) => (urls.push(u), ok({ outcome: 'ok', available: true, consistency: { status: 'eventual' }, items: [item], next_cursor: null })));
		const { container } = render(WikiSearch, { q: '部署', project: 'AUTH' });
		const link = await screen.findByRole('link', { name: '部署流程' });
		expect(link.getAttribute('href')).toBe(`/project/AUTH/wiki/${item.url_title}`);
		expect(screen.getByTestId('wiki-hit-project').textContent).toBe('AUTH');
		const snip = screen.getByTestId('wiki-hit-snippet');
		// The server's text is never markup.
		expect(snip.textContent).toBe('今天<b>討論</b>了部署的細節');
		expect([...snip.querySelectorAll('b')].map((b) => b.textContent)).toEqual(['部署']);
		expect(container.textContent).toContain('更新於');
		const sent = new URL(urls[0], 'http://x');
		expect(sent.pathname).toBe('/api/v1/wiki/search');
		expect(sent.searchParams.get('q')).toBe('部署');
		expect(sent.searchParams.get('project')).toBe('AUTH');
		expect(sent.searchParams.has('consistent_after')).toBe(false);
	});

	it('says nothing matched', async () => {
		setFetch(async () => ok({ outcome: 'ok', available: true, consistency: { status: 'eventual' }, items: [], next_cursor: null }));
		render(WikiSearch, { q: 'zzz', project: null });
		expect((await screen.findByTestId('wiki-empty-result')).textContent).toBe('沒有符合的 wiki 頁面。');
	});

	it('waits for a page just written; lagging is 「同步中」 with the page still reachable, not 「找不到」', async () => {
		rememberWikiWrite({ pageId: 'w9', projectKey: 'AUTH', title: '新頁 一', after: 'c1.5.42' });
		let lag = true;
		setFetch(async (u) => {
			urls.push(u);
			return lag
				? ok({ outcome: 'search_lagging', available: true, consistency: { status: 'lagging', retry_after_ms: 500 }, items: [], next_cursor: null })
				: ok({ outcome: 'ok', available: true, consistency: { status: 'satisfied', waited_ms: 3 }, items: [{ ...item, page_id: 'w9', title: '新頁 一' }], next_cursor: null });
		});
		render(WikiSearch, { q: '新頁', project: null });
		await screen.findByTestId('wiki-lagging');
		expect(new URL(urls[0], 'http://x').searchParams.get('consistent_after')).toBe('c1.5.42');
		expect(screen.queryByTestId('wiki-empty-result')).toBeNull();
		const card = screen.getByTestId('wiki-just-written');
		expect(card.textContent).toContain('剛寫入・同步中');
		expect(card.querySelector('a')?.getAttribute('href')).toBe('/project/AUTH/wiki/%E6%96%B0%E9%A0%81_%E4%B8%80');

		lag = false;
		screen.getByTestId('wiki-retry').click();
		await screen.findByTestId('wiki-hit');
		await waitFor(() => expect(screen.queryByTestId('wiki-just-written')).toBeNull());
		expect(latestWikiToken()).toBeNull();
	});

	it('drops tokens from an old epoch and searches without them', async () => {
		rememberWikiWrite({ pageId: 'w9', projectKey: 'AUTH', title: 'X', after: 'c1.4.1' });
		setFetch(async (u) => {
			urls.push(u);
			return new URL(u, 'http://x').searchParams.has('consistent_after')
				? json(409, { error: { code: 'consistency_epoch_changed', message: 'epoch', details: {}, next_actions: [], retryable: false, request_id: 'r' } })
				: ok({ outcome: 'ok', available: true, consistency: { status: 'eventual' }, items: [item], next_cursor: null });
		});
		render(WikiSearch, { q: '部署', project: null });
		await screen.findByTestId('wiki-hit');
		expect(urls).toHaveLength(2);
		expect(screen.queryByTestId('error')).toBeNull();
	});

	it('says the wiki search is down, not the issues', async () => {
		setFetch(async () => json(503, { error: { code: 'search_unavailable', message: 'x', details: {}, next_actions: [], retryable: true, request_id: 'r' } }));
		render(WikiSearch, { q: '部署', project: null });
		expect((await screen.findByTestId('error')).textContent).toContain('Wiki 搜尋暫時無法使用');
	});

	it('asks for words when only filters were typed, and says which ones do not apply', async () => {
		setFetch(async (u) => (urls.push(u), ok({})));
		render(WikiSearch, { q: null, project: 'AUTH', ignored: ['status:'] });
		expect(screen.getByTestId('wiki-no-query').textContent).toBe('輸入要在 wiki 找的字。');
		expect(screen.getByTestId('wiki-ignored').textContent).toContain('status:');
		expect(urls).toEqual([]);
	});
});

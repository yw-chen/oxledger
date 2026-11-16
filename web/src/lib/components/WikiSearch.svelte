<script lang="ts">
	// The search page's Wiki tab (design 06 §6.7): `GET /wiki/search`, one card
	// per page (project badge, title linking to the page, the snippet with the
	// matched words bold, updated time), 「還有更多」 by cursor, and 03 Q-S10:
	// a page this tab just wrote waits for the index (`consistent_after`) and,
	// while the index has not got it, shows as 「剛寫入・同步中」, not missing.
	// The snippet is rendered as text parts, never as HTML.
	import type { WikiSearchItem, WikiSearchResult } from '$api/types';
	import { untrack } from 'svelte';
	import { api, ApiError } from '#lib/api.js';
	import { fmtTime } from '#lib/format.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import { clearWikiWrites, latestWikiToken, pendingWikiWrites, wikiHitHref, type JustWritten } from '#lib/wikisearch.js';
	import { wikiPath } from '#lib/wiki.js';

	let { q, project, ignored = [] }: { q: string | null; project: string | null; ignored?: string[] } = $props();

	let items = $state<WikiSearchItem[]>([]);
	let result = $state<WikiSearchResult | null>(null);
	let error = $state<unknown>(null);
	let loading = $state(false);
	let slow = $state(false);
	let justWritten = $state<JustWritten[]>([]);
	let seq = 0;

	async function run(more = false, retried = false) {
		if (!q) return;
		const mine = ++seq;
		error = null;
		loading = true;
		slow = false;
		const timer = setTimeout(() => (slow = true), 400);
		const after = more ? null : latestWikiToken();
		try {
			const r = await api('GET /wiki/search', {
				query: { q, project: project ?? undefined, cursor: more ? (result?.next_cursor ?? undefined) : undefined, consistent_after: after ?? undefined }
			});
			if (mine !== seq) return;
			items = more ? [...items, ...r.items] : r.items;
			result = r;
			justWritten = pendingWikiWrites(
				items.map((i) => i.page_id),
				project
			);
		} catch (e) {
			if (mine !== seq) return;
			// The tokens died with the old epoch (data reset): search without them.
			if (e instanceof ApiError && (e.code === 'consistency_epoch_changed' || e.code === 'invalid_consistency_token') && after && !retried) {
				clearWikiWrites();
				clearTimeout(timer);
				return run(more, true);
			}
			error = e;
			if (!more) {
				items = [];
				result = null;
			}
		} finally {
			clearTimeout(timer);
			if (mine === seq) loading = false;
		}
	}

	$effect(() => {
		// Re-run when the query or the project changes.
		void q;
		void project;
		items = [];
		result = null;
		error = null;
		justWritten = [];
		untrack(() => run());
	});

	const unavailable = $derived(error instanceof ApiError && error.code === 'search_unavailable');
</script>

{#if ignored.length}
	<div class="hint" style="margin-bottom:6px" data-testid="wiki-ignored">Wiki 搜尋只看 project: 與全文；{ignored.join(' ')} 只用在議題。</div>
{/if}
{#if !q}
	<div class="empty" data-testid="wiki-no-query">輸入要在 wiki 找的字。</div>
{:else}
	{#if unavailable}
		<div class="err" role="alert" data-testid="error">
			<b>Wiki 搜尋暫時無法使用；wiki 頁面本身仍可開啟</b>
			<div class="chg">沒有任何變更。</div>
			<button class="btn sm" style="margin-top:6px" onclick={() => run()}>重試</button>
		</div>
	{:else}
		<ErrorBox {error} onaction={() => run()} />
	{/if}
	{#if loading && slow}
		<div class="sync" data-testid="search-slow"><span class="spin"></span>搜尋中…</div>
	{/if}
	{#each justWritten as c (c.pageId)}
		<div class="card p" data-testid="wiki-just-written">
			<div class="row" style="gap:8px">
				<span class="chip c-prop">剛寫入・同步中</span>
				<span class="chip c-dim">{c.projectKey}</span>
				<a href={wikiPath(c.projectKey, c.title)}>{c.title}</a>
			</div>
			<p class="hint" style="margin:4px 0 0">你剛寫入的頁可能還沒進搜尋索引，或不符合這次的查詢；頁面本身已經可以開啟。</p>
		</div>
	{/each}
	{#if result?.outcome === 'search_lagging'}
		<div class="card p" data-testid="wiki-lagging">
			<b>剛寫入・搜尋同步中</b>
			<p class="muted" style="margin:4px 0;font-size:13px">剛寫入的內容還沒進搜尋索引；這不是寫入失敗，也不是「找不到」。頁面本身已經可以開啟。</p>
			<button class="btn sm" onclick={() => run()} disabled={loading} data-testid="wiki-retry">再找一次</button>
		</div>
	{:else if result}
		{#each items as it (it.page_id)}
			<div class="card" style="padding:8px 12px" data-testid="wiki-hit">
				<div class="row" style="gap:8px">
					<span class="chip c-dim" title={it.project.name} data-testid="wiki-hit-project">{it.project.key}</span>
					<a href={wikiHitHref(it)} data-testid="wiki-hit-title"><b>{it.title}</b></a>
					<span style="flex:1"></span>
					<span class="hint" title={it.updated_at}>更新於 {fmtTime(it.updated_at)}</span>
				</div>
				{#if it.snippet.length}
					<div class="hint" style="margin-top:3px" data-testid="wiki-hit-snippet">
						{#each it.snippet as part, i (i)}{#if part.hit}<b>{part.text}</b>{:else}{part.text}{/if}{/each}
					</div>
				{/if}
			</div>
		{:else}
			<div class="empty" data-testid="wiki-empty-result">沒有符合的 wiki 頁面。</div>
		{/each}
		{#if result.next_cursor}
			<div class="pager"><button class="btn" onclick={() => run(true)} disabled={loading} data-testid="wiki-more">還有更多</button><span class="hint">不顯示總數</span></div>
		{/if}
	{/if}
{/if}

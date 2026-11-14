<script lang="ts">
	// A saved query tab (R-T6): run as an ordinary search with the viewer's
	// own visibility; no stored results, no total.
	import type { SearchItem, SearchResult, Tab } from '$api/types';
	import { api } from '#lib/api.js';
	import ErrorBox from './ErrorBox.svelte';
	import IssueList from './IssueList.svelte';

	let { projectId, tab }: { projectId: string; tab: Tab } = $props();
	let items = $state<SearchItem[]>([]);
	let result = $state<SearchResult | null>(null);
	let error = $state<unknown>(null);
	let loading = $state(false);

	async function load(more = false) {
		loading = true;
		error = null;
		try {
			const r = await api('GET /projects/{id}/tabs/{tab}/issues', { path: { id: projectId, tab: tab.id }, query: { cursor: more ? result?.next_cursor : undefined } });
			items = more ? [...items, ...r.items] : r.items;
			result = r;
		} catch (e) {
			error = e;
		} finally {
			loading = false;
		}
	}
	$effect(() => {
		void tab.id;
		void projectId;
		load();
	});
</script>

<div class="hint" style="margin:8px 0">條件：<span class="mono">{tab.query}</span>（用你自己的權限即時查詢）</div>
<ErrorBox {error} onaction={() => load()} />
{#if result?.outcome === 'search_lagging'}<div class="banner i">搜尋同步中，剛寫入的內容可能還沒出現。</div>{/if}
{#if result?.truncated}<div class="banner w">結果太多，系統只看過前面一部分候選；可能還有更多相符的單。</div>{/if}
{#if result}<IssueList {items} />{/if}
{#if result?.next_cursor}<div class="pager"><button class="btn" onclick={() => load(true)} disabled={loading}>還有更多</button><span class="hint">不顯示總數</span></div>{/if}

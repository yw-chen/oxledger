<script lang="ts">
	// 開發（GitHub）分頁 (§19.7): issues with PRs, newest PR activity first,
	// only issues the viewer can see, never a total. GitHub's text is shown
	// as written; a repository the App lost shows data "as of" its last sync.
	import type { ProjectDev } from '$api/types';
	import { api } from '$lib/api';
	import { fmtShort } from '$lib/format';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from './ErrorBox.svelte';

	let { projectId }: { projectId: string } = $props();
	let filter = $state('all');
	let includeHidden = $state(false);
	let data = $state<ProjectDev | null>(null);
	let items = $state<ProjectDev['items']>([]);
	let error = $state<unknown>(null);

	async function load(more = false) {
		error = null;
		try {
			const r = await api('GET /projects/{id}/dev', { path: { id: projectId }, query: { filter, include_hidden: includeHidden || undefined, cursor: more ? data?.next_cursor : undefined } });
			items = more ? [...items, ...r.items] : r.items;
			data = r;
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void projectId;
		void filter;
		void includeHidden;
		load();
	});
	async function hide(prId: string, hidden: boolean) {
		try {
			await api(hidden ? 'POST /projects/{id}/dev/prs/{pr_id}/hide' : 'DELETE /projects/{id}/dev/prs/{pr_id}/hide', { path: { id: projectId, pr_id: prId } });
			toast(hidden ? '已隱藏這個 PR' : '已取消隱藏', 'ok');
			await load();
		} catch (e) {
			error = e;
		}
	}
	const FILTERS: [string, string][] = [
		['all', '全部'],
		['ci_failure', 'CI 失敗'],
		['review', '等 review'],
		['merged_open', '已合併但單沒關']
	];
	const PR: Record<string, string> = { open: '開啟', draft: '草稿', merged: '已合併', closed: '已關閉' };
	const CI: Record<string, { t: string; c: string }> = { success: { t: 'CI 通過', c: 'c-ok' }, failure: { t: 'CI 失敗', c: 'c-bad' }, pending: { t: 'CI 執行中', c: 'c-warn' } };
	const REVIEW: Record<string, string> = { approved: '已核可', changes_requested: '要求修改', pending: '等 review', none: '沒有 review' };
</script>

<div class="row" style="gap:6px;margin:8px 0">
	{#each FILTERS as [k, l] (k)}<button class="btn sm" class:on={filter === k} onclick={() => (filter = k)}>{l}</button>{/each}
	{#if data?.can_manage}<label class="row" style="gap:4px;margin-left:8px"><input type="checkbox" bind:checked={includeHidden} /> 顯示隱藏的 PR</label>{/if}
</div>
<ErrorBox {error} onaction={() => load()} />
{#each items as it (it.key)}
	<div class="card" data-testid="dev-issue">
		<div class="row" style="gap:8px">
			<a class="key" href="/issue/{it.key}">{it.key}</a>
			<b>{it.subject}</b>
			<span class="chip c-dim">{it.status}</span>
			{#if it.merged_open}<span class="chip c-warn">PR 已合併，單還沒關</span>{/if}
		</div>
		{#each it.prs as pr (pr.pr_id)}
			<div class="row hint" style="gap:6px;margin-top:3px">
				<a href={pr.url} target="_blank" rel="noopener noreferrer">{pr.repo}#{pr.number}</a>
				<span>{pr.title}</span>
				<span class="chip c-dim">{PR[pr.state] ?? pr.state}</span>
				{#if pr.ci_state}<span class="chip {CI[pr.ci_state]?.c ?? 'c-dim'}">{CI[pr.ci_state]?.t ?? pr.ci_state}</span>{/if}
				<span>{REVIEW[pr.review_state] ?? pr.review_state}</span>
				{#if !pr.accessible}<span class="chip c-dim" title="App 已無法讀取這個 repo">資料停在 {pr.synced_at ? fmtShort(pr.synced_at) : '—'}</span>{/if}
				{#if pr.hidden}<span class="chip c-dim">已隱藏</span>{/if}
				{#if data?.can_manage}<button class="btn sm ghost" onclick={() => hide(pr.pr_id, !pr.hidden)}>{pr.hidden ? '取消隱藏' : '隱藏'}</button>{/if}
			</div>
		{/each}
		{#if it.truncated}<div class="hint">還有更多 PR，請到議題頁看。</div>{/if}
	</div>
{:else}
	{#if !error}<div class="empty">沒有連到 PR 的單（或專案還沒連動 repo）。</div>{/if}
{/each}
{#if data?.next_cursor}<div class="pager"><button class="btn" onclick={() => load(true)}>還有更多</button><span class="hint">不顯示總數</span></div>{/if}

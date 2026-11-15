<script lang="ts">
	// The project's 「Wiki」 tab (design 06 §6.3, R-W13): the start page (the
	// set one, else the page titled "Wiki"); with none, the list of pages, or
	// 「這個專案還沒有 wiki 頁面。」. Wiki managers also get the pages deleted
	// in the last 30 days, to look at and restore (§6.5, R-W16).
	import { goto } from '$app/navigation';
	import type { WikiDeletedItem, WikiIndex } from '$api/types';
	import { api, newOperationId } from '#lib/api.js';
	import { NOT_FOUND } from '#lib/errors.js';
	import { fmtShort } from '#lib/format.js';
	import { toast } from '#lib/toast.svelte.js';
	import { buildTree, flatTree, wikiPath } from '#lib/wiki.js';
	import { runWiki } from '#lib/wikiwrite.svelte.js';
	import ErrorBox from './ErrorBox.svelte';
	import WikiLayout from './WikiLayout.svelte';
	import WikiPageView from './WikiPageView.svelte';
	import WikiUndoBar from './WikiUndoBar.svelte';

	let { projectKey }: { projectKey: string } = $props();

	let index = $state<WikiIndex | null>(null);
	let error = $state<unknown>(null);
	let deleted = $state<WikiDeletedItem[] | null>(null);
	let deletedError = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		try {
			index = await api('GET /projects/{id}/wiki', { path: { id: projectKey } });
		} catch (e) {
			error = e;
			index = null;
		}
		deleted = null;
		if (index?.can.manage) {
			try {
				deleted = (await api('GET /projects/{id}/wiki/deleted', { path: { id: projectKey } })).items;
			} catch (e) {
				deletedError = e;
			}
		}
	}
	$effect(() => {
		void projectKey;
		load();
	});

	async function restore(it: WikiDeletedItem) {
		busy = true;
		deletedError = null;
		try {
			const { effect } = await runWiki(projectKey, { kind: 'wiki_restore', op_id: newOperationId(), target: { project: projectKey, page_id: it.id } }, null, `還原〈${it.title}〉`);
			toast(`已還原〈${it.title}〉`, 'ok');
			await goto(wikiPath(projectKey, effect?.title ?? it.title));
		} catch (e) {
			deletedError = e;
		} finally {
			busy = false;
		}
	}
	const pages = $derived(index ? flatTree(buildTree(index.tree)) : []);
</script>

{#snippet trash()}
	{#if index?.can.manage && deleted !== null}
		<section class="card" style="margin-top:14px" data-testid="wiki-deleted">
			<h2 style="font-size:14px;margin:0 0 6px">最近刪除（30 天內）</h2>
			<ErrorBox error={deletedError} />
			{#each deleted as it (it.id)}
				<div class="row" style="gap:8px;padding:3px 0" data-testid="wiki-deleted-item">
					<b>{it.title}</b>
					<span class="hint">{it.deleted_by?.name ?? '（不明）'} 於 {fmtShort(it.deleted_at)} 刪除</span>
					<span style="flex:1"></span>
					<a class="btn sm" href="/project/{projectKey}/wiki-deleted/{it.id}">檢視</a>
					<button class="btn sm" onclick={() => restore(it)} disabled={busy || it.title_taken || index.read_only} title={it.title_taken ? '已經有同名的頁面，請先把那一頁改名' : index.read_only ? '專案已關閉，wiki 唯讀' : ''} data-testid="wiki-restore">還原</button>
				</div>
			{:else}
				<div class="hint">沒有最近刪除的頁面。</div>
			{/each}
		</section>
	{/if}
{/snippet}

{#if error}
	<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
{:else if !index}
	<div class="sync"><span class="spin"></span>載入中…</div>
{:else if index.start_page}
	{#key index.start_page.id}
		<WikiPageView {projectKey} title={index.start_page.title} {index} below={trash} />
	{/key}
{:else}
	<WikiLayout {projectKey} {index}>
		<WikiUndoBar {projectKey} onundone={load} />
		{#if index.read_only}<div class="banner w" data-testid="wiki-readonly">專案已關閉，wiki 唯讀。</div>{/if}
		{#if !pages.length}
			<div class="card" data-testid="wiki-empty">
				<p>這個專案還沒有 wiki 頁面。</p>
				{#if index.can.create}<a class="btn pri" href={wikiPath(projectKey, 'Wiki', '/edit')} data-testid="wiki-create-first">建立第一頁</a>{/if}
			</div>
		{:else}
			<div class="card" data-testid="wiki-pages">
				<p class="hint" style="margin-top:0">這個 wiki 還沒有起始頁（標題為「Wiki」的頁會當作起始頁）。所有頁面：</p>
				<ul class="plain">
					{#each pages as p (p.id)}<li style="padding-left:{p.depth * 16}px"><a href={wikiPath(projectKey, p.title)}>{p.title}</a></li>{/each}
				</ul>
			</div>
		{/if}
		{@render trash()}
	</WikiLayout>
{/if}

<style>
	.plain { list-style: none; margin: 0; padding: 0; }
</style>

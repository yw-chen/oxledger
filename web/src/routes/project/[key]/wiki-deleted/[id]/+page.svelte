<script lang="ts">
	// Design 06 §6.5 / R-W16: a page deleted in the last 30 days, read-only for
	// wiki managers (a grey bar 「這一頁已刪除」), with 「還原」.
	import { goto } from '$app/navigation';
	import { page as route } from '$app/state';
	import type { WikiDeletedPage, WikiIndex } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { NOT_FOUND } from '#lib/errors.js';
	import { fmtShort } from '#lib/format.js';
	import { toast } from '#lib/toast.svelte.js';
	import { wikiPath } from '#lib/wiki.js';
	import { runWiki } from '#lib/wikiwrite.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import RichText from '#lib/components/RichText.svelte';
	import WikiLayout from '#lib/components/WikiLayout.svelte';

	const key = $derived((route.params.key ?? '').toUpperCase());
	const id = $derived(route.params.id ?? '');

	let index = $state<WikiIndex | null>(null);
	let p = $state<WikiDeletedPage | null>(null);
	let error = $state<unknown>(null);
	let actionError = $state<unknown>(null);
	let busy = $state(false);

	$effect(() => {
		const k = key;
		const i = id;
		error = null;
		p = null;
		Promise.all([api('GET /projects/{id}/wiki', { path: { id: k } }), api('GET /projects/{id}/wiki/deleted/{page_id}', { path: { id: k, page_id: i } })])
			.then(([ix, d]) => {
				index = ix;
				p = d;
			})
			.catch((e) => (error = e));
	});
	async function restore() {
		if (!p) return;
		busy = true;
		actionError = null;
		try {
			const { effect } = await runWiki(key, { kind: 'wiki_restore', op_id: newOperationId(), target: { project: key, page_id: p.id } }, null, `還原〈${p.title}〉`);
			toast(`已還原〈${p.title}〉`, 'ok');
			await goto(wikiPath(key, effect?.title ?? p.title));
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}
	const forbidden = $derived(error instanceof ApiError && error.code === 'forbidden_op');
</script>

<svelte:head><title>已刪除的頁・{key} Wiki・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active="wiki">
	{#if forbidden}
		<div class="empty">只有 wiki 管理者能看已刪除的頁。</div>
	{:else if error}
		<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
	{:else if p}
		<WikiLayout projectKey={key} {index}>
			<a class="hint" href="/project/{key}/wiki">← Wiki</a>
			<div class="banner row" style="gap:8px;margin-top:6px;background:var(--bg2);border:1px solid var(--line)" data-testid="wiki-deleted-bar">
				<span>這一頁已刪除（{p.deleted_by?.name ?? '（不明）'} 於 {fmtShort(p.deleted_at)}）。</span>
				<span style="flex:1"></span>
				<button class="btn sm pri" onclick={restore} disabled={busy || p.title_taken || !!index?.read_only} title={p.title_taken ? '已經有同名的頁面，請先把那一頁改名' : index?.read_only ? '專案已關閉，wiki 唯讀' : ''} data-testid="wiki-restore">還原</button>
			</div>
			<ErrorBox error={actionError} />
			<h1 style="margin:6px 0 2px">{p.title}</h1>
			<div class="hint">第 {p.version} 版</div>
			<div class="card" style="margin-top:8px"><RichText html={p.body_html} text={p.body} testid="wiki-body" /></div>
		</WikiLayout>
	{:else}
		<div class="sync"><span class="spin"></span>載入中…</div>
	{/if}
</ProjectFrame>

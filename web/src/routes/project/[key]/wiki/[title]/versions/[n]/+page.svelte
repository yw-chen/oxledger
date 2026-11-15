<script lang="ts">
	// Design 06 §6.6: one version of a page, with a yellow bar 「你在看第 8 版
	// （目前是第 12 版）」, 「回復到這一版」 (off for a redacted version) and
	// 「看與目前的差異」.
	import { goto } from '$app/navigation';
	import { page as route } from '$app/state';
	import type { WikiIndex, WikiPage, WikiVersionView } from '$api/types';
	import { api } from '#lib/api.js';
	import { FORBIDDEN, NOT_FOUND } from '#lib/errors.js';
	import { fmtShort } from '#lib/format.js';
	import { toast } from '#lib/toast.svelte.js';
	import { titleFromParam, urlTitle, wikiPath } from '#lib/wiki.js';
	import { revertTo } from '#lib/wikiwrite.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import RichText from '#lib/components/RichText.svelte';
	import WikiLayout from '#lib/components/WikiLayout.svelte';

	const key = $derived((route.params.key ?? '').toUpperCase());
	const title = $derived(route.params.title ?? '');
	const n = $derived(Number(route.params.n));

	let index = $state<WikiIndex | null>(null);
	let page = $state<WikiPage | null>(null);
	let v = $state<WikiVersionView | null>(null);
	let error = $state<unknown>(null);
	let actionError = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		v = null;
		try {
			const t = urlTitle(title);
			const [ix, pg] = await Promise.all([api('GET /projects/{id}/wiki', { path: { id: key } }), api('GET /projects/{id}/wiki/pages/{title}', { path: { id: key, title: t } })]);
			index = ix;
			page = pg;
			v = await api('GET /projects/{id}/wiki/pages/{title}/versions/{n}', { path: { id: key, title: t, n } });
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void key;
		void title;
		void n;
		load();
	});
	const revertWhy = $derived.by(() => {
		if (!v || !page) return '';
		if (v.redacted) return '這一版已遮蔽';
		if (v.version === v.current_version) return '這是目前的版本';
		if (!page.can.edit) return page.read_only ? '專案已關閉，wiki 唯讀' : page.protected && index?.can.create ? '這一頁受保護，只有有保護權限的人能編輯' : FORBIDDEN;
		return '';
	});
	async function revert() {
		if (!page || !v || revertWhy) return;
		if (!confirm(`要把〈${page.title}〉回復到第 ${v.version} 版嗎？會新增一個版本，現在的內容仍保留在歷史裡。`)) return;
		busy = true;
		actionError = null;
		try {
			const { effect } = await revertTo(key, page, v.version);
			toast(effect?.changed === false ? '內容和目前相同，沒有變更' : `已回復到第 ${v.version} 版`, 'ok');
			await goto(wikiPath(key, effect?.title ?? page.title));
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>第 {n} 版・{titleFromParam(title)}・{key} Wiki・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active="wiki">
	{#if !index && error}
		<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
	{:else}
		<WikiLayout projectKey={key} {index} currentId={page?.id ?? null}>
			{#if page}<a class="hint" href={wikiPath(key, page.title, '/history')}>← 〈{page.title}〉的歷史</a>{/if}
			<ErrorBox {error} />
			{#if v && page}
				<div class="banner w row" style="gap:8px;margin-top:6px" data-testid="wiki-version-bar">
					<span>你在看第 {v.version} 版（目前是第 {v.current_version} 版）</span>
					<span style="flex:1"></span>
					<button class="btn sm" onclick={revert} disabled={!!revertWhy || busy} title={revertWhy} data-testid="wiki-version-revert">回復到這一版</button>
					{#if v.version !== v.current_version}<a class="btn sm" href="{wikiPath(key, page.title, '/diff')}?from={v.version}&to={v.current_version}" data-testid="wiki-version-diff">看與目前的差異</a>{/if}
				</div>
				<ErrorBox error={actionError} />
				<h1 style="margin:6px 0 2px">{v.title}</h1>
				<div class="hint">第 {v.version} 版・{v.author?.name ?? '（不明）'}{#if v.author?.kind === 'agent'}<span class="agent-label">agent</span>{/if} 於 {fmtShort(v.created_at)}{v.comment ? `・${v.comment}` : ''}</div>
				<div class="card" style="margin-top:8px">
					{#if v.redacted}<div class="dim" data-testid="wiki-body">[已遮蔽]</div>{:else}<RichText html={v.body_html} text={v.body} testid="wiki-body" />{/if}
				</div>
			{/if}
		</WikiLayout>
	{/if}
</ProjectFrame>

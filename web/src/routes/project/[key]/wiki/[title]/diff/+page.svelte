<script lang="ts">
	// Design 06 §6.6: the difference between two versions (`?from=&to=`;
	// default the newest against the one before it, `from=0` the empty page).
	import { page as route } from '$app/state';
	import type { WikiDiff, WikiIndex, WikiPage } from '$api/types';
	import { api } from '#lib/api.js';
	import { NOT_FOUND } from '#lib/errors.js';
	import { titleFromParam, urlTitle, wikiPath } from '#lib/wiki.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import WikiDiffView from '#lib/components/WikiDiffView.svelte';
	import WikiLayout from '#lib/components/WikiLayout.svelte';

	const key = $derived((route.params.key ?? '').toUpperCase());
	const title = $derived(route.params.title ?? '');
	const qFrom = $derived(route.url.searchParams.get('from'));
	const qTo = $derived(route.url.searchParams.get('to'));

	let index = $state<WikiIndex | null>(null);
	let page = $state<WikiPage | null>(null);
	let diff = $state<WikiDiff | null>(null);
	let error = $state<unknown>(null);

	async function load() {
		error = null;
		diff = null;
		try {
			const t = urlTitle(title);
			const [ix, pg] = await Promise.all([api('GET /projects/{id}/wiki', { path: { id: key } }), api('GET /projects/{id}/wiki/pages/{title}', { path: { id: key, title: t } })]);
			index = ix;
			page = pg;
			diff = await api('GET /projects/{id}/wiki/pages/{title}/diff', { path: { id: key, title: t }, query: { from: qFrom, to: qTo } });
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void key;
		void title;
		void qFrom;
		void qTo;
		load();
	});
	/** The newer side's text, to open the folded stretches. */
	async function newer(): Promise<string> {
		if (!diff || !page) return '';
		if (diff.to === page.version) return page.body;
		return (await api('GET /projects/{id}/wiki/pages/{title}/versions/{n}', { path: { id: key, title: urlTitle(page.title), n: diff.to } })).body;
	}
</script>

<svelte:head><title>差異・{titleFromParam(title)}・{key} Wiki・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active="wiki">
	{#if !index && error}
		<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
	{:else}
		<WikiLayout projectKey={key} {index} currentId={page?.id ?? null}>
			{#if page}
				<a class="hint" href={wikiPath(key, page.title, '/history')}>← 〈{page.title}〉的歷史</a>
				<h1 style="margin:4px 0 8px">〈{page.title}〉{#if diff}第 {diff.from} 版 → 第 {diff.to} 版{/if}</h1>
			{/if}
			<ErrorBox {error} />
			{#if diff}
				{#key `${diff.from}-${diff.to}`}
					<WikiDiffView hunks={diff.hunks} truncated={diff.truncated} loadNew={newer} leftLabel={diff.from === 0 ? '（空白）' : `第 ${diff.from} 版`} rightLabel="第 {diff.to} 版" />
				{/key}
			{/if}
		</WikiLayout>
	{/if}
</ProjectFrame>

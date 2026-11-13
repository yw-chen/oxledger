<script lang="ts">
	// A project and one of its shared tabs (§18.6): 時程, 里程碑, 議題, 開發,
	// saved queries (run with the viewer's own visibility, R-T6) and links.
	import { page } from '$app/state';
	import type { TabSettings } from '$api/types';
	import { api } from '$lib/api';
	import { projectByKey } from '$lib/session.svelte';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import Timeline from '$lib/components/Timeline.svelte';
	import Milestones from '$lib/components/Milestones.svelte';
	import DevTab from '$lib/components/DevTab.svelte';
	import QueryTab from '$lib/components/QueryTab.svelte';
	import ViewPage from '$lib/components/ViewPage.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	const project = $derived(projectByKey(key));
	let tabs = $state<TabSettings | null>(null);
	let error = $state<unknown>(null);
	$effect(() => {
		const k = key;
		error = null;
		api('GET /projects/{id}/tabs', { path: { id: k } })
			.then((t) => (tabs = t))
			.catch((e) => (error = e));
	});
	const tabId = $derived(page.params.tab ?? tabs?.default_tab ?? null);
	const tab = $derived(tabs?.tabs.find((t) => t.id === tabId) ?? null);
	const host = (u: string) => {
		try {
			return new URL(u).host;
		} catch {
			return u;
		}
	};
</script>

<svelte:head><title>{key}{tab ? `・${tab.title}` : ''}・oxledger</title></svelte:head>
{#if error}
	<div class="page"><ErrorBox {error} /></div>
{:else}
	<ProjectFrame projectKey={key} active={tabId}>
		{#if !tabs}
			<div class="sync"><span class="spin"></span>載入中…</div>
		{:else if !tab}
			<div class="empty">沒有這個分頁（可能已被管理者移除）。</div>
		{:else if tab.id === 'timeline'}
			<Timeline projectId={project?.id ?? key} />
		{:else if tab.id === 'milestones'}
			<Milestones projectId={project?.id ?? key} projectKey={key} />
		{:else if tab.id === 'issues'}
			{#if project}{#key project.id}<ViewPage viewId={`builtin:project_issues:${project.id}`} projectKey={key} inFrame />{/key}{/if}
		{:else if tab.id === 'board'}
			{#if project}{#key tab.view_id ?? project.id}<ViewPage viewId={tab.view_id ?? `builtin:project_board:${project.id}`} projectKey={key} inFrame />{/key}{/if}
		{:else if tab.kind === 'view' && tab.view_id}
			{#key tab.view_id}<ViewPage viewId={tab.view_id} projectKey={key} inFrame />{/key}
		{:else if tab.id === 'dev'}
			<DevTab projectId={project?.id ?? key} />
		{:else if tab.id === 'plan'}
			<div class="empty">規劃（心智圖）第一版不提供。</div>
		{:else if tab.kind === 'query'}
			<QueryTab projectId={project?.id ?? key} {tab} />
		{:else if tab.kind === 'link' && tab.url}
			<div class="card">這是外部連結分頁，會開到 <b>{host(tab.url)}</b>：<a href={tab.url} target="_blank" rel="noopener noreferrer">{tab.url} ↗</a></div>
		{/if}
	</ProjectFrame>
{/if}

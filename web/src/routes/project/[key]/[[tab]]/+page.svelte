<script lang="ts">
	// A project and one of its shared tabs (§18.6): 時程, 里程碑, 議題, 開發, Wiki,
	// saved queries (run with the viewer's own visibility, R-T6) and links.
	import { page } from '$app/state';
	import type { TabSettings } from '$api/types';
	import { api } from '#lib/api.js';
	import { projectByKey } from '#lib/session.svelte.js';
	import { me } from '#lib/me.svelte.js';
	import { myDefaults, personalTab } from '#lib/home.js';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import Timeline from '#lib/components/Timeline.svelte';
	import Milestones from '#lib/components/Milestones.svelte';
	import DevTab from '#lib/components/DevTab.svelte';
	import QueryTab from '#lib/components/QueryTab.svelte';
	import ViewPage from '#lib/components/ViewPage.svelte';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import TriageInbox from '#lib/components/TriageInbox.svelte';
	import TimeReport from '#lib/components/TimeReport.svelte';
	import AiTeamList from '#lib/components/AiTeamList.svelte';
	import WikiHome from '#lib/components/WikiHome.svelte';

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
	// The generic `*` (no interface role chosen) would hide the project's own default tab; only role templates and per-project choices count.
	const mine = $derived(myDefaults(me.prefs));
	// §3.8 default_views: the person's own default for this project (or for all) wins over the project's.
	const tabId = $derived(page.params.tab ?? (tabs ? personalTab(mine, project?.id ?? '', tabs.tabs) : null) ?? tabs?.default_tab ?? null);
	const tab = $derived(tabs?.tabs.find((t) => t.id === tabId) ?? null);
	// §6.4 / §6.8: the Triage inbox and the time report open at their own
	// path even when no tab shows them.
	const special = $derived(page.params.tab === 'triage' || page.params.tab === 'time' || page.params.tab === 'ai-team' ? page.params.tab : null);
	const host = (u: string) => {
		try {
			return new URL(u).host;
		} catch {
			return u;
		}
	};
</script>

<svelte:head><title>{key}{tab ? `・${tab.title}` : special === 'time' ? '・工時' : special === 'triage' ? '・Triage' : special === 'ai-team' ? '・AI 團隊' : ''}・oxledger</title></svelte:head>
{#if error}
	<div class="page"><ErrorBox {error} /></div>
{:else}
	<ProjectFrame projectKey={key} active={tabId}>
		{#if special === 'triage'}
			{#key key}<TriageInbox projectKey={key} projectId={project?.id ?? null} />{/key}
		{:else if special === 'time'}
			{#key key}<TimeReport projectKey={key} />{/key}
		{:else if special === 'ai-team'}
			<div style="padding:12px">{#if project}{#key project.id}<AiTeamList projectId={project.id} />{/key}{/if}</div>
		{:else if !tabs}
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
		{:else if tab.id === 'wiki'}
			{#key key}<WikiHome projectKey={key} />{/key}
		{:else if tab.id === 'plan'}
			<div class="empty">規劃（心智圖）第一版不提供。</div>
		{:else if tab.kind === 'query'}
			<QueryTab projectId={project?.id ?? key} {tab} />
		{:else if tab.kind === 'link' && tab.url}
			<div class="card">這是外部連結分頁，會開到 <b>{host(tab.url)}</b>：<a href={tab.url} target="_blank" rel="noopener noreferrer">{tab.url} ↗</a></div>
		{/if}
	</ProjectFrame>
{/if}

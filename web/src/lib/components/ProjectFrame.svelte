<script lang="ts">
	// The project frame (§18.6): project name and its shared tabs. Issue pages
	// sit inside it; "← 回到〈分頁〉" when the person came from one of them.
	import type { Snippet } from 'svelte';
	import type { TabSettings } from '$api/types';
	import { api } from '#lib/api.js';
	import { projectByKey } from '#lib/session.svelte.js';
	import { current, fromTab, rememberTab } from '#lib/current.svelte.js';

	let {
		projectKey,
		active = null,
		issuePage = false,
		reload = 0,
		children
	}: { projectKey: string; active?: string | null; issuePage?: boolean; reload?: number; children: Snippet } = $props();

	let tabs = $state<TabSettings | null>(null);
	const project = $derived(projectByKey(projectKey));
	const back = $derived(issuePage ? fromTab(projectKey) : null);
	const shownTab = $derived(issuePage ? (back?.tab ?? 'issues') : active);

	$effect(() => {
		current.projectKey = projectKey.toUpperCase();
		return () => {
			current.projectKey = null;
		};
	});
	$effect(() => {
		void reload;
		api('GET /projects/{id}/tabs', { path: { id: projectKey } })
			.then((t) => (tabs = t))
			.catch(() => (tabs = null));
	});
	function remember(id: string, title: string) {
		rememberTab({ project: projectKey, tab: id, title });
	}
</script>

<div class="page" style="padding-top:10px">
	<div class="row" style="gap:8px">
		<span class="mono dim">{projectKey}</span>
		<h1 style="margin:0">{project?.name ?? projectKey}</h1>
		<span style="flex:1"></span>
		<a class="btn sm ghost" href="/project/{projectKey}/ai-team" class:on={active === 'ai-team'} data-testid="project-ai-team">AI 團隊</a>
		<a class="btn sm ghost" href="/project/{projectKey}/time" class:on={active === 'time'} data-testid="project-time">工時</a>
		{#if tabs?.can_manage}<a class="btn sm" href="/project/{projectKey}/settings">專案設定</a>{/if}
	</div>
	{#if tabs}
		<nav class="ptabs" aria-label="專案分頁" data-testid="ptabs">
			{#each tabs.tabs.filter((t) => !t.hidden) as t (t.id)}
				{#if t.kind === 'link' && t.url}
					<a href={t.url} target="_blank" rel="noopener noreferrer" title="開到 {t.url}">{t.title} ↗</a>
				{:else}
					<a href="/project/{projectKey}/{t.id}" class:on={shownTab === t.id} onclick={() => remember(t.id, t.title)} aria-current={shownTab === t.id ? 'page' : undefined}>{t.title}</a>
				{/if}
			{/each}
		</nav>
	{/if}
	{#if back}
		<a href="/project/{projectKey}/{back.tab}" class="hint" data-testid="back-to-tab">← 回到〈{back.title}〉</a>
	{/if}
	{@render children()}
</div>

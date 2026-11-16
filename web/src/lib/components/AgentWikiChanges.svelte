<script lang="ts">
	// Design 06 §6.5 A5 (c): 「agent 最近 7 天改了這些 wiki 頁」. The wiki
	// versions agents wrote, in the wikis you read; hidden when there are none
	// (or the list cannot be read). `project` narrows it to one wiki.
	import type { AgentWikiChange } from '$api/types';
	import { api } from '#lib/api.js';
	import { fmtShort } from '#lib/format.js';
	import { wikiPath } from '#lib/wiki.js';

	let { project = null, limit = 10, style = '' }: { project?: string | null; limit?: number; style?: string } = $props();

	let items = $state<AgentWikiChange[]>([]);
	$effect(() => {
		const p = project;
		api('GET /wiki/agent-changes', { query: p ? { project: p } : {} })
			.then((r) => (items = r.items))
			.catch(() => (items = []));
	});
	const diffHref = (c: AgentWikiChange) => `${wikiPath(c.project.key, c.title, '/diff')}?from=${c.version - 1}&to=${c.version}`;
</script>

{#if items.length}
	<div class="card" {style} data-testid="agent-wiki-changes">
		<b>agent 最近 7 天改了這些 wiki 頁</b>
		<ul class="list">
			{#each items.slice(0, limit) as c (`${c.page_id}/${c.version}`)}
				<li data-testid="agent-wiki-change">
					<a href={wikiPath(c.project.key, c.title)}>〈{c.title}〉</a>{#if !project}<span class="hint">（{c.project.key}）</span>{/if}
					・{#if c.agent}<span class="agname">{c.agent.name}</span>{/if}<span class="agent-label">agent</span>
					{#if c.on_behalf_of}<span class="hint">代 {c.on_behalf_of.name}</span>{/if}
					{#if c.approver}<span class="hint">・{c.approver.name} 核准</span>{/if}
					<span class="hint">・{fmtShort(c.at)}</span>
					<a class="hint" href={diffHref(c)} data-testid="agent-wiki-diff">看差異</a>
				</li>
			{/each}
		</ul>
		{#if items.length > limit}<div class="hint">還有 {items.length - limit} 筆</div>{/if}
	</div>
{/if}

<style>
	.list { margin: 6px 0 0; padding-left: 18px; }
	.list li { margin: 2px 0; }
</style>

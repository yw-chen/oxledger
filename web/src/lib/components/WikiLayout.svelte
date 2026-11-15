<script lang="ts">
	// Design 06 §6.3: the wiki's page tree on the left (240 px, collapsible;
	// below 1024 px a 「頁面」 button opens it as a drawer), filtered locally,
	// with 「新增頁面」 at the bottom; the page itself on the right.
	import type { Snippet } from 'svelte';
	import type { WikiIndex } from '$api/types';
	import { ancestorIds, buildTree, filterTree, wikiPath, type TreeNode } from '#lib/wiki.js';

	let { projectKey, index, currentId = null, children }: { projectKey: string; index: WikiIndex | null; currentId?: string | null; children: Snippet } = $props();

	let q = $state('');
	let collapsed = $state(false);
	let drawer = $state(false);
	/** Nodes the person folded or opened by hand (default: open down to the current page, top level open). */
	let toggled = $state<Record<string, boolean>>({});
	const roots = $derived(index ? buildTree(index.tree) : []);
	const shown = $derived(filterTree(roots, q));
	const openPath = $derived(new Set(index && currentId ? ancestorIds(index.tree, currentId) : []));
	const isOpen = (n: TreeNode) => (q.trim() ? true : (toggled[n.id] ?? (openPath.has(n.id) || n.id === currentId)));
	const newHref = $derived(`/project/${projectKey}/wiki-new${currentId ? `?parent=${encodeURIComponent(currentId)}` : ''}`);
</script>

{#snippet node(n: TreeNode)}
	<li>
		<div class="tn" class:cur={n.id === currentId}>
			{#if n.children.length}
				<button type="button" class="tw" onclick={() => (toggled[n.id] = !isOpen(n))} aria-expanded={isOpen(n)} aria-label="{isOpen(n) ? '收合' : '展開'}〈{n.title}〉">{isOpen(n) ? '▾' : '▸'}</button>
			{:else}<span class="tw"></span>{/if}
			<a href={wikiPath(projectKey, n.title)} aria-current={n.id === currentId ? 'page' : undefined} onclick={() => (drawer = false)}>{n.title}</a>
		</div>
		{#if n.children.length && isOpen(n)}
			<ul>{#each n.children as c (c.id)}{@render node(c)}{/each}</ul>
		{/if}
	</li>
{/snippet}

<div class="wl" class:collapsed>
	<button type="button" class="btn sm wl-toggle-narrow" onclick={() => (drawer = !drawer)} aria-expanded={drawer} data-testid="wiki-tree-open">頁面</button>
	{#if drawer}<button type="button" class="wl-bg" onclick={() => (drawer = false)} aria-label="關閉頁面清單"></button>{/if}
	<aside class="wl-side" class:open={drawer} aria-label="Wiki 頁面" data-testid="wiki-tree">
		<div class="row" style="gap:4px;flex-wrap:nowrap">
			<input type="search" bind:value={q} placeholder="篩選頁面" aria-label="篩選頁面" data-testid="wiki-tree-filter" style="flex:1;min-width:0" />
			<button type="button" class="btn sm ghost wl-collapse" onclick={() => (collapsed = true)} title="收合頁面清單" aria-label="收合頁面清單">«</button>
		</div>
		{#if index}
			{#if shown.length}
				<ul class="tree">{#each shown as n (n.id)}{@render node(n)}{/each}</ul>
			{:else}
				<div class="hint" style="padding:6px 0">{q.trim() ? '沒有符合的頁面。' : '還沒有頁面。'}</div>
			{/if}
			{#if index.can.create}
				<a class="btn sm" href={newHref} data-testid="wiki-new">＋ 新增頁面</a>
			{:else}
				<button type="button" class="btn sm" disabled title={index.read_only ? '專案已關閉，wiki 唯讀' : '你沒有權限建立頁面'} data-testid="wiki-new">＋ 新增頁面</button>
			{/if}
		{/if}
	</aside>
	{#if collapsed}<button type="button" class="btn sm ghost wl-expand" onclick={() => (collapsed = false)} title="展開頁面清單" aria-label="展開頁面清單">»</button>{/if}
	<div class="wl-main">{@render children()}</div>
</div>

<style>
	.wl { display: flex; gap: 14px; align-items: flex-start; margin-top: 10px; }
	.wl-side { width: 240px; flex: none; display: flex; flex-direction: column; gap: 6px; position: sticky; top: 0; max-height: calc(100vh - 140px); overflow: auto; font-size: 13px; }
	.wl.collapsed .wl-side { display: none; }
	.wl-main { flex: 1; min-width: 0; }
	.wl-toggle-narrow, .wl-bg { display: none; }
	.tree, .tree ul { list-style: none; margin: 0; padding: 0; }
	.tree ul { padding-left: 14px; }
	.tn { display: flex; align-items: center; gap: 2px; border-radius: 6px; padding: 1px 4px; }
	.tn.cur { background: var(--act-a); }
	.tn.cur a { font-weight: 700; }
	.tn a { color: inherit; text-decoration: none; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
	.tn a:hover { text-decoration: underline; }
	.tw { width: 16px; flex: none; background: none; border: 0; padding: 0; color: var(--dim); cursor: pointer; font-size: 11px; }
	@media (max-width: 1023.98px) {
		.wl { flex-direction: column; gap: 6px; }
		.wl-toggle-narrow { display: inline-block; }
		.wl-collapse, .wl-expand { display: none; }
		.wl-side, .wl.collapsed .wl-side { display: none; }
		.wl-side.open { display: flex; position: fixed; top: 0; left: 0; bottom: 0; max-height: none; width: min(300px, 85vw); z-index: 45; background: var(--card); border-right: 1px solid var(--line2); padding: 12px; }
		.wl-bg { display: block; position: fixed; inset: 0; z-index: 44; background: rgba(3, 7, 15, 0.45); border: 0; }
	}
</style>

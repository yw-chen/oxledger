<script lang="ts">
	// Top bar bell (contract 05 §6.15): unread badge, a 400px panel with up to
	// ten unread rows, 「查看全部」 to /notifications.
	import { goto } from '$app/navigation';
	import type { NotificationItem } from '$api/types';
	import { api } from '#lib/api.js';
	import { ago, badge, bellLabel, budgetLine, eventLine, moreCount, reasonLabels } from '#lib/notify.js';
	import { markRead, pollUnread, unread } from '#lib/notifs.svelte.js';
	import ErrorBox from './ErrorBox.svelte';

	let open = $state(false);
	let items = $state<NotificationItem[] | null>(null);
	let error = $state<unknown>(null);
	let root: HTMLElement | undefined = $state();

	$effect(() => pollUnread());

	async function toggle() {
		open = !open;
		if (!open) return;
		error = null;
		items = null;
		try {
			items = (await api('GET /notifications', { query: { filter: 'unread', limit: 10 } })).items;
		} catch (e) {
			error = e;
		}
	}
	async function go(n: NotificationItem) {
		open = false;
		markRead(n).catch(() => {});
		goto(n.kind === 'ai_budget' ? '/admin/ai' : `/issue/${n.issue?.key}`);
	}
	function outside(e: MouseEvent) {
		if (open && root && !root.contains(e.target as Node)) open = false;
	}
	const b = $derived(badge(unread.count));
</script>

<svelte:window onclick={outside} onkeydown={(e) => e.key === 'Escape' && (open = false)} />

<div class="bell" bind:this={root}>
	<button class="btn sm ghost" onclick={toggle} aria-label={bellLabel(unread.count)} aria-expanded={open} data-testid="bell">
		🔔{#if b}<span class="badge" data-testid="bell-count">{b}</span>{/if}
	</button>
	{#if open}
		<div class="bpanel" role="dialog" aria-label="未讀通知" data-testid="bell-panel">
			<ErrorBox {error} />
			{#if items}
				{#each items as n (n.id)}
					<button class="brow" onclick={() => go(n)} data-testid="bell-row">
						{#if n.kind === 'ai_budget'}
							<b>{budgetLine(n)}</b>
						{:else if n.issue}
							<b><span class="mono">{n.issue.key}</span> {n.issue.subject}</b>
							<span class="hint">{reasonLabels(n.reasons).join('・')}・{ago(n.last_at)}</span>
							{#if n.events[0]}<span class="ln">{eventLine(n.events[0])}</span>{/if}
							{#if moreCount(n)}<span class="hint">還有 {moreCount(n)} 則更新</span>{/if}
						{/if}
					</button>
				{:else}
					<div class="empty">沒有未讀通知。</div>
				{/each}
			{:else if !error}
				<div class="sync"><span class="spin"></span>載入中</div>
			{/if}
			<a class="ball" href="/notifications" onclick={() => {
					open = false;
					unread.rev++;
				}} data-testid="bell-all">查看全部</a>
		</div>
	{/if}
</div>

<style>
	.bell { position: relative; }
	.bell .badge { margin-left: 2px; }
	.bpanel { position: absolute; right: 0; top: calc(100% + 6px); width: 400px; max-width: calc(100vw - 32px); max-height: 70vh; overflow: auto; background: var(--card); border: 1px solid var(--line2); border-radius: 10px; box-shadow: 0 10px 30px rgba(0, 0, 0, 0.35); z-index: 60; padding: 6px; }
	.brow { display: flex; flex-direction: column; align-items: flex-start; gap: 1px; width: 100%; text-align: left; background: none; border: 0; border-radius: 6px; padding: 7px 8px; }
	.brow:hover, .brow:focus-visible { background: var(--card2); }
	.brow b { font-size: 13px; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
	@media (max-width: 600px) { .bpanel { position: fixed; left: 16px; right: 16px; top: 52px; width: auto; max-width: none; } }
	.ln { font-size: 12.5px; color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 100%; }
	.ball { display: block; text-align: center; padding: 6px; border-top: 1px solid var(--line); margin-top: 4px; font-size: 13px; }
</style>

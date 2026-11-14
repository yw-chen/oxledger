<script lang="ts">
	// 議題頁「工時」卡 (contract 05 §6.6): estimate vs logged (only time the
	// person can see), the latest 5 entries, a link to all of them, and 記工時.
	// The page shows the card only when `spent_hours` is present (module on
	// and `view_time_entries`).
	import type { TimeEntryView } from '$api/types';
	import { api } from '#lib/api.js';
	import { showHours } from '#lib/hours.js';
	import { toast } from '#lib/toast.svelte.js';
	import Actor from './Actor.svelte';
	import ErrorBox from './ErrorBox.svelte';
	import LogTime from './LogTime.svelte';

	let {
		issueKey,
		projectId,
		projectKey,
		estimated,
		spent,
		reloadKey = 0,
		entries = $bindable([]),
		onlogged
	}: {
		issueKey: string;
		projectId: string;
		projectKey: string;
		estimated: string | null | undefined;
		spent: string;
		reloadKey?: number;
		/** The latest entries (up to 20), for the history column. */
		entries?: TimeEntryView[];
		onlogged: () => void;
	} = $props();

	let error = $state<unknown>(null);
	let open = $state(false);
	let seq = 0;
	$effect(() => {
		void reloadKey;
		const k = issueKey;
		const s = ++seq;
		error = null;
		api('GET /time-entries', { query: { issue: k, limit: 20 } })
			.then((r) => {
				if (s === seq) entries = r.items;
			})
			.catch((e) => {
				if (s === seq) error = e;
			});
	});
	function logged(text: string) {
		open = false;
		toast(text, 'ok');
		onlogged();
	}
</script>

<div class="card" data-testid="time-card">
	<div class="row" style="gap:8px">
		<h3 style="margin:0">工時</h3>
		<span style="flex:1"></span>
		<button class="btn sm" onclick={() => (open = !open)} data-testid="log-time">記工時</button>
	</div>
	<div data-testid="time-sum">
		{estimated ? `預估 ${showHours(estimated)} 小時 · ` : ''}已記 {showHours(spent)} 小時
		<span class="dim" title="只包含你看得到的工時">ⓘ</span>
	</div>
	{#if open}
		<LogTime {projectId} {issueKey} ondone={logged} oncancel={() => (open = false)} />
	{/if}
	<ErrorBox {error} />
	{#each entries.slice(0, 5) as t (t.id)}
		<div class="te" data-testid="time-entry">
			<span class="mono">{t.spent_on}</span> · <Actor name={t.user.name} /> · {t.activity.name} · <b>{showHours(t.hours)} 小時</b>
			{#if t.author.kind === 'agent'}<span class="agent-label" title={t.author.on_behalf_of ? `代表 ${t.author.on_behalf_of.name}` : ''}>agent</span>{/if}
			{#if t.comments}<div class="dim">{t.comments}</div>{/if}
		</div>
	{:else}
		{#if !error}<div class="dim">還沒有工時。</div>{/if}
	{/each}
	<a href="/project/{projectKey}/time?issue={issueKey}" data-testid="all-time">全部工時 →</a>
</div>

<style>
	.te {
		padding: 4px 0;
		border-bottom: 1px solid var(--line2);
		font-size: 13px;
	}
</style>

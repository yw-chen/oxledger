<script lang="ts">
	// Layout B, right: the collaboration column. 「AI 助理」 (contract 05
	// §6.19) only when the organisation turned the built-in AI on; "動態"
	// shows what agents wrote and what waits for you, scoped to the issue
	// open in the middle (or everything).
	import { page } from '$app/state';
	import type { ActivityItem, AiStatus } from '$api/types';
	import AiChat from './AiChat.svelte';
	import { api } from '$lib/api';
	import { counts, myRoles } from '$lib/counts.svelte';
	import { prefs, savePrefs } from '$lib/prefs.svelte';
	import { fmtShort } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { labelOf, show, type Lookups } from '$lib/render';
	import { projectLookups } from '$lib/trackers';

	type Row = ActivityItem & { agent: string; agentId: string };

	const issueKey = $derived(page.url.pathname.startsWith('/issue/') ? decodeURIComponent(page.params.key ?? '').toUpperCase() : null);
	let rows = $state<Row[]>([]);
	let loading = $state(false);
	// Names for field keys and ids in the diffs (§15.3: names, not ids).
	let look = $state<Lookups>({ fields: [], assignees: [], versions: [] });

	const agents = $derived.by(() => {
		const m = new Map<string, string>();
		// My own roles only (as before /ai-team): one request each, and the
		// whole organisation's roles would grow with the team.
		for (const c of myRoles()) if (c.status !== 'retired') m.set(c.id, c.handle);
		return [...m.entries()];
	});
	const pending = $derived(counts.awaiting.filter((c) => !issueKey || c.targets.some((t) => t.key === issueKey)));
	const paused = $derived(issueKey ? [] : myRoles().filter((m) => m.status === 'paused'));

	let seq = 0;
	$effect(() => {
		const key = issueKey;
		const list = agents;
		void counts.loadedAt;
		const my = ++seq;
		if (!list.length) {
			rows = [];
			return;
		}
		loading = true;
		Promise.all(
			list.map(([id, label]) =>
				api('GET /ai-team/{id}/activity', { path: { id }, query: { issue: key ?? undefined } })
					.then((a) => a.items.map((it) => ({ ...it, agent: label, agentId: id })))
					.catch(() => [] as Row[])
			)
		).then((all) => {
			if (my !== seq) return;
			const shown = all
				.flat()
				.sort((a, b) => b.at.localeCompare(a.at))
				.slice(0, key ? 20 : 8);
			const prefixes = new Set(shown.flatMap((r) => r.keys.map((k) => k.split('-')[0])));
			const pids = session.projects.filter((p) => prefixes.has(p.key)).map((p) => p.id);
			projectLookups(pids)
				.catch(() => ({ fields: [], assignees: [], versions: [] }) as Lookups)
				.then((l) => {
					if (my !== seq) return;
					look = l;
					rows = shown;
					loading = false;
				});
		});
	});

	const hasContent = $derived(rows.length + pending.length + paused.length > 0);
	const KIND: Record<string, string> = { direct: '直接寫入', proposal: '核准後套用', blocked: '被擋', undo: '回復' };

	// §6.19: `enabled = false` shows no AI tab at all.
	let ai = $state<AiStatus | null>(null);
	async function loadAi() {
		try {
			ai = await api('GET /ai/status');
		} catch {
			ai = null;
		}
	}
	$effect(() => {
		if (session.user?.user_id) loadAi();
	});
	const aiOn = $derived(!!ai?.enabled);
	const showAi = $derived(aiOn && prefs.collabTab === 'ai');

	function tab(t: 'ai' | 'activity') {
		prefs.collabTab = t;
		savePrefs();
	}
	function hide(v: boolean) {
		prefs.collabHidden = v;
		savePrefs();
	}
</script>

{#if prefs.collabHidden || (!hasContent && !aiOn && !loading)}
	<aside class="collab rail" aria-label="協作欄（已收合）">
		<button class="btn sm ghost" title="展開協作欄" onclick={() => { hide(false); tab(hasContent || !aiOn ? 'activity' : 'ai'); }} data-testid="collab-open">«</button>
		<span class="vlabel">{hasContent ? '動態' : aiOn ? 'AI 助理' : '沒有 agent 動態'}</span>
	</aside>
{:else}
	<aside class="collab" aria-label="協作欄" data-testid="collab">
		<div class="ch">
			{#if aiOn}<button class="ctab" class:on={showAi} onclick={() => tab('ai')} data-testid="tab-ai">AI 助理</button>{/if}
			<button class="ctab" class:on={!showAi} onclick={() => tab('activity')} data-testid="tab-activity">動態</button>
			<span class="dim" style="font-size:12px">{issueKey ?? '全部'}</span>
			<span style="flex:1"></span>
			<button class="btn sm ghost" title="收合" onclick={() => hide(true)}>»</button>
		</div>
		{#if showAi && ai}
			<AiChat status={ai} onstatus={loadAi} />
		{:else}
			<div class="csec">
				{#each paused as c (c.id)}
					<div class="banner w" style="margin:0 0 8px"><span class="agname">{c.handle}</span> 寫入已暫停<span class="sp"></span><a class="btn sm" href="/ai-team/{encodeURIComponent(c.handle)}">處理</a></div>
				{/each}
				{#if pending.length}
					<div class="k" style="margin:2px 0 4px">待你核准</div>
					{#each pending.slice(0, 5) as c (c.changeset_id)}
						<div class="dyn">
							<a href="/cs/{c.changeset_id}">{c.title ?? '提案'}</a>
							<div style="font-size:12.5px" class="muted">{c.targets.map((t) => t.key).join('、') || '建立新單'}</div>
						</div>
					{/each}
					{#if pending.length > 5}<a class="dim" href="/review">還有 {pending.length - 5} 份</a>{/if}
				{/if}
				{#if rows.length}
					<div class="k" style="margin:8px 0 4px">agent 寫入</div>
					{#each rows as r (r.agentId + (r.execution_id ?? r.at))}
						<div class="dyn" data-testid="dyn-row">
							<div class="row" style="gap:6px"><span class="dim mono" style="font-size:12px">{fmtShort(r.at)}</span><span class="chip {r.kind === 'blocked' ? 'c-bad' : r.kind === 'undo' ? 'c-dim' : 'c-prop'}">{KIND[r.kind] ?? r.kind}</span></div>
							<div style="font-size:12.5px"><span class="agname">{r.agent}</span></div>
							<div style="font-size:12.5px">
								{#each r.keys as k (k)}<a class="key" href="/issue/{k}">{k}</a> {/each}
								{#each r.diff.slice(0, 3) as d (d.key + d.field)}
									<div class="diff">{labelOf(d.field, look)}：<span class="old">{show(d.field, d.before, look)}</span><span class="arrow">→</span><span class="new">{show(d.field, d.after, look)}</span></div>
								{/each}
							</div>
						</div>
					{/each}
					<a class="dim" style="font-size:12.5px" href="/ai-team">AI 團隊與完整活動 →</a>
				{:else if loading}
					<div class="sync"><span class="spin"></span>讀取中</div>
				{/if}
			</div>
		{/if}
	</aside>
{/if}

<script lang="ts">
	// Layout B, right: the collaboration column. "AI 對話" shows the closed
	// state (frontend Q2 off); "動態" shows what agents wrote and what waits
	// for you, scoped to the issue open in the middle (or everything).
	import { page } from '$app/state';
	import type { ActivityItem } from '$api/types';
	import { api } from '$lib/api';
	import { counts } from '$lib/counts.svelte';
	import { prefs, savePrefs } from '$lib/prefs.svelte';
	import { fmtShort, fmtValue, fieldLabel } from '$lib/format';

	type Row = ActivityItem & { agent: string; agentId: string };

	const issueKey = $derived(page.url.pathname.startsWith('/issue/') ? decodeURIComponent(page.params.key ?? '').toUpperCase() : null);
	let rows = $state<Row[]>([]);
	let loading = $state(false);

	const agents = $derived.by(() => {
		const m = new Map<string, string>();
		for (const c of counts.contexts) m.set(c.agent_principal_id, c.agent_label);
		return [...m.entries()];
	});
	const pending = $derived(counts.awaiting.filter((c) => !issueKey || c.targets.some((t) => t.key === issueKey)));
	const paused = $derived(issueKey ? [] : counts.contexts.filter((c) => c.state === 'active' && c.suspended_at));

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
				api('GET /agents/{id}/activity', { path: { id }, query: { issue: key ?? undefined } })
					.then((a) => a.items.map((it) => ({ ...it, agent: label, agentId: id })))
					.catch(() => [] as Row[])
			)
		).then((all) => {
			if (my !== seq) return;
			rows = all
				.flat()
				.sort((a, b) => b.at.localeCompare(a.at))
				.slice(0, key ? 20 : 8);
			loading = false;
		});
	});

	const hasContent = $derived(rows.length + pending.length + paused.length > 0);
	const KIND: Record<string, string> = { direct: '直接寫入', proposal: '核准後套用', blocked: '被擋', undo: '回復' };

	function tab(t: 'ai' | 'activity') {
		prefs.collabTab = t;
		savePrefs();
	}
	function hide(v: boolean) {
		prefs.collabHidden = v;
		savePrefs();
	}
</script>

{#if prefs.collabHidden || (!hasContent && prefs.collabTab === 'activity' && !loading)}
	<aside class="collab rail" aria-label="協作欄（已收合）">
		<button class="btn sm ghost" title="展開協作欄" onclick={() => { hide(false); tab(hasContent ? 'activity' : 'ai'); }} data-testid="collab-open">«</button>
		<span class="vlabel">{hasContent ? '動態' : '沒有 agent 動態'}</span>
	</aside>
{:else}
	<aside class="collab" aria-label="協作欄" data-testid="collab">
		<div class="ch">
			<button class="ctab" class:on={prefs.collabTab === 'ai'} onclick={() => tab('ai')}>AI 對話</button>
			<button class="ctab" class:on={prefs.collabTab === 'activity'} onclick={() => tab('activity')}>動態</button>
			<span class="dim" style="font-size:12px">{issueKey ?? '全部'}</span>
			<span style="flex:1"></span>
			<button class="btn sm ghost" title="收合" onclick={() => hide(true)}>»</button>
		</div>
		{#if prefs.collabTab === 'ai'}
			<div class="csec" data-testid="ai-off">
				<b>內建 AI 未啟用</b>
				<p class="muted" style="font-size:13px">組織目前關閉內建 AI 與對外的模型資料出口（前端 Q2）。外部 agent（例如 Claude Code）透過 MCP 做的事，在「動態」分頁看得到。</p>
				<p class="hint">⌘K 輸入自然語言時會改用文字搜尋。</p>
			</div>
		{:else}
			<div class="csec">
				{#each paused as c (c.id)}
					<div class="banner w" style="margin:0 0 8px"><span class="agname">{c.agent_label}</span> 寫入已暫停<span class="sp"></span><a class="btn sm" href="/agents">處理</a></div>
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
									<div class="diff">{fieldLabel(d.field)}：<span class="old">{fmtValue(d.before)}</span><span class="arrow">→</span><span class="new">{fmtValue(d.after)}</span></div>
								{/each}
							</div>
						</div>
					{/each}
					<a class="dim" style="font-size:12.5px" href="/agents">完整活動與回復 →</a>
				{:else if loading}
					<div class="sync"><span class="spin"></span>讀取中</div>
				{/if}
			</div>
		{/if}
	</aside>
{/if}

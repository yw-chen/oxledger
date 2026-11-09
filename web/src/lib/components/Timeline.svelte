<script lang="ts">
	// 時程 (§17.8): one lane per milestone (by date) plus the lane without a
	// milestone; bars from start to due date; dates and health come from the
	// server (the org time zone's "today"). Only issues the viewer can see.
	import type { Timeline } from '$api/types';
	import { api } from '$lib/api';
	import { FLAG, HEALTH, HEALTH_REASON } from '$lib/health';
	import ErrorBox from './ErrorBox.svelte';

	let { projectId }: { projectId: string } = $props();
	let data = $state<Timeline | null>(null);
	let error = $state<unknown>(null);
	let includeClosed = $state(false);
	let loading = $state(false);

	async function load() {
		loading = true;
		error = null;
		try {
			data = await api('GET /projects/{id}/timeline', { path: { id: projectId }, query: { include_closed: includeClosed } });
		} catch (e) {
			error = e;
		} finally {
			loading = false;
		}
	}
	$effect(() => {
		void projectId;
		void includeClosed;
		load();
	});

	const DAY = 86400000;
	const t = (d: string) => Date.parse(`${d}T00:00:00Z`);
	const span = $derived(data ? Math.max(1, (t(data.range.to) - t(data.range.from)) / DAY + 1) : 1);
	function pos(d: string | null): number | null {
		if (!data || !d) return null;
		return Math.min(100, Math.max(0, ((t(d) - t(data.range.from)) / DAY / span) * 100));
	}
	function bar(start: string | null, due: string | null) {
		const a = pos(start ?? due);
		const b = pos(due ?? start);
		if (a === null || b === null) return null;
		return { left: a, width: Math.max(0.8, b - a + 100 / span) };
	}
	const todayPos = $derived(data ? pos(data.today) : null);
</script>

<div class="row" style="gap:8px;margin:8px 0">
	<label class="row" style="gap:4px"><input type="checkbox" bind:checked={includeClosed} /> 包含已關閉的單</label>
	{#if data}<span class="hint">{data.range.from} ～ {data.range.to}{data.range.truncated_by_version ? `（因里程碑 ${data.range.truncated_by_version} 截到 400 天）` : ''}・今天 {data.today}</span>{/if}
</div>
<ErrorBox {error} onaction={load} />
{#if loading && !data}<div class="sync"><span class="spin"></span>載入中…</div>{/if}
{#if data}
	{#each data.lanes as lane, i (lane.version?.id ?? `none-${i}`)}
		<div class="card" data-testid="lane">
			<div class="row" style="gap:8px">
				<b>{lane.version ? lane.version.name : '沒有里程碑'}</b>
				{#if lane.version}
					<span class="hint">{lane.version.effective_date ?? '未定日期'}</span>
					<span class="chip {HEALTH[lane.version.health]?.cls ?? 'c-dim'}" title={lane.version.health_reasons.map((r) => HEALTH_REASON[r] ?? r).join('、')}>{HEALTH[lane.version.health]?.label ?? lane.version.health}</span>
					{#if lane.version.deleted}<span class="chip c-dim">已刪除</span>{/if}
				{/if}
				{#if lane.undated_count}<span class="hint">另有 {lane.undated_count} 張沒有日期（不畫）</span>{/if}
			</div>
			{#each lane.items as it (it.key)}
				{@const b = bar(it.start_date, it.due_date)}
				<div class="tl-row">
					<div class="tl-label">
						<a class="key" href="/issue/{it.key}">{it.key}</a>
						<span class:muted={it.status.is_closed}>{it.subject}</span>
						{#each it.flags as f (f)}<span class="chip {FLAG[f]?.cls ?? 'c-dim'}">{FLAG[f]?.label ?? f}</span>{/each}
					</div>
					<div class="tl-track">
						{#if todayPos !== null}<div class="tl-today" style="left:{todayPos}%"></div>{/if}
						{#if lane.version?.effective_date && pos(lane.version.effective_date) !== null}<div class="tl-ms" style="left:{pos(lane.version.effective_date)}%" title="里程碑 {lane.version.effective_date}"></div>{/if}
						{#if b}<div class="tl-bar" class:closed={it.status.is_closed} class:late={it.flags.length > 0} style="left:{b.left}%;width:{b.width}%" title="{it.start_date ?? '—'} → {it.due_date ?? '—'}・{it.status.name}{it.assigned_to ? `・${it.assigned_to.name}` : ''}"></div>{/if}
					</div>
				</div>
			{:else}
				<div class="hint">這條沒有畫得出來的單。</div>
			{/each}
		</div>
	{:else}
		<div class="empty">沒有單。</div>
	{/each}
{/if}

<style>
	.tl-row { display: grid; grid-template-columns: minmax(180px, 40%) 1fr; gap: 8px; align-items: center; padding: 2px 0; }
	.tl-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }
	.tl-track { position: relative; height: 14px; background: var(--bg2); border-radius: 4px; }
	.tl-bar { position: absolute; top: 2px; height: 10px; border-radius: 3px; background: var(--act); }
	.tl-bar.closed { background: var(--dim); }
	.tl-bar.late { background: var(--warn); }
	.tl-today { position: absolute; top: -2px; bottom: -2px; width: 2px; background: var(--bad); }
	.tl-ms { position: absolute; top: -2px; bottom: -2px; width: 0; border-left: 2px dashed var(--prop); }
</style>

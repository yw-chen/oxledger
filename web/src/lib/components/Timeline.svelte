<script lang="ts">
	// 時程 (§17.8): one lane per milestone (by date) plus the lane without a
	// milestone; bars from start to due date; dates and health come from the
	// server (the org time zone's "today"). Only issues the viewer can see.
	import type { Timeline } from '$api/types';
	import { api } from '#lib/api.js';
	import { FLAG, HEALTH, HEALTH_REASON } from '#lib/health.js';
	import ErrorBox from './ErrorBox.svelte';

	let { projectId }: { projectId: string } = $props();
	let data = $state<Timeline | null>(null);
	let error = $state<unknown>(null);
	let includeClosed = $state(false);
	let loading = $state(false);
	// The range (§17.12): empty = the server's default window.
	let from = $state('');
	let to = $state('');
	let versionNames = $state<Record<string, string>>({});

	async function load() {
		loading = true;
		error = null;
		try {
			data = await api('GET /projects/{id}/timeline', { path: { id: projectId }, query: { include_closed: includeClosed, from: from || undefined, to: to || undefined } });
			// The server names the cutting milestone by id only (gap G22).
			const cut = data.range.truncated_by_version;
			if (cut && !versionNames[cut]) {
				api('GET /projects/{id}/versions', { path: { id: projectId }, query: { status: 'open,locked,closed' } })
					.then((l) => (versionNames = Object.fromEntries(l.versions.map((v) => [v.id, `${v.name}（${v.effective_date ?? '未定日期'}）`]))))
					.catch(() => {});
			}
		} catch (e) {
			error = e;
		} finally {
			loading = false;
		}
	}
	$effect(() => {
		void projectId;
		void includeClosed;
		void from;
		void to;
		load();
	});
	const shift = (d: string, n: number) => new Date(Date.parse(`${d}T00:00:00Z`) + n * 86400000).toISOString().slice(0, 10);
	/** Show the weeks around a milestone date. */
	function jump(date: string) {
		from = shift(date, -28);
		to = shift(date, 14);
	}

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
	<label class="row" style="gap:4px">從 <input type="date" bind:value={from} aria-label="開始日期" /></label>
	<label class="row" style="gap:4px">到 <input type="date" bind:value={to} aria-label="結束日期" /></label>
	{#if from || to}<button class="btn sm ghost" onclick={() => { from = ''; to = ''; }}>預設範圍</button>{/if}
</div>
{#if data}
	<div class="hint" data-testid="tl-range">
		{data.range.from} ～ {data.range.to}・今天 {data.today}
		{#if data.range.truncated_by_version}・範圍最多 400 天，最遠的里程碑 {versionNames[data.range.truncated_by_version] ?? ''} 沒有包含在內{/if}
	</div>
{/if}
<ErrorBox {error} onaction={load} />
{#if loading && !data}<div class="sync"><span class="spin"></span>載入中…</div>{/if}
{#if data}
	{#each data.lanes as lane, i (lane.version?.id ?? `none-${i}`)}
		<div class="card" data-testid="lane">
			<div class="row" style="gap:8px">
				<b>{lane.version ? lane.version.name : '沒有里程碑'}</b>
				{#if lane.version}
					<span class="hint">{lane.version.effective_date ?? '未定日期'}</span>
					{#if lane.version.effective_date && (lane.version.effective_date < data.range.from || lane.version.effective_date > data.range.to)}<button class="btn sm ghost" onclick={() => jump(lane.version!.effective_date!)} data-testid="tl-jump">跳過去</button>{/if}
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
				<div class="hint">這段期間內沒有可以畫的單（可以調整上方的日期範圍{lane.version?.effective_date ? '，或按「跳過去」' : ''}）。</div>
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

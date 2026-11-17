<script lang="ts">
	// 專案工時報表 (contract 05 §6.8): a period, one grouping, filters by
	// person and issue; horizontal bars by hours plus a table. A row drills
	// down to the entries behind it. Totals count only time the viewer may
	// see (`scope_note = only_visible`).
	import { page } from '$app/state';
	import type { Summary, SummaryRow, TimeEntryView } from '$api/types';
	import { api, ApiError } from '#lib/api.js';
	import { exportProblem, fetchExport, saveBlob } from '#lib/exports.js';
	import { toast } from '#lib/toast.svelte.js';
	import { addDays, period, showHours, todayLocal, weekMonday } from '#lib/hours.js';
	import ErrorBox from './ErrorBox.svelte';
	import Actor from './Actor.svelte';

	let { projectKey }: { projectKey: string } = $props();

	type P = 'this_week' | 'last_week' | 'this_month' | 'last_month' | 'custom';
	// From an issue's 「全部工時 →」: the last year, not just this week.
	const fromIssue = !!page.url.searchParams.get('issue');
	let per = $state<P>(fromIssue ? 'custom' : 'this_week');
	const today = todayLocal();
	let from = $state(fromIssue ? addDays(today, -365) : period('this_week', today).from);
	let to = $state(fromIssue ? today : period('this_week', today).to);
	let groupBy = $state('user');
	let exporting = $state(false);
	/** 匯出 CSV (design 06 §6.11): the entries the report's filters match. */
	async function exportCsv() {
		exporting = true;
		try {
			const f = await fetchExport('/exports/time-entries', { project: projectKey, from, to, user: user || undefined, issue: issue || undefined });
			saveBlob(f.blob, f.filename);
		} catch (e) {
			toast(exportProblem(e) ?? '匯出失敗，請稍後再試', 'w');
		} finally {
			exporting = false;
		}
	}
	let user = $state('');
	let issue = $state(page.url.searchParams.get('issue') ?? '');
	let issueInput = $state(page.url.searchParams.get('issue') ?? '');
	let weekStart = $state(1);
	let data = $state<Summary | null>(null);
	let people = $state<SummaryRow[]>([]);
	let error = $state<unknown>(null);
	let access = $state<'ok' | 'off' | 'denied'>('ok');

	$effect(() => {
		api('GET /filter-fields')
			.then((f) => (weekStart = f.week_start))
			.catch(() => {});
	});
	$effect(() => {
		if (per === 'custom') return;
		const p = period(per, today, weekStart);
		from = p.from;
		to = p.to;
	});

	let seq = 0;
	async function load() {
		const s = ++seq;
		error = null;
		drill = null;
		const q = { project: projectKey, from, to, group_by: groupBy, user: user || undefined, issue: issue || undefined };
		try {
			const [r, ppl] = await Promise.all([
				api('GET /time-entries/summary', { query: q }),
				groupBy === 'user' ? Promise.resolve(null) : api('GET /time-entries/summary', { query: { project: projectKey, from, to, group_by: 'user' } }).catch(() => null)
			]);
			if (s !== seq) return;
			data = r;
			people = groupBy === 'user' && !user && !issue ? r.rows : (ppl?.rows ?? people);
			access = 'ok';
		} catch (e) {
			if (s !== seq) return;
			data = null;
			if (e instanceof ApiError && e.code === 'forbidden_op') {
				// Module off and no permission answer the same; the project's
				// time settings tell them apart.
				const t = await api('GET /projects/{id}/time-activities', { path: { id: projectKey } }).catch(() => null);
				if (s !== seq) return;
				access = t && !t.module_enabled ? 'off' : 'denied';
				return;
			}
			error = e;
		}
	}
	$effect(() => {
		void [projectKey, from, to, groupBy, user, issue];
		if (from && to) load();
	});

	const top = $derived(Math.max(0, ...(data?.rows ?? []).map((r) => Number(r.hours))));
	const GROUPS: [string, string][] = [
		['user', '人'],
		['activity', '活動'],
		['issue', '議題'],
		['tracker', '類型'],
		['fixed_version', '里程碑'],
		['day', '日'],
		['week', '週'],
		['month', '月']
	];
	const label = (r: SummaryRow) => r.label || (r.key === null ? (groupBy === 'issue' ? '（沒有議題）' : groupBy === 'fixed_version' ? '（沒有里程碑）' : '（沒有）') : r.key);

	// ---- drill down ----
	let drill = $state<{ row: SummaryRow; items: TimeEntryView[]; next: string | null; error: unknown } | null>(null);
	function drillQuery(r: SummaryRow): Record<string, string | undefined> | null {
		const base = { project: projectKey, from, to, user: user || undefined, issue: issue || undefined };
		if (groupBy === 'user' && r.key) return { ...base, user: r.key };
		if (groupBy === 'activity' && r.key) return { ...base, activity: r.key };
		if (groupBy === 'issue' && r.key) return { ...base, issue: r.key };
		if (groupBy === 'day' && r.key) return { ...base, from: r.key, to: r.key };
		if (groupBy === 'week' && r.key) {
			const m = weekMonday(r.key);
			return { ...base, from: m < from ? from : m, to: addDays(m, 6) > to ? to : addDays(m, 6) };
		}
		if (groupBy === 'month' && r.key) {
			const f = `${r.key}-01`;
			const end = new Date(Date.UTC(Number(r.key.slice(0, 4)), Number(r.key.slice(5, 7)), 0)).toISOString().slice(0, 10);
			return { ...base, from: f < from ? from : f, to: end > to ? to : end };
		}
		return null;
	}
	async function openDrill(r: SummaryRow, more = false) {
		const q = drillQuery(r);
		if (!q) return;
		const cursor = more ? (drill?.next ?? undefined) : undefined;
		if (!more) drill = { row: r, items: [], next: null, error: null };
		try {
			const p = await api('GET /time-entries', { query: { ...q, cursor, limit: 50 } });
			if (drill?.row !== r) return;
			drill = { row: r, items: more ? [...drill.items, ...p.items] : p.items, next: p.next_cursor, error: null };
		} catch (e) {
			if (drill?.row === r) drill = { ...drill, error: e };
		}
	}
</script>

<div data-testid="time-report">
	<h2 style="margin:8px 0">工時報表 · {projectKey}</h2>
	{#if access === 'off'}
		<div class="empty" data-testid="time-off">這個專案沒有開啟工時。</div>
	{:else if access === 'denied'}
		<div class="empty" data-testid="time-denied">你沒有權限查看這個專案的工時。</div>
	{:else}
		<div class="row" style="justify-content:flex-end"><button class="btn sm" onclick={exportCsv} disabled={exporting} data-testid="tr-export">{exporting ? '匯出中…' : '匯出 CSV'}</button></div>
		<div class="row" style="gap:10px;flex-wrap:wrap">
			<label>期間
				<select bind:value={per} data-testid="tr-period">
					<option value="this_week">本週</option><option value="last_week">上週</option>
					<option value="this_month">本月</option><option value="last_month">上月</option>
					<option value="custom">自訂</option>
				</select>
			</label>
			{#if per === 'custom'}
				<input type="date" bind:value={from} aria-label="開始日" data-testid="tr-from" /> ～ <input type="date" bind:value={to} aria-label="結束日" data-testid="tr-to" />
			{:else}
				<span class="mono dim" data-testid="tr-range">{from} ～ {to}</span>
			{/if}
			<label>分組
				<select bind:value={groupBy} data-testid="tr-group">
					{#each GROUPS as [v, t] (v)}<option value={v}>{t}</option>{/each}
				</select>
			</label>
			<label>人
				<select bind:value={user} data-testid="tr-user">
					<option value="">全部</option>
					{#each people.filter((p) => p.key) as p (p.key)}<option value={p.key}>{p.label}</option>{/each}
				</select>
			</label>
			<label>議題
				<input bind:value={issueInput} placeholder="單號" style="width:110px" onchange={() => (issue = issueInput.trim().toUpperCase())} data-testid="tr-issue" />
			</label>
			{#if issue}<button class="btn sm ghost" onclick={() => ((issue = ''), (issueInput = ''))}>清除議題</button>{/if}
		</div>
		<ErrorBox {error} onaction={load} />
		{#if data}
			<div class="card" style="margin-top:8px">
				{#each data.rows as r (r.key ?? '∅')}
					{@const q = drillQuery(r)}
					<div class="bar-row" data-testid="tr-row">
						{#if q}
							<button class="bl linklike" onclick={() => openDrill(r)} title="看這組的工時">{label(r)}</button>
						{:else}
							<span class="bl">{label(r)}</span>
						{/if}
						<span class="bar" style="width:{top ? (Number(r.hours) / top) * 100 : 0}%" aria-hidden="true"></span>
						<span class="bh">{showHours(r.hours)} 小時</span>
					</div>
				{:else}
					<div class="empty" data-testid="tr-empty">這段期間沒有你看得到的工時。</div>
				{/each}
				<table class="trt">
					<thead><tr><th>{GROUPS.find((g) => g[0] === groupBy)?.[1]}</th><th>時數</th></tr></thead>
					<tbody>{#each data.rows as r (r.key ?? '∅')}<tr><td>{label(r)}</td><td>{showHours(r.hours)}</td></tr>{/each}</tbody>
					<tfoot><tr><th>合計</th><th data-testid="tr-total">{showHours(data.total)}</th></tr></tfoot>
				</table>
				<p class="hint" data-testid="tr-note">只包含你看得到的工時。</p>
				{#if groupBy === 'tracker' || groupBy === 'fixed_version'}
					<!-- GET /time-entries has no tracker / fixed_version filter yet, so these rows cannot drill down. -->
					<p class="hint" data-testid="tr-nodrill">依類型或里程碑分組時，還不能點開看明細。</p>
				{/if}
			</div>
		{/if}
		{#if drill}
			<div class="card" data-testid="tr-drill">
				<div class="row"><b>{label(drill.row)}</b><span style="flex:1"></span><button class="btn sm ghost" onclick={() => (drill = null)}>關閉</button></div>
				<ErrorBox error={drill.error} />
				{#each drill.items as t (t.id)}
					<div class="te"><span class="mono">{t.spent_on}</span> · <Actor name={t.user.name} /> · {t.activity.name} · {#if t.issue}<a class="mono" href="/issue/{t.issue.key}">{t.issue.key}</a>{:else}<span class="dim">沒有議題</span>{/if} · <b>{showHours(t.hours)}</b>{#if t.comments}<span class="dim"> · {t.comments}</span>{/if}</div>
				{/each}
				{#if drill.next}<button class="btn sm" onclick={() => drill && openDrill(drill.row, true)}>載入更多</button>{/if}
			</div>
		{/if}
	{/if}
</div>

<style>
	.bar-row {
		display: grid;
		grid-template-columns: minmax(120px, 240px) 1fr 80px;
		gap: 8px;
		align-items: center;
		padding: 2px 0;
	}
	.bl {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		text-align: left;
	}
	.linklike {
		background: none;
		border: none;
		color: var(--act);
		cursor: pointer;
		padding: 0;
	}
	.bar {
		height: 12px;
		background: var(--act);
		border-radius: 3px;
		min-width: 2px;
	}
	.bh {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.trt {
		margin-top: 10px;
		border-collapse: collapse;
		min-width: 280px;
	}
	.trt th,
	.trt td {
		border-bottom: 1px solid var(--line2);
		padding: 3px 8px;
		text-align: left;
	}
	.te {
		padding: 3px 0;
		border-bottom: 1px solid var(--line2);
		font-size: 13px;
	}
</style>

<script lang="ts">
	// One home widget's content (contract 05 §3.8 table). Everything is read
	// with the signed-in person's own permissions, also in a role preview.
	import type { AgentSummary, RunItem, Timesheet, TriageCount, VersionView, Widget } from '$api/types';
	import { api } from '$lib/api';
	import { counts, refreshCounts } from '$lib/counts.svelte';
	import { session, projectById } from '$lib/session.svelte';
	import { HEALTH, HEALTH_REASON } from '$lib/health';
	import { fmtValue } from '$lib/format';
	import { showHours } from '$lib/hours';
	import ErrorBox from './ErrorBox.svelte';

	let { w }: { w: Widget } = $props();

	let error = $state<unknown>(null);
	let rows = $state<RunItem[] | null>(null);
	let viewGone = $state(false);
	let triage = $state<TriageCount[] | null>(null);
	let milestones = $state<{ project: string; v: VersionView }[] | null>(null);
	let sheet = $state<Timesheet | null>(null);
	let summary = $state<AgentSummary | null>(null);

	async function load() {
		error = null;
		viewGone = false;
		try {
			switch (w.type) {
				case 'view': {
					if (!w.available || !w.view_id) {
						viewGone = true;
						return;
					}
					try {
						const r = await api('POST /views/{id}/run', { path: { id: w.view_id }, body: { limit: w.limit ?? 8 } });
						rows = r.items ?? (r.lanes ?? []).flatMap((l) => (l as { items?: RunItem[] }).items ?? []).slice(0, w.limit ?? 8);
					} catch (e) {
						if ((e as { code?: string }).code === 'not_found') viewGone = true;
						else throw e;
					}
					break;
				}
				case 'triage_count':
					triage = (await api('GET /triage/counts')).projects;
					break;
				case 'milestones': {
					if (!w.available) {
						milestones = [];
						return;
					}
					const ps = w.project_id ? [{ id: w.project_id, key: w.project_key ?? projectById(w.project_id)?.key ?? '' }] : session.projects.slice(0, 8);
					const out: { project: string; v: VersionView }[] = [];
					for (const p of ps) {
						const l = await api('GET /projects/{id}/versions', { path: { id: p.id }, query: { include: 'stats' } });
						for (const v of l.versions) if (v.status !== 'closed') out.push({ project: p.key, v });
					}
					milestones = out;
					break;
				}
				case 'my_time_week':
					sheet = await api('GET /me/timesheet');
					break;
				case 'agent_summary':
					summary = await api('GET /home/agent-summary');
					break;
			}
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void w.type;
		void w.view_id;
		void w.limit;
		void w.project_id;
		if (w.type !== 'need_decision') load();
	});

	const failed = $derived(counts.awaiting.filter((c) => c.last_execution_status === 'failed'));
	const paused = $derived(counts.contexts.filter((c) => c.state === 'active' && c.suspended_at));
	const broken = $derived(counts.contexts.filter((c) => c.state === 'active' && c.changed_projects.length > 0));
	const projName = (id: string) => projectById(id)?.key ?? '某個專案';
	const SUSPEND: Record<string, string> = {
		manual: '你手動暫停',
		audience_blocked: '1 小時內多次被受眾規則擋下，已自動暫停',
		issue_write_burst: '同一張單短時間內寫入太多次，已自動暫停',
		version_write_burst: '里程碑短時間內寫入太多次，已自動暫停'
	};
	const DAY = ['一', '二', '三', '四', '五', '六', '日'];
	const v = (r: RunItem, k: string) => fmtValue((r.values ?? {})[k] ?? null);
</script>

<ErrorBox {error} onaction={load} />
{#if w.type === 'need_decision'}
	{@const n = counts.awaiting.length}
	{#each paused as c (c.id)}
		<div class="banner w"><span class="agname">{c.agent_label}</span> 寫入已暫停：{SUSPEND[c.suspension_reason ?? ''] ?? c.suspension_reason}<span class="sp"></span><a class="btn sm" href="/agents">處理</a></div>
	{/each}
	{#each broken as c (c.id)}
		<div class="banner b"><span class="agname">{c.agent_label}</span> 的工作脈絡因 {c.changed_projects.map(projName).join('、')} 的權限設定變更而停用<span class="sp"></span><a class="btn sm" href="/agents">建立新工作脈絡</a></div>
	{/each}
	{#each failed as c (c.changeset_id)}
		<div class="banner b">提案「{c.title ?? c.targets.map((t) => t.key).join('、')}」已核准但套用失敗<span class="sp"></span><a class="btn sm" href="/cs/{c.changeset_id}">查看</a></div>
	{/each}
	{#if n}
		<div class="row"><span class="badge">{counts.awaitingMore ? `${n}+` : n}</span> 份提案等你審<span style="flex:1"></span><a class="btn sm pri" href="/review">開啟待審</a></div>
		<ul style="margin:6px 0 0;padding-left:18px;font-size:13px">
			{#each counts.awaiting.slice(0, 10) as c (c.changeset_id)}
				<li><a href="/cs/{c.changeset_id}">{c.title ?? '提案'}</a> <span class="dim">{c.targets.map((t) => t.key).join('、')}</span></li>
			{/each}
		</ul>
	{:else if !paused.length && !broken.length && !failed.length}
		<div class="empty">目前沒有要你決定的事。</div>
	{/if}
	<button class="btn sm ghost" onclick={() => refreshCounts()}>重新整理</button>
{:else if w.type === 'view'}
	{#if viewGone}
		<div class="empty" data-testid="widget-view-gone">這個視圖已不存在。</div>
	{:else if rows}
		{#each rows as r (r.id)}
			<div class="wrow"><a href="/issue/{r.key}" class="mono">{r.key}</a> <span class="ws">{v(r, 'subject')}</span><span class="chip c-dim">{v(r, 'status')}</span></div>
		{:else}
			<div class="empty">沒有符合的議題。</div>
		{/each}
		<a class="hint" href="/views/{encodeURIComponent(w.view_id ?? '')}">開啟視圖 →</a>
	{/if}
{:else if w.type === 'triage_count'}
	{#if triage}
		{#each triage as t (t.project.id)}
			<div class="wrow"><a href="/project/{t.project.key}/triage"><span class="mono">{t.project.key}</span> {t.project.name}</a><span style="flex:1"></span><span class="badge">{t.more ? '99+' : t.count}</span></div>
		{:else}
			<div class="empty">沒有待分流的單。</div>
		{/each}
	{/if}
{:else if w.type === 'milestones'}
	{#if milestones}
		{#each milestones as m (m.v.id)}
			{@const h = HEALTH[m.v.health ?? ''] ?? { label: m.v.health, cls: 'c-dim' }}
			<div class="msw">
				<div class="row" style="gap:6px">
					<a href="/project/{m.project}/milestones"><span class="mono">{m.project}</span> {m.v.name}</a>
					<span class="chip {h.cls}" title={(m.v.health_reasons ?? []).map((r) => HEALTH_REASON[r] ?? r).join('；')}>{h.label}</span>
					<span class="dim mono" style="font-size:12px">{m.v.effective_date ?? '未定'}</span>
				</div>
				{#if m.v.stats && m.v.stats.total}
					<div class="prog"><i style="width:{Math.round((m.v.stats.closed / m.v.stats.total) * 100)}%"></i></div>
					<div class="hint">完成 {m.v.stats.closed}／{m.v.stats.total}{m.v.stats.overdue ? `・逾期 ${m.v.stats.overdue}` : ''}</div>
				{/if}
			</div>
		{:else}
			<div class="empty">沒有進行中的里程碑。</div>
		{/each}
	{/if}
{:else if w.type === 'my_time_week'}
	{#if sheet}
		<div class="row"><b style="font-size:20px" data-testid="widget-week-total">{showHours(sheet.week_total)}</b><span class="dim">小時（{sheet.week}）</span><span style="flex:1"></span><a class="btn sm" href="/time">週表</a></div>
		<div class="days">
			{#each sheet.days as d, i (d)}
				<span class="day" title={d}><span class="dim">{DAY[i] ?? d}</span><b>{showHours(sheet.day_totals[i] ?? '0')}</b></span>
			{/each}
		</div>
	{/if}
{:else if w.type === 'agent_summary'}
	{#if summary}
		{#each summary.today as t (t.agent_principal_id)}
			<div class="row"><span class="agname">{t.label}</span><span class="agent-label">agent</span> 今天改了 <b>{t.issues_changed}</b> 張<span style="flex:1"></span><a class="btn sm" href="/agents/{t.agent_principal_id}/activity">活動</a></div>
		{:else}
			<div class="empty">今天沒有 agent 寫入你看得到的單。</div>
		{/each}
	{/if}
{/if}

<style>
	.wrow { display: flex; align-items: center; gap: 6px; font-size: 13px; padding: 2px 0; }
	.ws { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
	.days { display: grid; grid-template-columns: repeat(7, 1fr); gap: 4px; margin-top: 6px; }
	.day { display: flex; flex-direction: column; align-items: center; font-size: 12px; background: var(--card2); border-radius: 6px; padding: 3px 0; }
</style>

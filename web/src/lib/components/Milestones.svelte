<script lang="ts">
	// 里程碑 (§17.8, §17.12): health and counts from the server (dates only,
	// never AI); create; change the date with an impact preview first; lock
	// or close. Writes need manage_versions; the server decides.
	import type { Impact, VersionList, VersionView } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { HEALTH, HEALTH_REASON, VERSION_STATUS } from '$lib/health';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from './ErrorBox.svelte';

	let { projectId, projectKey }: { projectId: string; projectKey: string } = $props();
	let data = $state<VersionList | null>(null);
	let error = $state<unknown>(null);
	let showClosed = $state(false);
	let creating = $state(false);
	let draft = $state({ name: '', effective_date: '', description: '' });
	let editing = $state<string | null>(null);
	let newDate = $state('');
	let newStatus = $state('');
	let impact = $state<Impact | null>(null);
	let actionError = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		try {
			data = await api('GET /projects/{id}/versions', { path: { id: projectId }, query: { status: showClosed ? 'open,locked,closed' : 'open,locked', include: 'stats,next' } });
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void projectId;
		void showClosed;
		load();
	});

	async function create() {
		busy = true;
		actionError = null;
		try {
			await api('POST /projects/{id}/versions', { path: { id: projectId }, body: { name: draft.name.trim(), effective_date: draft.effective_date || null, description: draft.description || null } });
			toast(`已建立里程碑 ${draft.name}`, 'ok');
			draft = { name: '', effective_date: '', description: '' };
			creating = false;
			await load();
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}
	function startEdit(v: VersionView) {
		editing = v.id;
		newDate = v.effective_date ?? '';
		newStatus = v.status;
		impact = null;
		actionError = null;
	}
	// Impact first (§17.8): how many open issues end up after the new date.
	$effect(() => {
		const id = editing;
		const d = newDate;
		const s = newStatus;
		if (!id) return;
		const v = data?.versions.find((x) => x.id === id);
		if (!v || ((d || null) === v.effective_date && s === v.status)) {
			impact = null;
			return;
		}
		api('GET /versions/{id}/impact', { path: { id }, query: { effective_date: d, status: s } })
			.then((r) => (impact = r.impact))
			.catch(() => (impact = null));
	});
	async function save(v: VersionView) {
		busy = true;
		actionError = null;
		try {
			const set: Record<string, unknown> = {};
			if ((newDate || null) !== v.effective_date) set.effective_date = newDate || null;
			if (newStatus !== v.status) set.status = newStatus;
			const r = await api('PATCH /versions/{id}', { path: { id: v.id }, body: { lock_version: v.lock_version, set } });
			toast(r.changed ? '已更新里程碑' : '沒有變更', 'ok');
			editing = null;
			await load();
		} catch (e) {
			actionError = e;
			if (e instanceof ApiError && e.code === 'conflict') await load();
		} finally {
			busy = false;
		}
	}
	const pct = (v: VersionView) => (v.stats && v.stats.total ? Math.round((v.stats.closed / v.stats.total) * 100) : 0);
</script>

<div class="row" style="gap:8px;margin:8px 0">
	<label class="row" style="gap:4px"><input type="checkbox" bind:checked={showClosed} /> 顯示已關閉</label>
	<span style="flex:1"></span>
	{#if data?.can_manage}<button class="btn sm" onclick={() => (creating = !creating)} data-testid="new-milestone">新增里程碑</button>{/if}
</div>
{#if data?.next?.version}
	<div class="hint" data-testid="next-version">下一個里程碑：<b>{data.next.version.name}</b></div>
{/if}
{#if creating}
	<div class="card" data-testid="milestone-form">
		<div class="row" style="gap:8px;flex-wrap:wrap">
			<label class="f" for="ms-name"><span>名稱 *</span><input id="ms-name" bind:value={draft.name} maxlength="60" /></label>
			<label class="f" for="ms-date"><span>日期</span><input id="ms-date" type="date" bind:value={draft.effective_date} /></label>
			<label class="f" for="ms-desc" style="flex:1"><span>說明</span><input id="ms-desc" bind:value={draft.description} /></label>
		</div>
		<div class="row" style="gap:8px;margin-top:6px">
			<button class="btn pri" onclick={create} disabled={busy || !draft.name.trim()} data-testid="ms-create">建立</button>
			<button class="btn" onclick={() => (creating = false)}>取消</button>
		</div>
	</div>
{/if}
<ErrorBox error={actionError} onaction={load} />
<ErrorBox {error} onaction={load} />
{#if data}
	<div class="hint">今天 {data.today}（組織時區）</div>
	{#each data.versions as v (v.id)}
		<div class="card" data-testid="milestone">
			<div class="row" style="gap:8px">
				<b>{v.name}</b>
				<span class="hint">{v.effective_date ?? '未定日期'}</span>
				<span class="chip c-dim">{VERSION_STATUS[v.status] ?? v.status}</span>
				{#if v.health}<span class="chip {HEALTH[v.health]?.cls ?? 'c-dim'}" data-testid="health">{HEALTH[v.health]?.label ?? v.health}</span>{/if}
				<span style="flex:1"></span>
				<a class="btn sm" href="/search?q={encodeURIComponent(`project:${projectKey} version:"${v.name}"`)}">看單</a>
				{#if data.can_manage}<button class="btn sm" onclick={() => startEdit(v)} data-testid="ms-edit">改日期／狀態</button>{/if}
			</div>
			{#if v.description}<div class="hint">{v.description}</div>{/if}
			{#if v.health_reasons?.length}<div class="hint">{v.health_reasons.map((r) => HEALTH_REASON[r] ?? r).join('、')}</div>{/if}
			{#if v.stats}
				<div class="row" style="gap:8px;margin-top:4px">
					<div class="tl-prog" title="完成 {v.stats.closed}／{v.stats.total}"><div style="width:{pct(v)}%"></div></div>
					<span class="hint">完成 {v.stats.closed}／{v.stats.total}{v.stats.overdue ? `・逾期 ${v.stats.overdue}` : ''}{v.stats.late_for_version ? `・晚於里程碑 ${v.stats.late_for_version}` : ''}{v.stats.no_dates ? `・沒有日期 ${v.stats.no_dates}` : ''}</span>
				</div>
			{/if}
			{#if v.health_thresholds}<div class="hint">風險門檻：{v.health_thresholds.at_risk_days ?? '—'} 天內、未完成超過 {v.health_thresholds.at_risk_open_pct ?? '—'}%（{v.health_thresholds.source === 'project' ? '專案設定' : '組織預設'}）</div>{/if}
			{#if editing === v.id}
				<div class="card" data-testid="milestone-edit">
					<div class="row" style="gap:8px">
						<label class="f" for="ms-d-{v.id}"><span>日期</span><input id="ms-d-{v.id}" type="date" bind:value={newDate} /></label>
						<label class="f" for="ms-s-{v.id}"><span>狀態</span>
							<select id="ms-s-{v.id}" bind:value={newStatus}>{#each Object.entries(VERSION_STATUS) as [k, l] (k)}<option value={k}>{l}</option>{/each}</select>
						</label>
					</div>
					{#if impact}
						<div class="banner {impact.due_after_new_date || impact.becomes_overdue_version ? 'w' : 'i'}" data-testid="impact">
							未完成 {impact.open_issues} 張{impact.due_after_new_date ? `，其中 ${impact.due_after_new_date} 張的到期日會晚於新日期` : ''}{impact.becomes_overdue_version ? '；這個里程碑會變成「已逾期」' : ''}。
						</div>
					{/if}
					<div class="row" style="gap:8px;margin-top:6px">
						<button class="btn pri" onclick={() => save(v)} disabled={busy} data-testid="ms-save">儲存</button>
						<button class="btn" onclick={() => (editing = null)}>取消</button>
					</div>
				</div>
			{/if}
		</div>
	{:else}
		<div class="empty">還沒有里程碑。</div>
	{/each}
{/if}

<style>
	.tl-prog { width: 160px; height: 6px; background: var(--bg2); border-radius: 3px; overflow: hidden; }
	.tl-prog div { height: 100%; background: var(--ok); }
</style>

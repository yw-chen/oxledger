<script lang="ts">
	// 一個 agent 的活動 (§16.10): direct writes, applied proposals, blocked
	// attempts and undos, each with its diff and, when possible, an undo
	// (preview first, then one click). 事故回復: pick a start time → preview →
	// (pause first, suggested) → one undo per item, newest first, each with
	// its own result; items that need a choice are listed to do one by one.
	import { page } from '$app/state';
	import type { ActivityItem, IncidentPreview, UndoPreview, UndoResult } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { fullIssueKey } from '$lib/aiteam';
	import { describeError } from '$lib/errors';
	import { fmtTime, fmtValue, fieldLabel } from '$lib/format';
	import { counts, refreshCounts } from '$lib/counts.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import { labelOf, show, type Lookups } from '$lib/render';
	import { projectLookups } from '$lib/trackers';
	import { session } from '$lib/session.svelte';

	const agentId = $derived(page.params.id ?? '');
	const role = $derived(counts.team.find((m) => m.id === agentId) ?? null);
	const label = $derived(role?.handle ?? 'agent');
	const isPaused = $derived(role?.status === 'paused');
	const mine = $derived(!!role && role.sponsor.id === session.user?.user_id);

	let items = $state<ActivityItem[]>([]);
	/** G24: the server filters by time and issue and pages with a cursor. */
	let next = $state<string | null>(null);
	let more = $state(false);
	let loadError = $state<unknown>(null);
	let issueFilter = $state('');
	let kindFilter = $state('');
	/** Hours back, 0 = all (§16.10: filter by time and issue). */
	let sinceHours = $state(0);
	let open = $state<string | null>(null);
	let preview = $state<UndoPreview | null>(null);
	let previewError = $state<unknown>(null);
	let skipFields = $state<Record<string, boolean>>({});
	let transitions = $state<Record<string, string>>({});
	let undoError = $state<unknown>(null);
	let undone = $state<Record<string, UndoResult>>({});
	let busy = $state(false);
	let look = $state<Lookups>({ fields: [], assignees: [], versions: [] });

	/** A full issue key goes to the server (`issue`); a partial one only filters what is loaded. */
	const serverIssue = $derived(fullIssueKey(issueFilter));
	function query(cursor?: string) {
		return {
			from: sinceHours ? new Date(Date.now() - sinceHours * 3600_000).toISOString() : undefined,
			issue: serverIssue || undefined,
			limit: 50,
			cursor
		};
	}
	async function load() {
		loadError = null;
		try {
			const a = await api('GET /agents/{id}/activity', { path: { id: agentId }, query: query() });
			items = a.items;
			next = a.next_cursor;
			const prefixes = new Set(items.flatMap((i) => i.keys.map((k) => k.split('-')[0])));
			const pids = session.projects.filter((p) => prefixes.has(p.key)).map((p) => p.id);
			look = await projectLookups(pids);
		} catch (e) {
			loadError = e;
		}
	}
	async function loadMore() {
		if (!next) return;
		more = true;
		try {
			const a = await api('GET /agents/{id}/activity', { path: { id: agentId }, query: query(next) });
			items = [...items, ...a.items];
			next = a.next_cursor;
		} catch (e) {
			loadError = e;
		} finally {
			more = false;
		}
	}
	$effect(() => {
		void agentId;
		void sinceHours;
		void serverIssue;
		load();
	});
	const shown = $derived(
		items.filter(
			(i) =>
				(!kindFilter || i.kind === kindFilter) &&
				(!issueFilter.trim() || i.keys.some((k) => k.toUpperCase().includes(issueFilter.trim().toUpperCase())))
		)
	);
	const KIND: Record<string, { t: string; c: string }> = {
		direct: { t: '直接寫入', c: 'c-act' },
		proposal: { t: '送審後套用', c: 'c-prop' },
		blocked: { t: '被擋下', c: 'c-bad' },
		undo: { t: '已回復', c: 'c-dim' }
	};

	async function openUndo(executionId: string) {
		// The preview is drawn inside the item's row: make sure it is listed.
		if (!shown.some((i) => i.execution_id === executionId)) {
			issueFilter = '';
			kindFilter = '';
			sinceHours = 0;
		}
		open = executionId;
		preview = null;
		previewError = null;
		undoError = null;
		skipFields = {};
		transitions = {};
		try {
			preview = await api('GET /executions/{id}/undo-preview', { path: { id: executionId } });
		} catch (e) {
			previewError = e;
		}
	}
	const REASON: Record<string, string> = {
		changed_since: '之後已被修改（保留後來的決定）',
		value_no_longer_valid: '舊值現在已不合法',
		permission: '你目前沒有權限修改',
		redacted: '內容已遮蔽',
		expired: '超過 30 天，資料已清除',
		in_use: '仍有議題使用',
		version_not_open: '里程碑已鎖定或關閉'
	};
	async function doUndo() {
		if (!preview) return;
		busy = true;
		undoError = null;
		try {
			const r = await api('POST /executions/{id}/undo', {
				path: { id: preview.execution_id },
				body: {
					skip_fields: Object.entries(skipFields).filter(([, v]) => v).map(([k]) => k),
					transitions: Object.fromEntries(Object.entries(transitions).filter(([, v]) => v))
				},
				operationId: newOperationId()
			});
			undone[preview.execution_id] = r;
			toast('已回復', 'ok');
			open = null;
			await load();
		} catch (e) {
			undoError = e;
		} finally {
			busy = false;
		}
	}
	const restorable = $derived(!!preview && (preview.fields.some((f) => f.restorable) || preview.notes.some((n) => n.restorable) || preview.relations.some((r) => r.restorable) || preview.choices.length > 0 || !!preview.version_created?.restorable || preview.version_fields.some((f) => f.restorable)));

	// ------------------------------------------------------------ 事故回復
	let since = $state('');
	let incident = $state<IncidentPreview | null>(null);
	let incidentError = $state<unknown>(null);
	let incidentResults = $state<Record<string, { ok: boolean; text: string }>>({});
	let running = $state(false);
	async function previewIncident() {
		incident = null;
		incidentError = null;
		incidentResults = {};
		try {
			incident = await api('POST /agents/{id}/incident-recovery/preview', { path: { id: agentId }, body: { since: new Date(since).toISOString() } });
		} catch (e) {
			incidentError = e;
		}
	}
	async function pauseFirst() {
		if (!role) return;
		try {
			await api('POST /ai-team/{id}/pause', { path: { id: role.id }, body: { scope: 'principal', reason: '事故回復' } });
			toast(`已暫停 ${label} 的寫入`, 'ok');
			refreshCounts();
		} catch (e) {
			incidentError = e;
		}
	}
	/** One undo per automatic item, newest first; each its own result (partial success is fine). */
	async function runIncident() {
		if (!incident) return;
		running = true;
		for (const it of incident.items.filter((x) => x.automatic && !x.needs_choice)) {
			try {
				const r = await api('POST /executions/{id}/undo', { path: { id: it.execution_id }, body: {}, operationId: newOperationId() });
				incidentResults[it.execution_id] = { ok: true, text: `還原 ${r.fields_restored} 欄、撤回 ${r.notes_retracted} 則 note、移除 ${r.relations_removed} 個關係` };
			} catch (e) {
				incidentResults[it.execution_id] = { ok: false, text: describeError(e).title };
				if (e instanceof ApiError && ['unauthenticated', 'csrf_failed'].includes(e.code)) break;
			}
		}
		running = false;
		await load();
	}
	const incidentSummary = $derived.by(() => {
		const rs = Object.values(incidentResults);
		if (!rs.length) return '';
		return `已回復 ${rs.filter((r) => r.ok).length}／${rs.length}${rs.some((r) => !r.ok) ? `；${rs.filter((r) => !r.ok).length} 筆沒有回復` : ''}`;
	});
</script>

{#snippet undoPanel()}
	<div class="card" data-testid="undo-preview">
		<ErrorBox error={previewError} />
		{#if preview}
			<b>回復預覽</b>
			{#if preview.expired}<div class="banner w">超過 30 天，這筆已不能回復。</div>{/if}
			{#each preview.fields as f (f.issue_id + f.field)}
				<div class="row" style="gap:6px">
					{#if f.restorable}<input type="checkbox" checked={!skipFields[`${f.issue_id}/${f.field}`]} onchange={(e) => (skipFields[`${f.issue_id}/${f.field}`] = !e.currentTarget.checked)} aria-label="還原 {f.key} {labelOf(f.field, look)}" />{:else}<span class="chip c-dim">跳過</span>{/if}
					<span class="mono">{f.key}</span> {labelOf(f.field, look)}：現在 {show(f.field, f.current, look)} → 還原成 <b>{show(f.field, f.before, look)}</b>
					{#if !f.restorable}<span class="hint">（{REASON[f.reason ?? ''] ?? f.reason}）</span>{/if}
				</div>
			{/each}
			{#each preview.notes as n (n.note_id)}<div>{n.restorable ? '撤回' : '不能撤回'} {n.key} 的 note{#if !n.restorable}<span class="hint">（{REASON[n.reason ?? ''] ?? n.reason}）</span>{/if}</div>{/each}
			{#each preview.relations as r (r.relation_id)}<div>{r.restorable ? '移除' : '不能移除'}「{r.source_key} 是 {r.target_key} 的重複」{#if !r.restorable}<span class="hint">（{REASON[r.reason ?? ''] ?? r.reason}）</span>{/if}</div>{/each}
			{#each preview.choices as c (c.issue_id)}
				<label class="row" style="gap:6px">{c.key}：{c.kind === 'close_as_mistaken' ? '關閉為誤建' : '轉回'}
					<select bind:value={transitions[c.issue_id]} aria-label="{c.key} 要轉到">
						<option value="">不動</option>
						{#each c.options as o (o.to_status_id)}<option value={o.to_status_id}>{o.name}</option>{/each}
					</select>
					{#if c.reason}<span class="hint">{REASON[c.reason] ?? c.reason}</span>{/if}
				</label>
			{/each}
			<ErrorBox error={undoError} />
			<div class="row" style="gap:8px;margin-top:6px">
				<button class="btn pri" onclick={doUndo} disabled={busy || !restorable || preview.expired} data-testid="undo-confirm">回復選取的部分</button>
				<button class="btn" onclick={() => (open = null)}>取消</button>
			</div>
		{/if}
	</div>
{/snippet}

<svelte:head><title>{label} 的活動・oxledger</title></svelte:head>
<div class="page">
	<a href={role ? `/ai-team/${encodeURIComponent(role.handle)}` : '/ai-team'} class="hint">← {role ? role.handle : 'AI 團隊'}</a>
	<div class="row" style="gap:8px">
		<h1 style="margin:0"><span class="agname">{label}</span> 的活動</h1>
		<span class="agent-label">agent</span>
		{#if isPaused}<span class="chip c-warn">寫入已暫停</span>{/if}
	</div>

	<div class="row" style="gap:8px;margin:8px 0">
		<input type="search" bind:value={issueFilter} placeholder="篩選單號，例如 AUTH-12" aria-label="篩選單號" style="width:200px" />
		<select bind:value={sinceHours} aria-label="篩選時間" data-testid="act-since">
			<option value={0}>最近 30 天</option>
			<option value={1}>最近 1 小時</option>
			<option value={24}>最近 24 小時</option>
			<option value={168}>最近 7 天</option>
		</select>
		<select bind:value={kindFilter} aria-label="篩選種類">
			<option value="">全部</option>
			{#each Object.entries(KIND) as [k, v] (k)}<option value={k}>{v.t}</option>{/each}
		</select>
	</div>
	<ErrorBox error={loadError} onaction={load} />

	{#if open && !shown.some((x) => x.execution_id === open)}
		{@render undoPanel()}
	{/if}
	{#each shown as it, i (it.execution_id ?? `${it.at}-${i}`)}
		<div class="card" style="padding:8px 12px" data-testid="activity">
			<div class="row" style="gap:8px">
				<span class="chip {KIND[it.kind]?.c ?? 'c-dim'}">{KIND[it.kind]?.t ?? it.kind}</span>
				{#each it.keys as k (k)}<a class="key" href="/issue/{k}">{k}</a>{/each}
				<span class="hint">{fmtTime(it.at)}</span>
				<span style="flex:1"></span>
				{#if it.execution_id && undone[it.execution_id]}<span class="chip c-ok">已回復</span>
				{:else if it.undoable && it.execution_id}<button class="btn sm" onclick={() => openUndo(it.execution_id!)} data-testid="undo">回復…</button>{/if}
			</div>
			{#if it.diff.length}
				<table class="t" style="margin-top:4px">
					<tbody>
						{#each it.diff as d, j (j)}<tr><td class="mono">{d.key}</td><td>{labelOf(d.field, look)}</td><td class="muted">{show(d.field, d.before, look)}</td><td>→</td><td>{show(d.field, d.after, look)}</td></tr>{/each}
					</tbody>
				</table>
			{/if}
			{#if open && open === it.execution_id}
				{@render undoPanel()}
			{/if}
		</div>
	{:else}
		{#if !loadError}<div class="empty">沒有活動。</div>{/if}
	{/each}
	{#if next}<button class="btn" onclick={loadMore} disabled={more} data-testid="act-more">{more ? '載入中…' : '載入更多'}</button>{/if}

	<div class="card" style="margin-top:16px" data-testid="incident">
		<b>事故回復</b>
		<p class="hint">回復 {label} 從某個時間起的所有變更。每筆各自執行、各自結果；需要選擇的（轉回、關閉為誤建）會列出來讓你逐筆處理。</p>
		<div class="row" style="gap:8px">
			<input type="datetime-local" bind:value={since} aria-label="起始時間" />
			<button class="btn" onclick={previewIncident} disabled={!since} data-testid="incident-preview">預覽</button>
		</div>
		<ErrorBox error={incidentError} />
		{#if incident}
			{#if mine && !isPaused}
				<div class="banner w">建議先暫停 {label} 的寫入，避免回復時它又寫入。<button class="btn sm" onclick={pauseFirst}>暫停寫入</button></div>
			{/if}
			<div class="hint">從 {fmtTime(incident.since)} 起，共 {incident.items.length} 筆（清單在 {fmtTime(incident.frozen_at)} 凍結）</div>
			{#each incident.items as it (it.execution_id)}
				{@const r = incidentResults[it.execution_id]}
				<div class="row" style="gap:6px">
					{#each it.keys as k (k)}<span class="mono">{k}</span>{/each}
					<span class="hint">{fmtTime(it.at)}</span>
					{#if it.needs_choice}<span class="chip c-warn">需要逐筆處理</span><button class="btn sm" onclick={() => openUndo(it.execution_id)}>打開</button>
					{:else if !it.automatic}<span class="chip c-dim">沒有可回復的內容</span>{/if}
					{#if r}<span class="chip {r.ok ? 'c-ok' : 'c-bad'}">{r.ok ? '已回復' : '沒有回復'}</span><span class="hint">{r.text}</span>{/if}
				</div>
			{/each}
			<div class="row" style="gap:8px;margin-top:6px">
				<button class="btn pri" onclick={runIncident} disabled={running || !incident.items.some((x) => x.automatic && !x.needs_choice)} data-testid="incident-run">回復可自動回復的 {incident.items.filter((x) => x.automatic && !x.needs_choice).length} 筆</button>
				{#if incidentSummary}<span class="hint" data-testid="incident-summary">{incidentSummary}</span>{/if}
			</div>
		{/if}
	</div>
</div>

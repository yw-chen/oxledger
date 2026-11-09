<script lang="ts">
	// One issue inside its project frame (§18.6), with direct operations
	// (human_direct, §6.8): edit fields, move status (only allowed ones), add a
	// note (optionally private), mark as duplicate. A conflict never
	// overwrites and keeps what the person typed.
	import { page } from '$app/state';
	import type { Assignee, CommandDoc, FieldSchema, SpaIssue, TrackerSchema, VersionView } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { changedFields, doc, issueRef, submit, wireFields } from '$lib/commands';
	import { fmtTime, fmtValue, fieldLabel } from '$lib/format';
	import { refreshCounts } from '$lib/counts.svelte';
	import { toast } from '$lib/toast.svelte';
	import { FLAG } from '$lib/health';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import FieldInput from '$lib/components/FieldInput.svelte';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import { issueValues, labelOf, show } from '$lib/render';

	const key = $derived(decodeURIComponent(page.params.key ?? '').toUpperCase());
	let issue = $state<SpaIssue | null>(null);
	let loadError = $state<unknown>(null);
	let schema = $state<TrackerSchema | null>(null);
	let assignees = $state<Assignee[]>([]);
	let versions = $state<VersionView[]>([]);
	let today = $state<string>(new Date().toLocaleDateString('sv-SE', { timeZone: 'Asia/Taipei' }));

	// Editing (kept across a reload so a conflict never loses input).
	let mode = $state<null | 'edit' | 'note' | 'dup' | 'transition'>(null);
	let start = $state<Record<string, unknown>>({});
	let form = $state<Record<string, unknown>>({});
	let note = $state({ body: '', private: false });
	let dupKey = $state('');
	let actionError = $state<unknown>(null);
	let conflict = $state(false);
	let busy = $state(false);
	let ripple = $state(false);

	async function load(k: string) {
		loadError = null;
		try {
			const v = await api('GET /issues/{key}/view', {
				path: { key: k },
				query: { include: 'actions,recent_notes,recent_events,dev', note_limit: 10, event_limit: 20 }
			});
			issue = v;
			if (!schema || schema.project_id !== v.project_id || schema.tracker_id !== v.tracker_id) {
				const [s, a, vl] = await Promise.all([
					api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: v.project_id, tracker_id: v.tracker_id } }),
					api('GET /projects/{id}/assignees', { path: { id: v.project_id } }).catch(() => ({ assignees: [] as Assignee[], project_id: '' })),
					api('GET /projects/{id}/versions', { path: { id: v.project_id }, query: { status: 'open,locked' } }).catch(() => null)
				]);
				schema = s;
				assignees = a.assignees;
				versions = vl?.versions ?? [];
				if (vl) today = vl.today;
			}
		} catch (e) {
			issue = null;
			loadError = e;
		}
	}
	$effect(() => {
		mode = null;
		conflict = false;
		load(key);
	});

	const currentValues = (v: SpaIssue) => issueValues(v, schema?.fields ?? []);
	const editable = $derived.by(() => {
		const a = issue?.actions;
		if (!a || !schema) return [] as { f: FieldSchema; required: boolean }[];
		return schema.fields
			.filter((f) => f.ops.includes('update'))
			.map((f) => ({ f, act: a.fields.find((x) => x.key === f.key) }))
			.filter((x) => x.act?.editable)
			.map((x) => ({ f: x.f, required: !!x.act?.required }));
	});

	function openEdit() {
		if (!issue) return;
		start = currentValues(issue);
		form = { ...start };
		mode = 'edit';
		actionError = null;
		conflict = false;
	}
	function openMode(m: 'note' | 'dup' | 'transition') {
		mode = m;
		actionError = null;
		conflict = false;
	}

	// The last document sent and its Idempotency-Key: a transient failure
	// ("尚未確認是否完成") is retried with exactly the same pair (§8.7);
	// anything else gets a fresh key next time.
	let last: { d: CommandDoc; op: string; done: string } | null = null;

	async function send(d: CommandDoc, done: string, op = newOperationId()) {
		busy = true;
		actionError = null;
		last = { d, op, done };
		try {
			await submit(d, op);
			last = null;
			toast(done, 'ok');
			mode = null;
			conflict = false;
			note = { body: '', private: false };
			dupKey = '';
			ripple = true;
			setTimeout(() => (ripple = false), 400);
			await load(key);
			refreshCounts();
		} catch (e) {
			actionError = e;
			if (!(e instanceof ApiError && e.retryable)) last = null;
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
		} finally {
			busy = false;
		}
	}
	function retry() {
		if (last) send(last.d, last.done, last.op);
	}
	const op = () => newOperationId();

	function saveEdit() {
		if (!issue) return;
		const set = wireFields(changedFields(start, form), schema?.fields ?? []);
		if (!Object.keys(set).length) {
			mode = null;
			return;
		}
		const lock = issue.lock_version ?? 0;
		return send(doc([{ kind: 'update_fields', op_id: op(), target: issueRef(key), set }], [{ key, lock }]), '已儲存');
	}
	function transition(to: string, name: string) {
		if (!issue) return;
		const lock = issue.lock_version ?? 0;
		return send(doc([{ kind: 'transition', op_id: op(), target: issueRef(key), to_status_id: to }], [{ key, lock }]), `已轉到「${name}」`);
	}
	function addNote() {
		if (!issue || !note.body.trim()) return;
		const lock = issue.lock_version ?? 0;
		return send(
			doc([{ kind: 'add_note', op_id: op(), target: issueRef(key), body: note.body, is_private: note.private }], [{ key, lock }]),
			note.private ? '已新增私有 note' : '已新增 note'
		);
	}
	async function markDup() {
		if (!issue) return;
		const target = dupKey.trim().toUpperCase();
		const lock = issue.lock_version ?? 0;
		actionError = null;
		let t;
		try {
			t = await api('GET /issues/{key}', { path: { key: target } });
		} catch (e) {
			actionError = e;
			return;
		}
		return send(
			doc([{ kind: 'duplicate_of', op_id: op(), source: issueRef(key), target: issueRef(t.key) }], [
				{ key, lock },
				{ key: t.key, lock: t.lock_version }
			]),
			`已標為 ${t.key} 的重複（不會關單）`
		);
	}
	/** Conflict: show the newest values, keep the person's input. */
	async function reloadKeep() {
		await load(key);
		if (mode === 'edit' && issue) start = currentValues(issue);
		conflict = false;
		actionError = null;
	}

	const flags = $derived.by(() => {
		const f: string[] = [];
		if (!issue) return f;
		const closed = issue.status_category === 'done';
		if (!closed && issue.due_date && issue.due_date < today) f.push('overdue');
		const vd = issue.fixed_version?.effective_date;
		if (vd && issue.due_date && issue.due_date > vd) f.push('late_for_version');
		if (!closed && issue.fixed_version && (issue.fixed_version.status === 'closed' || issue.fixed_version.deleted)) f.push('in_closed_version');
		return f;
	});
	const EVENT: Record<string, string> = {
		created: '建立',
		fields_changed: '修改欄位',
		status_changed: '轉狀態',
		note_added: '新增 note',
		duplicate_added: '標為重複'
	};
	/** History names custom fields `cf:N` and stores `{"<type>": v}`. */
	const lookups = $derived({ fields: schema?.fields ?? [], assignees, versions });
	const changeLabel = (k: string) => labelOf(k, lookups);
	const changeValue = (k: string, v: unknown) => show(k, v, lookups);
	function eventLines(detail: unknown): string[] {
		if (!detail || typeof detail !== 'object') return [];
		const d = detail as Record<string, unknown>;
		const out: string[] = [];
		const changes = (d.changes ?? d.fields) as Record<string, { old?: unknown; new?: unknown; before?: unknown; after?: unknown }> | undefined;
		if (changes && typeof changes === 'object') {
			for (const [k, c] of Object.entries(changes)) {
				if (c && typeof c === 'object') out.push(`${changeLabel(k)}：${changeValue(k, c.old ?? c.before)} → ${changeValue(k, c.new ?? c.after)}`);
			}
		}
		if ('old' in d || 'new' in d) out.push(`${changeValue('', d.old)} → ${changeValue('', d.new)}`);
		if (typeof d.other_key === 'string') out.push(d.role === 'source' ? `標為 ${d.other_key} 的重複` : `${d.other_key} 標為這張的重複`);
		for (const [k, v] of Object.entries(d)) {
			if (k === 'changes' || k === 'fields') continue;
			if (['from', 'to', 'status', 'target', 'source', 'approver', 'origin', 'on_behalf_of', 'is_private'].includes(k)) out.push(`${fieldLabel(k)}：${fmtValue(v)}`);
		}
		return out;
	}
	const PR: Record<string, string> = { open: '開啟', draft: '草稿', merged: '已合併', closed: '已關閉' };
	const CI: Record<string, { t: string; c: string }> = {
		success: { t: 'CI 通過', c: 'c-ok' },
		failure: { t: 'CI 失敗', c: 'c-bad' },
		pending: { t: 'CI 執行中', c: 'c-warn' }
	};
	const REVIEW: Record<string, string> = { approved: '已核可', changes_requested: '要求修改', pending: '等 review', none: '沒有 review' };
</script>

<svelte:head><title>{key}・oxledger</title></svelte:head>

{#if loadError}
	<div class="page">
		<!-- §18.6: no project frame when the issue cannot be seen. -->
		<ErrorBox error={loadError} onaction={() => load(key)} />
		<a href="/search">← 回到議題</a>
	</div>
{:else if issue}
	{@const v = issue}
	<ProjectFrame projectKey={v.project_key} issuePage>
		{#if v.requested_key && v.requested_key !== v.key}
			<div class="banner i">{v.requested_key} 已搬到 {v.key}。</div>
		{/if}
		<div class="row" style="gap:8px;margin:8px 0" class:ripple>
			<span class="key" style="font-size:15px">{v.key}</span>
			<h1 style="margin:0" data-testid="issue-subject">{v.subject}</h1>
			{#if v.is_private}<span class="chip c-warn">私有</span>{/if}
		</div>
		<div class="propstrip" data-testid="props">
			<div class="pc"><span class="k">狀態</span><span data-testid="status">{v.status}</span></div>
			<div class="pc"><span class="k">類型</span>{v.tracker}</div>
			<div class="pc"><span class="k">優先度</span>{v.priority ?? '—'}</div>
			<div class="pc"><span class="k">指派</span>{v.assignee ?? '未指派'}{v.assignee_kind === 'group' ? '（群組）' : ''}</div>
			<div class="pc"><span class="k">里程碑</span>
				{#if v.fixed_version}<a href="/project/{v.project_key}/milestones">{v.fixed_version.name}{v.fixed_version.deleted ? '（已刪除）' : ''}</a>{:else}—{/if}
			</div>
			<div class="pc"><span class="k">開始 → 到期</span><span class="mono">{v.start_date ?? '—'} → {v.due_date ?? '—'}</span></div>
			<div class="pc"><span class="k">作者</span>{v.author}</div>
			<div class="pc"><span class="k">更新</span>{fmtTime(v.updated_at)}</div>
			{#each flags as f (f)}<div class="pc"><span class="chip {FLAG[f].cls}">{FLAG[f].label}</span></div>{/each}
		</div>
		{#if v.custom_fields?.length}
			<dl class="field card" style="margin-bottom:10px">
				{#each v.custom_fields as cf (cf.name)}<dt>{cf.name}</dt><dd>{fmtValue(cf.value)}</dd>{/each}
			</dl>
		{/if}

		<div class="actbar" data-testid="actbar">
			{#if editable.length}<button class="btn" onclick={openEdit} data-testid="edit">編輯</button>{/if}
			{#if v.actions?.transitions.length}<button class="btn" onclick={() => openMode('transition')} data-testid="transition">轉狀態</button>{/if}
			{#if v.actions?.notes?.can_add}<button class="btn" onclick={() => openMode('note')} data-testid="add-note">新增 note</button>{/if}
			{#if v.actions?.duplicate_of?.can_propose}<button class="btn" onclick={() => openMode('dup')} data-testid="dup">標為重複</button>{/if}
		</div>

		{#if conflict}
			<div class="banner w" data-testid="conflict">
				這張單在你開啟後被別人改過，尚未套用任何變更；你填的內容還在。
				<span class="sp"></span><button class="btn sm" onclick={reloadKeep}>載入最新內容（保留我的輸入）</button>
			</div>
		{:else}
			<ErrorBox error={actionError} onaction={(a) => (a === 'retry' ? retry() : a === 'refresh' ? reloadKeep() : null)} />
		{/if}

		{#if mode === 'edit'}
			<div class="card">
				<h3>編輯</h3>
				{#each editable as e (e.f.key)}
					<FieldInput field={e.f} bind:value={form[e.f.key]} {assignees} {versions} required={e.required} />
				{/each}
				<div class="row">
					<button class="btn pri" onclick={saveEdit} disabled={busy || conflict} data-testid="save">{busy ? '儲存中…' : '儲存'}</button>
					<button class="btn" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{:else if mode === 'transition'}
			<div class="card">
				<h3>轉到哪個狀態？</h3>
				<p class="hint">只列出你現在可以用的轉換。</p>
				<div class="row">
					{#each v.actions?.transitions ?? [] as t (t.to_status_id)}
						<button class="btn" disabled={busy || t.blocked || conflict} title={t.blocked ? '有你看不到的條件擋住這個轉換' : t.then_required.length ? `轉換後必填：${t.then_required.map(fieldLabel).join('、')}` : ''} onclick={() => transition(t.to_status_id, t.name)}>{t.name}</button>
					{/each}
					<button class="btn ghost" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{:else if mode === 'note'}
			<div class="card">
				<h3>新增 note</h3>
				<textarea bind:value={note.body} placeholder="寫給專案成員看的補充" data-testid="note-body"></textarea>
				<div class="row" style="margin-top:6px">
					{#if v.actions?.notes?.can_set_private}<label><input type="checkbox" bind:checked={note.private} /> 私有（只有能看私有 note 的人看得到）</label>{/if}
					<span style="flex:1"></span>
					<button class="btn pri" onclick={addNote} disabled={busy || !note.body.trim() || conflict} data-testid="note-submit">新增</button>
					<button class="btn" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{:else if mode === 'dup'}
			<div class="card">
				<h3>標為重複</h3>
				<p class="hint">只建立「重複」關係，不會關閉任何單；兩張要在同一個專案。</p>
				<div class="inline-form">
					<span>{v.key} 是</span>
					<input type="text" bind:value={dupKey} placeholder="{v.project_key}-1" style="width:140px" data-testid="dup-target" />
					<span>的重複</span>
					<button class="btn pri" onclick={markDup} disabled={busy || !dupKey.trim() || conflict} data-testid="dup-submit">建立關係</button>
					<button class="btn" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{/if}

		<div class="grid2">
			<div>
				<h2>描述</h2>
				<div class="card pre" data-testid="description">{v.description || '（沒有描述）'}</div>
				{#if v.children?.length}
					<h2>子議題</h2>
					{#each v.children as c (c.key)}<div><a class="key" href="/issue/{c.key}">{c.key}</a> {c.subject} <span class="dim">{c.status}</span></div>{/each}
				{/if}
				<h2>Note</h2>
				{#each v.recent_notes?.items ?? [] as n (n.note_id)}
					<div class="note" class:priv={n.is_private} data-testid="note">
						<div class="h">{n.author}・{fmtTime(n.created_at)} {#if n.is_private}<span class="chip c-warn">私有</span>{/if}</div>
						{#if n.redacted}<div class="dim">（內容已遮蔽）</div>
						{:else if n.retracted}<div class="dim">（已由 {n.retracted_by ?? '委派者'} 撤回）</div>
						{:else}<div class="pre">{n.body}</div>{/if}
					</div>
				{:else}
					<div class="empty">還沒有 note。</div>
				{/each}
				{#if v.recent_notes?.truncated}<div class="hint">只顯示最新 10 則（第一版沒有完整 note 分頁）。</div>{/if}
			</div>
			<div>
				{#if v.dev}
					<div class="card" data-testid="dev-card">
						<h3>開發</h3>
						{#if v.dev.merged_open}<div class="banner i" style="font-size:12.5px">有 PR 已合併，但這張單還沒結案（只提示，不會自動改單）。</div>{/if}
						{#each v.dev.prs as pr (pr.pr_id)}
							<div class="dyn">
								<a href={pr.url} target="_blank" rel="noopener noreferrer">{pr.repo}#{pr.number}</a> <span class="chip c-dim">{PR[pr.state] ?? pr.state}</span>
								{#if pr.ci_state}<span class="chip {CI[pr.ci_state]?.c ?? 'c-dim'}">{CI[pr.ci_state]?.t ?? pr.ci_state}</span>{/if}
								<div style="font-size:12.5px">{pr.title}</div>
								<div class="hint">{REVIEW[pr.review_state] ?? pr.review_state}{pr.accessible ? '' : '・App 已無法讀取此 repo'}</div>
							</div>
						{:else}
							<div class="empty">還沒有 PR 提到 {v.key}。在 PR 標題、分支名或 commit 訊息寫上單號就會出現。</div>
						{/each}
						{#if v.dev.synced_at}<div class="hint">同步於 {fmtTime(v.dev.synced_at)}；可能落後 GitHub 幾分鐘。</div>{/if}
					</div>
				{/if}
				<h2>歷史</h2>
				{#each v.recent_events?.items ?? [] as ev (ev.event_id)}
					<div class="ev" data-testid="event">
						<b>{EVENT[ev.kind] ?? ev.kind}</b>・<span>{ev.actor}</span>
						<span class="dim">・{fmtTime(ev.at)}</span>
						{#if ev.redacted}<div class="dim">（內容已遮蔽）</div>{/if}
						{#each eventLines(ev.detail) as line, i (i)}<div>{line}</div>{/each}
					</div>
				{:else}
					<div class="empty">沒有歷史。</div>
				{/each}
				{#if v.recent_events?.truncated}<div class="hint">只顯示最新 20 筆。</div>{/if}
			</div>
		</div>
	</ProjectFrame>
{:else}
	<div class="page"><div class="sync"><span class="spin"></span>載入中</div></div>
{/if}

<script lang="ts">
	// 一份提案 (§15.3 single review, §15.4 conflicts). Opens at the version in
	// `?v=` and says so when it is not the current one (never switches by
	// itself). "核准並套用" binds version + hash; a timed-out apply is looked
	// up by its operation, never sent again as a new request. On a conflict:
	// three columns (原本／現在／提案), a choice per field (default: the
	// proposal), a full revision preview on every change, then one button
	// "建立修訂版並核准套用" (or "只儲存修訂版").
	import { page } from '$app/state';
	import type { Assignee, ChangesetView, ExecutionView, RevisionPreview, SpaIssue, Validation, VersionView } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { describeError, describeFailure } from '$lib/errors';
	import { goto } from '$app/navigation';
	import { fmtShort, fmtTime } from '$lib/format';
	import { refreshCounts, counts } from '$lib/counts.svelte';
	import { session } from '$lib/session.svelte';
	import { issueValues, labelOf, sameValue, show, type Lookups } from '$lib/render';
	import { toast } from '$lib/toast.svelte';
	import { projectLookups } from '$lib/trackers';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	type Op = Record<string, unknown> & { kind: string; op_id: string };
	type Doc = { title?: string; ops: Op[]; preconditions?: { target: { id?: string; key?: string }; lock_version: number; field_values?: Record<string, unknown> }[]; evidence?: { citation_id: string; quote?: string; note?: string }[] };
	type Target = { issue: SpaIssue; look: Lookups };

	const id = $derived(page.params.id ?? '');
	const wantV = $derived(Number(page.url.searchParams.get('v')) || null);

	let cs = $state<ChangesetView | null>(null);
	let shown = $state<ChangesetView | null>(null);
	let targets = $state<Record<string, Target>>({});
	let createLook = $state<Record<string, Lookups>>({});
	let validation = $state<Validation | null>(null);
	let loadError = $state<unknown>(null);
	let actionError = $state<unknown>(null);
	let busy = $state(false);
	let result = $state<ExecutionView | null>(null);
	let following = $state(false);
	let conflictOpen = $state(false);
	let last: { op: string; kind: 'approve' | 'revise' } | null = null;

	const NO_LOOK: Lookups = { fields: [], assignees: [], versions: [] };
	const doc = $derived((shown?.proposal ?? null) as Doc | null);
	const keyOf = (tid: string | undefined) => cs?.targets.find((t) => t.id === tid)?.key ?? '（看不到的單）';
	const current = $derived(!!cs && !!shown && shown.version === cs.current_version);
	/** Still decidable: waiting, or its last apply failed (§8). */
	const isOpen = (state: string | undefined) => state === 'proposed' || state === 'failed';

	async function lookupsFor(projectId: string, trackerId: string): Promise<Lookups> {
		const [s, a, v] = await Promise.all([
			api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: projectId, tracker_id: trackerId } }).catch(() => null),
			api('GET /projects/{id}/assignees', { path: { id: projectId } }).catch(() => ({ assignees: [] as Assignee[], project_id: '' })),
			api('GET /projects/{id}/versions', { path: { id: projectId }, query: { status: 'open,locked,closed' } }).catch(() => null)
		]);
		// A proposal may name a field the target tracker does not have (it will
		// fail validation): still show its name and option labels, from the
		// project's other trackers, like the review queue does.
		const fields = [...(s?.fields ?? [])];
		const all = await projectLookups([projectId]).catch(() => null);
		for (const f of all?.fields ?? []) if (!fields.some((x) => x.key === f.key)) fields.push(f);
		return { fields, assignees: a.assignees, versions: (v?.versions ?? []) as VersionView[] };
	}

	async function load() {
		loadError = null;
		try {
			const c = await api('GET /changesets/{id}', { path: { id } });
			cs = c;
			const v = wantV ?? c.current_version;
			shown = v === c.current_version ? c : await api('GET /changesets/{id}/versions/{v}', { path: { id, v } });
			await loadTargets();
			validation = null;
			if (isOpen(c.state) && shown.version === c.current_version && shown.proposal) {
				validation = await api('POST /changesets/{id}/validations', { path: { id }, body: { version: c.current_version } }).catch(() => null);
				if (validation?.status === 'conflict') conflictOpen = true;
			}
		} catch (e) {
			loadError = e;
		}
	}
	async function loadTargets() {
		const t: Record<string, Target> = {};
		await Promise.all(
			(cs?.targets ?? []).map(async (x) => {
				try {
					const issue = await api('GET /issues/{key}/view', { path: { key: x.key }, query: { include: 'actions' } });
					t[x.id] = { issue, look: await lookupsFor(issue.project_id, issue.tracker_id) };
				} catch {
					/* unreadable: shown as such */
				}
			})
		);
		targets = t;
		const cl: Record<string, Lookups> = {};
		for (const op of doc?.ops ?? []) {
			if (op.kind === 'create_issue') {
				const k = `${op.project_id}/${op.tracker_id}`;
				if (!cl[k]) cl[k] = await lookupsFor(String(op.project_id), String(op.tracker_id));
			}
		}
		createLook = cl;
	}
	$effect(() => {
		void id;
		void wantV;
		result = null;
		actionError = null;
		conflictOpen = false;
		load();
	});

	const agentName = $derived(counts.contexts.find((c) => c.agent_principal_id === cs?.proposer_id)?.agent_label ?? 'agent');
	const projectName = (pid: unknown) => session.projects.find((p) => p.id === pid)?.key ?? '（專案）';
	function statusName(t: Target | undefined, sid: unknown): string {
		if (!t) return String(sid);
		if (t.issue.status_id === sid) return t.issue.status ?? '';
		return t.issue.actions?.transitions.find((x) => x.to_status_id === sid)?.name ?? '（目前不能轉到的狀態）';
	}

	// ------------------------------------------------------------ approve / reject
	async function approve(retry = false) {
		if (!shown || !cs) return;
		const op = retry && last?.kind === 'approve' ? last.op : newOperationId();
		last = { op, kind: 'approve' };
		busy = true;
		actionError = null;
		try {
			const ex = await api('POST /changesets/{id}/approve-and-apply', {
				path: { id },
				body: { version: shown.version, proposal_hash: shown.proposal_hash, review_mode: 'detail' },
				operationId: op
			});
			await settle(ex, op);
		} catch (e) {
			await onError(e, op);
		} finally {
			busy = false;
		}
	}
	async function settle(ex: ExecutionView, op: string): Promise<void> {
		result = ex;
		if (ex.status === 'queued' || ex.status === 'running') return follow(op);
		last = null;
		if (ex.status === 'succeeded') {
			toast('已套用', 'ok');
			refreshCounts();
		} else if (ex.error?.code === 'conflict') conflictOpen = true;
		await load();
		result = ex;
	}
	async function onError(e: unknown, op: string) {
		actionError = e;
		if (!(e instanceof ApiError)) return;
		if (e.code === 'execution_in_progress') return follow(op);
		if (!e.retryable) last = null;
		if (e.code === 'conflict') conflictOpen = true;
	}
	/** §15.5: "結果確認中" looks the original operation up. */
	async function follow(op: string): Promise<void> {
		following = true;
		try {
			for (let i = 0; i < 15; i++) {
				await new Promise((r) => setTimeout(r, 1000 * Math.min(4, i + 1)));
				try {
					const o = await api('GET /operations/{id}', { path: { id: op } });
					const ex = o.execution;
					if (o.status === 'in_progress' || !ex || ex.status === 'queued' || ex.status === 'running') continue;
					actionError = null;
					following = false;
					return settle(ex, op);
				} catch (e) {
					if (e instanceof ApiError && e.code === 'not_found') {
						actionError = new ApiError(0, null, op, '沒有送達');
						return;
					}
				}
			}
		} finally {
			following = false;
		}
	}
	async function reject() {
		if (!shown) return;
		busy = true;
		actionError = null;
		try {
			await api('POST /changesets/{id}/reject', { path: { id }, body: { version: shown.version, proposal_hash: shown.proposal_hash } });
			toast('已拒絕', 'ok');
			refreshCounts();
			await load();
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}

	// ------------------------------------------------------------ conflict (§15.4)
	type Row = { choice: string; key: string; field: string; label: string; original: unknown; hasOriginal: boolean; now: unknown; proposed: unknown; look: Lookups };
	const rows = $derived.by(() => {
		const out: Row[] = [];
		for (const op of doc?.ops ?? []) {
			if (op.kind !== 'update_fields') continue;
			const tid = (op.target as { id?: string })?.id;
			const t = tid ? targets[tid] : undefined;
			const pre = doc?.preconditions?.find((p) => p.target.id === tid);
			const now = t ? issueValues(t.issue, t.look.fields) : {};
			for (const [k, v] of Object.entries((op.set ?? {}) as Record<string, unknown>)) {
				out.push({
					choice: `${op.op_id}/${k}`,
					key: keyOf(tid),
					field: k,
					label: labelOf(k, t?.look ?? NO_LOOK),
					original: pre?.field_values?.[k],
					hasOriginal: !!pre?.field_values && k in pre.field_values,
					now: now[k],
					proposed: v,
					look: t?.look ?? NO_LOOK
				});
			}
		}
		return out;
	});
	let choices = $state<Record<string, 'current' | 'proposed'>>({});
	let preview = $state<RevisionPreview | null>(null);
	let previewError = $state<unknown>(null);
	let previewing = $state(false);
	let outdated = $state(false);
	const knownOriginals = $derived(rows.filter((r) => r.hasOriginal));
	const nothingOfMineChanged = $derived(knownOriginals.length === rows.length && rows.length > 0 && rows.every((r) => sameValue(r.original, r.now)));

	let timer: ReturnType<typeof setTimeout> | undefined;
	$effect(() => {
		if (!conflictOpen || !cs || !isOpen(cs.state)) return;
		const c = { ...choices };
		const v = cs.current_version;
		clearTimeout(timer);
		timer = setTimeout(() => runPreview(v, c), 300);
	});
	async function runPreview(version: number, c: Record<string, string>) {
		previewing = true;
		previewError = null;
		try {
			preview = await api('POST /changesets/{id}/revision-previews', { path: { id }, body: { version, choices: c } });
		} catch (e) {
			preview = null;
			previewError = e;
		} finally {
			previewing = false;
		}
	}
	async function reviseAndApply(retry = false) {
		if (!cs || !preview) return;
		const op = retry && last?.kind === 'revise' ? last.op : newOperationId();
		last = { op, kind: 'revise' };
		busy = true;
		actionError = null;
		outdated = false;
		try {
			const ex = await api('POST /changesets/{id}/revise-and-apply', {
				path: { id },
				body: { version: cs.current_version, expected_current_version: cs.current_version, proposal: preview.proposal as never, revision_review_token: preview.revision_review_token },
				operationId: op
			});
			conflictOpen = ex.status !== 'succeeded' && ex.error?.code === 'conflict';
			await settle(ex, op);
		} catch (e) {
			if (e instanceof ApiError && e.code === 'review_outdated') {
				// Changed after the preview: nothing was approved or saved; preview again.
				outdated = true;
				last = null;
				await loadTargets();
				await runPreview(cs.current_version, { ...choices });
			} else await onError(e, op);
		} finally {
			busy = false;
		}
	}
	async function saveOnly() {
		if (!cs || !preview) return;
		busy = true;
		actionError = null;
		try {
			await api('POST /changesets/{id}/revisions', { path: { id }, body: { expected_current_version: cs.current_version, proposal: preview.proposal as never } });
			toast('已儲存修訂版（尚未套用）', 'ok');
			choices = {};
			await load();
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}

	function onAction(a: string | undefined) {
		if (a === 'latest') location.href = `/cs/${id}`;
		else if (a === 'conflict') conflictOpen = true;
		else if (a === 'retry') last?.kind === 'revise' ? reviseAndApply(true) : approve(true);
		else if (a === 'delegations') goto('/agents');
		else if (a === 'queue') goto('/review');
		else if (a === 'login') session.expired = true;
		else load();
	}
	const STATE: Record<string, string> = { proposed: '待審', failed: '套用失敗', succeeded: '已套用', applied: '已套用', rejected: '已拒絕', canceled: '已取消', expired: '已到期', superseded: '已取代' };
	const CHECK: Record<string, string> = { audience: '受眾', structure: '格式', visibility: '可見性', permissions: '權限', preconditions: '前置條件', workflow: '流程' };
</script>

<svelte:head><title>提案・oxledger</title></svelte:head>
<div class="page">
	<a href="/review" class="hint">← 待審</a>
	<ErrorBox error={loadError} onaction={() => load()} />
	{#if cs && shown}
		<div class="row" style="gap:8px;margin:6px 0">
			<h1 style="margin:0" data-testid="cs-title">{doc?.title ?? '（沒有標題）'}</h1>
			<span class="chip c-dim" data-testid="cs-state">{STATE[cs.state] ?? cs.state}</span>
		</div>
		<div class="hint">{agentName} 代你提出・第 {shown.version} 版・{fmtTime(shown.version_created_at)}・到期 {fmtShort(cs.expires_at)}</div>
		{#if !current}
			<div class="banner w" data-testid="old-version">你看的是第 {shown.version} 版；目前是第 {cs.current_version} 版。舊版本不能核准。<a href="/cs/{id}">看最新版本</a></div>
		{/if}
		{#if shown.content_unavailable}
			<div class="banner w">提案內容目前無法完整存取，不能套用。</div>
		{/if}

		{#if validation}
			<div class="card" data-testid="validation">
				<div class="row" style="gap:8px">
					<b>檢查</b>
					<span class="chip {validation.status === 'passed' ? 'c-ok' : validation.status === 'conflict' ? 'c-warn' : 'c-bad'}">{validation.status === 'passed' ? '通過' : validation.status === 'conflict' ? '有衝突' : '未通過'}</span>
					<span class="hint">{fmtTime(validation.checked_at)}</span>
				</div>
				<div class="hint">{Object.entries(validation.checks).map(([k, v]) => `${CHECK[k] ?? k}：${v === 'passed' ? '✓' : v === 'failed' ? '✗' : '—'}`).join('　')}</div>
				{#if validation.error && validation.status !== 'conflict'}<div class="hint">{describeError(new ApiError(409, { ...validation.error, request_id: '', retryable: false } as never, null)).title}</div>{/if}
				{#each validation.warnings as w (w)}<div class="hint">⚠ {w === 'evidence_changed' ? '根據已變動（引用的內容不再相符）' : w}</div>{/each}
			</div>
		{/if}

		{#each doc?.ops ?? [] as op (op.op_id)}
			{@const tid = (op.target as { id?: string } | undefined)?.id}
			{@const t = tid ? targets[tid] : undefined}
			<div class="card" data-testid="op">
				{#if op.kind === 'update_fields'}
					<div><b>修改欄位</b>・<a class="key" href="/issue/{keyOf(tid)}">{keyOf(tid)}</a> {t?.issue.subject ?? ''}</div>
					<table class="t">
						<thead><tr><th>欄位</th><th>現在</th><th>提案</th></tr></thead>
						<tbody>
							{#each Object.entries((op.set ?? {}) as Record<string, unknown>) as [k, v] (k)}
								{@const now = t ? issueValues(t.issue, t.look.fields)[k] : undefined}
								<tr><td>{labelOf(k, t?.look ?? NO_LOOK)}</td><td class="muted">{t ? show(k, now, t.look) : '—'}</td><td><b>{show(k, v, t?.look ?? NO_LOOK)}</b></td></tr>
							{/each}
						</tbody>
					</table>
				{:else if op.kind === 'transition'}
					<div><b>轉狀態</b>・<a class="key" href="/issue/{keyOf(tid)}">{keyOf(tid)}</a>：{t?.issue.status ?? '—'} → <b>{statusName(t, op.to_status_id)}</b></div>
				{:else if op.kind === 'add_note'}
					<div><b>新增{op.is_private ? '私有 ' : ' '}note</b>・<a class="key" href="/issue/{keyOf(tid)}">{keyOf(tid)}</a></div>
					{#if op.is_private}<div class="hint">私有 note：只有能看私有 note 的成員看得到。</div>{/if}
					<div class="card pre" data-testid="note-body">{String(op.body ?? '')}</div>
				{:else if op.kind === 'create_issue'}
					{@const look = createLook[`${op.project_id}/${op.tracker_id}`] ?? NO_LOOK}
					<div><b>建立議題</b>・{projectName(op.project_id)}</div>
					<table class="t">
						<tbody>
							{#each Object.entries((op.fields ?? {}) as Record<string, unknown>) as [k, v] (k)}
								<tr><td>{labelOf(k, look)}</td><td class:pre={k === 'description'}>{show(k, v, look)}</td></tr>
							{/each}
						</tbody>
					</table>
				{:else if op.kind === 'duplicate_of'}
					{@const s = (op.source as { id?: string })?.id}
					<div><b>標為重複</b>：<a class="key" href="/issue/{keyOf(s)}">{keyOf(s)}</a> 是 <a class="key" href="/issue/{keyOf(tid)}">{keyOf(tid)}</a> 的重複（不會關單）</div>
				{:else}
					<div><b>{op.kind}</b></div>
				{/if}
			</div>
		{/each}

		{#if doc?.evidence?.length}
			<div class="card" data-testid="evidence">
				<b>根據</b>
				{#each doc.evidence as ev (ev.citation_id)}<blockquote class="hint" style="margin:4px 0">{ev.quote ?? ev.note ?? ev.citation_id}</blockquote>{/each}
			</div>
		{/if}

		{#if conflictOpen && isOpen(cs.state) && current}
			<div class="card w" data-testid="conflict-panel">
				<b>這張單在提案後被改過，尚未套用任何變更</b>
				{#if nothingOfMineChanged}<div class="hint">你提案的欄位都沒變（是別的內容變了），可以直接套用。</div>{/if}
				{#if outdated}<div class="banner w">預覽之後資料又變了，請再確認一次（已重新預覽）。</div>{/if}
				{#if rows.length}<table class="t" style="margin-top:6px">
					<thead><tr><th>單</th><th>欄位</th><th>原本</th><th>現在</th><th>提案</th></tr></thead>
					<tbody>
						{#each rows as r (r.choice)}
							<tr class:changed={r.hasOriginal && !sameValue(r.original, r.now)}>
								<td class="mono">{r.key}</td>
								<td>{r.label}</td>
								<td class="muted">{r.hasOriginal ? show(r.field, r.original, r.look) : '（提案時沒有記錄）'}</td>
								<td><label><input type="radio" name={r.choice} checked={choices[r.choice] === 'current'} onchange={() => (choices[r.choice] = 'current')} data-testid="use-current" /> {show(r.field, r.now, r.look)}</label></td>
								<td><label><input type="radio" name={r.choice} checked={(choices[r.choice] ?? 'proposed') === 'proposed'} onchange={() => (choices[r.choice] = 'proposed')} data-testid="use-proposed" /> <b>{show(r.field, r.proposed, r.look)}</b></label></td>
							</tr>
						{/each}
					</tbody>
				</table>{/if}
				{#if !rows.length}<div class="hint">這份提案沒有要逐欄選的欄位（例如 note 或轉狀態）；修訂版會以目前的資料為基準，內容不變。</div>{/if}
				<ErrorBox error={previewError} onaction={() => cs && runPreview(cs.current_version, { ...choices })} />
				{#if preview}
					<div class="hint" data-testid="preview-status">
						{previewing ? '預覽中…' : preview.validation.status === 'passed' ? '修訂後的內容檢查通過' : `修訂後仍有問題：${preview.validation.blocking.map((b) => b.field ?? b.code).join('、')}`}
					</div>
				{/if}
				<div class="row" style="gap:8px;margin-top:8px">
					<button class="btn pri" onclick={() => reviseAndApply()} disabled={busy || previewing || !preview || preview.validation.status !== 'passed'} data-testid="revise-apply">建立修訂版並核准套用</button>
					<button class="btn" onclick={saveOnly} disabled={busy || previewing || !preview} data-testid="save-revision">只儲存修訂版</button>
				</div>
			</div>
		{/if}

		<ErrorBox error={actionError} onaction={onAction} />
		{#if following}<div class="sync"><span class="spin"></span>結果確認中，請勿重複按；會自動更新</div>{/if}
		{#if result}
			<div class="card" data-testid="exec-result">
				{#if result.status === 'succeeded'}
					<span class="chip c-ok">已套用</span>
					{#each result.effects as e (e.issue_id)}<a class="btn sm" href="/issue/{e.key}">開啟 {e.key}</a>{/each}
					<div class="hint">搜尋可能要幾秒才找得到；單本身已經可以開啟。</div>
				{:else if result.status === 'failed'}
					<span class="chip c-bad">沒有套用</span> <span class="hint">{describeFailure(result.error)}</span>
				{:else}
					<span class="chip c-warn">確認中</span>
				{/if}
			</div>
		{/if}

		{#if isOpen(cs.state) && current}
			<div class="row" style="gap:8px;margin-top:10px" data-testid="decide">
				{#if !conflictOpen}<button class="btn pri" onclick={() => approve()} disabled={busy || following || !!shown.content_unavailable} data-testid="approve">{busy ? '套用中…' : cs.state === 'failed' ? '再核准並套用一次' : '核准並套用'}</button>{/if}
				<button class="btn" onclick={reject} disabled={busy || following} data-testid="reject">拒絕</button>
			</div>
		{/if}
	{/if}
</div>

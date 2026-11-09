<script lang="ts">
	// 待審佇列 (§15.3, U11). Only rows whose `review_modes` include `queue` can
	// be ticked; nothing is pre-selected; two ticked rows never touch the same
	// issue; a tick binds version + hash. Each ticked row is its own
	// approve-and-apply (own Idempotency-Key, review_mode "queue"), at most 4
	// in flight. 401 / CSRF / service errors stop what has not been sent;
	// "停止" only affects rows not yet sent. Partial success is shown per row.
	import type { ChangesetSummary } from '$api/types';
	import { api, ApiError, newOperationId, pool } from '$lib/api';
	import { describeError } from '$lib/errors';
	import { fieldLabel, fmtShort, fmtValue } from '$lib/format';
	import { refreshCounts } from '$lib/counts.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	type RowResult = { state: 'sending' | 'ok' | 'failed' | 'pending' | 'not_sent'; text?: string; keys?: string[]; op?: string };

	let bucket = $state<'awaiting_me' | 'all'>('awaiting_me');
	let items = $state<ChangesetSummary[]>([]);
	let cursor = $state<string | null>(null);
	let loadError = $state<unknown>(null);
	let loading = $state(false);
	/** changeset id → the version + hash the tick was made on. */
	let picked = $state<Record<string, { version: number; hash: string }>>({});
	let results = $state<Record<string, RowResult>>({});
	let running = $state(false);
	let stopRequested = false;

	async function load(more = false) {
		loading = true;
		loadError = null;
		try {
			const r = await api('GET /changesets', { query: { bucket, cursor: more ? cursor : null } });
			const next = more ? [...items, ...r.items] : r.items;
			// A proposal revised since it was ticked is unticked (the tick binds version + hash).
			for (const [id, p] of Object.entries(picked)) {
				const row = next.find((x) => x.changeset_id === id);
				if (!row || row.current_version !== p.version || row.proposal_hash !== p.hash || !row.review_modes.includes('queue')) delete picked[id];
			}
			items = next;
			cursor = r.next_cursor;
		} catch (e) {
			loadError = e;
		} finally {
			loading = false;
		}
	}
	$effect(() => {
		void bucket;
		picked = {};
		results = {};
		load();
	});

	const issueIds = (c: ChangesetSummary) => c.targets.map((t) => t.id);
	const pickedRows = $derived(items.filter((c) => picked[c.changeset_id]));
	const pickedIssues = $derived(new Set(pickedRows.flatMap(issueIds)));
	const queueable = (c: ChangesetSummary) => c.state === 'proposed' && c.review_modes.includes('queue') && !!c.proposal_hash && (!results[c.changeset_id] || results[c.changeset_id].state === 'not_sent');
	function blockedBySameIssue(c: ChangesetSummary) {
		return !picked[c.changeset_id] && issueIds(c).some((id) => pickedIssues.has(id));
	}
	function toggle(c: ChangesetSummary) {
		if (picked[c.changeset_id]) delete picked[c.changeset_id];
		else if (queueable(c) && !blockedBySameIssue(c)) picked[c.changeset_id] = { version: c.current_version, hash: c.proposal_hash! };
	}
	/** Ticks only the rows shown on this page that may be ticked. */
	function pickVisible() {
		for (const c of items) if (queueable(c) && !picked[c.changeset_id] && !blockedBySameIssue(c)) picked[c.changeset_id] = { version: c.current_version, hash: c.proposal_hash! };
	}
	const STOPPING = new Set(['unauthenticated', 'csrf_failed', 'temporarily_unavailable', 'network', 'internal_error', 'rate_limited']);

	async function applyPicked() {
		const rows = pickedRows.map((c) => ({ c, p: picked[c.changeset_id] }));
		if (!rows.length) return;
		running = true;
		stopRequested = false;
		for (const { c } of rows) results[c.changeset_id] = { state: 'not_sent' };
		await pool(
			rows,
			4,
			async ({ c, p }) => {
				const op = newOperationId();
				results[c.changeset_id] = { state: 'sending', op };
				try {
					const ex = await api('POST /changesets/{id}/approve-and-apply', {
						path: { id: c.changeset_id },
						body: { version: p.version, proposal_hash: p.hash, review_mode: 'queue' },
						operationId: op
					});
					results[c.changeset_id] = outcome(ex.status, ex.effects.map((e) => e.key), ex.error?.message, op);
					if (ex.status === 'queued' || ex.status === 'running') void follow(c.changeset_id, op);
				} catch (e) {
					const code = e instanceof ApiError ? e.code : 'internal_error';
					if (code === 'execution_in_progress') {
						results[c.changeset_id] = { state: 'pending', text: '結果確認中', op };
						void follow(c.changeset_id, op);
					} else {
						const retry = e instanceof ApiError && e.retryable;
						results[c.changeset_id] = { state: retry ? 'pending' : 'failed', text: describeError(e).title, op };
						if (retry) void follow(c.changeset_id, op);
					}
					if (STOPPING.has(code)) stopRequested = true;
				}
				delete picked[c.changeset_id];
			},
			() => stopRequested
		);
		running = false;
		refreshCounts();
	}
	function outcome(status: string, keys: string[], msg: string | undefined, op: string): RowResult {
		if (status === 'succeeded') return { state: 'ok', keys, op };
		if (status === 'failed') return { state: 'failed', text: msg ?? '套用失敗', op };
		return { state: 'pending', text: '結果確認中', op };
	}
	/** Looks the original operation up (never a new request, §15.5). */
	async function follow(id: string, op: string) {
		for (let i = 0; i < 10; i++) {
			await new Promise((r) => setTimeout(r, 1000 * Math.min(4, i + 1)));
			try {
				const o = await api('GET /operations/{id}', { path: { id: op } });
				if (o.status === 'in_progress') continue;
				const ex = o.execution;
				results[id] = ex ? outcome(ex.status, ex.effects.map((e) => e.key), ex.error?.message, op) : { state: 'failed', text: '操作沒有完成', op };
				if (ex && (ex.status === 'queued' || ex.status === 'running')) continue;
				return;
			} catch (e) {
				if (e instanceof ApiError && e.code === 'not_found') {
					// The request never reached the server: nothing was applied.
					results[id] = { state: 'not_sent', text: '沒有送達，沒有改到資料', op };
					return;
				}
			}
		}
	}
	function stop() {
		stopRequested = true;
	}
	const summary = $derived.by(() => {
		const rs = Object.values(results);
		if (!rs.length) return '';
		const n = (s: RowResult['state']) => rs.filter((r) => r.state === s).length;
		const parts = [`已完成 ${n('ok')}／${rs.length}`];
		if (n('failed')) parts.push(`${n('failed')} 份失敗`);
		if (n('pending') + n('sending')) parts.push(`${n('pending') + n('sending')} 份確認中`);
		if (n('not_sent')) parts.push(`${n('not_sent')} 份未送出`);
		return parts.join('；');
	});
	const STATE: Record<string, string> = { proposed: '待審', failed: '套用失敗', applied: '已套用', rejected: '已拒絕', canceled: '已取消', expired: '已到期', superseded: '已取代', approved: '已核准' };
	const WHY_NOT: Record<string, string> = { evidence_changed: '根據已變動' };
	function whyNotQueue(c: ChangesetSummary): string {
		if (c.state !== 'proposed') return '';
		if (c.warnings.length) return c.warnings.map((w) => WHY_NOT[w] ?? w).join('、') + '：請打開看';
		if (c.validation?.status && c.validation.status !== 'passed') return c.validation.status === 'conflict' ? '有衝突：請打開處理' : '檢查沒通過：請打開看';
		return '內容需要打開看（長文、note、建立、關係或私有設定）';
	}
</script>

<svelte:head><title>待審・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px">
		<h1 style="margin:0">待審</h1>
		<span style="flex:1"></span>
		<button class="btn sm" class:on={bucket === 'awaiting_me'} onclick={() => (bucket = 'awaiting_me')}>等我處理</button>
		<button class="btn sm" class:on={bucket === 'all'} onclick={() => (bucket = 'all')}>全部提案</button>
	</div>
	<p class="hint">agent 替你提出的變更。可以勾選的列（短欄位更新、單次轉狀態、檢查通過且沒有警告）可以一次核准；其他要打開看。</p>

	<div class="row" style="gap:8px;margin:8px 0" data-testid="queue-bar">
		<button class="btn sm" onclick={pickVisible} disabled={running}>選取本頁符合的</button>
		<button class="btn pri" onclick={applyPicked} disabled={running || !pickedRows.length} data-testid="apply-picked">核准並套用所選（{pickedRows.length} 份）</button>
		{#if running}<button class="btn" onclick={stop} data-testid="stop">停止（只影響未送出的）</button>{/if}
		{#if summary}<span class="hint" data-testid="queue-summary">{summary}</span>{/if}
	</div>

	<ErrorBox error={loadError} onaction={() => load()} />
	{#if !items.length && !loading && !loadError}
		<div class="empty">{bucket === 'awaiting_me' ? '目前沒有等你處理的提案。' : '還沒有任何提案。'}</div>
	{/if}

	{#each items as c (c.changeset_id)}
		{@const r = results[c.changeset_id]}
		{@const canTick = queueable(c)}
		<div class="card" style="padding:10px 12px" data-testid="cs-row" data-id={c.changeset_id}>
			<div class="row" style="gap:8px;align-items:flex-start">
				<input
					type="checkbox"
					aria-label="勾選這份提案"
					checked={!!picked[c.changeset_id]}
					disabled={running || !canTick || blockedBySameIssue(c)}
					title={!canTick ? whyNotQueue(c) : blockedBySameIssue(c) ? '同一次選取不能有兩份改同一張單' : ''}
					onchange={() => toggle(c)}
					data-testid="tick"
				/>
				<div style="flex:1;min-width:0">
					<div class="row" style="gap:8px">
						<a href="/cs/{c.changeset_id}"><b>{c.title ?? '（沒有標題）'}</b></a>
						<span class="chip c-dim">{STATE[c.state] ?? c.state}</span>
						{#if c.validation}<span class="chip {c.validation.status === 'passed' ? 'c-ok' : c.validation.status === 'conflict' ? 'c-warn' : 'c-bad'}">{c.validation.status === 'passed' ? '檢查通過' : c.validation.status === 'conflict' ? '有衝突' : '檢查未通過'}{c.validation.checked_at ? `・${fmtShort(c.validation.checked_at)}` : ''}</span>{/if}
						{#each c.warnings as w (w)}<span class="chip c-warn">{WHY_NOT[w] ?? w}</span>{/each}
						<span class="hint">到期 {fmtShort(c.expires_at)}</span>
					</div>
					<div class="hint">{#each c.targets as t, i (t.id)}{i ? '、' : ''}<a class="key" href="/issue/{t.key}">{t.key}</a>{/each}</div>
					{#if c.diff.length}
						<table class="t" style="margin-top:4px" data-testid="diff">
							<tbody>
								{#each c.diff as d, i (i)}
									{#if d.field === 'note'}
										<tr><td class="mono">{d.target.key}</td><td colspan="4">新增{(d.after as { is_private?: boolean })?.is_private ? '私有 ' : ' '}note（內容請打開看）</td></tr>
									{:else}
										<tr><td class="mono">{d.target.key}</td><td>{d.field.startsWith('cf:') ? d.label : fieldLabel(d.field)}</td><td class="muted">{fmtValue(d.before)}</td><td>→</td><td><b>{fmtValue(d.after)}</b></td></tr>
									{/if}
								{/each}
							</tbody>
						</table>
					{/if}
					{#if !canTick && c.state === 'proposed' && !r}<div class="hint" style="margin-top:4px">{whyNotQueue(c)}</div>{/if}
					{#if r}
						<div class="row" style="gap:6px;margin-top:6px" data-testid="row-result">
							{#if r.state === 'ok'}<span class="chip c-ok">已套用</span>{#each r.keys ?? [] as k (k)}<a class="btn sm" href="/issue/{k}">開啟 {k}</a>{/each}
							{:else if r.state === 'failed'}<span class="chip c-bad">失敗</span><span class="hint">{r.text}</span><a class="btn sm" href="/cs/{c.changeset_id}">打開看</a>
							{:else if r.state === 'pending' || r.state === 'sending'}<span class="chip c-warn"><span class="spin"></span>確認中</span><span class="hint">{r.text ?? '送出中'}</span>
							{:else}<span class="chip c-dim">未送出</span>{#if r.text}<span class="hint">{r.text}</span>{/if}{/if}
						</div>
					{/if}
				</div>
			</div>
		</div>
	{/each}
	{#if cursor}<div class="pager"><button class="btn" onclick={() => load(true)} disabled={loading}>還有更多</button></div>{/if}
</div>

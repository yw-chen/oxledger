<script lang="ts">
	// 概念與別名（contract 05 §6.13, §3.9）: a concept groups labels that mean
	// the same thing; search expands a label to the concept's other active
	// labels. Labels are never deleted: set them to staging (not expanded).
	import type { ConceptView, Expansion } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { session } from '$lib/session.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	const admin = $derived(!!session.user?.is_admin);
	let items = $state<ConceptView[]>([]);
	let cursor = $state<string | null>(null);
	let q = $state('');
	let loadError = $state<unknown>(null);
	let loaded = $state(false);
	let selId = $state<string | null>(null);
	const sel = $derived(items.find((c) => c.id === selId) ?? null);

	let error = $state<unknown>(null);
	let conflict = $state(false);
	let busy = $state(false);

	async function load(more = false) {
		loadError = null;
		try {
			const r = await api('GET /admin/concepts', { query: { q: q.trim() || undefined, limit: 50, cursor: more ? (cursor ?? undefined) : undefined } });
			items = more ? [...items, ...r.items] : r.items;
			cursor = r.next_cursor;
			loaded = true;
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		if (admin) load();
	});
	let qTimer: ReturnType<typeof setTimeout> | null = null;
	function onQuery() {
		if (qTimer) clearTimeout(qTimer);
		qTimer = setTimeout(() => load(), 250);
	}

	function replace(c: ConceptView) {
		const i = items.findIndex((x) => x.id === c.id);
		items = i < 0 ? [c, ...items] : items.map((x) => (x.id === c.id ? c : x));
		conflict = false;
	}
	async function act(fn: () => Promise<ConceptView>, done: string) {
		busy = true;
		error = null;
		try {
			replace(await fn());
			toast(done, 'ok');
			return true;
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
			else error = e;
			return false;
		} finally {
			busy = false;
		}
	}
	async function reloadSel() {
		if (!selId) return;
		try {
			replace(await api('GET /admin/concepts/{id}', { path: { id: selId } }));
		} catch (e) {
			error = e;
		}
	}

	// ---- new concept ----
	let nc = $state({ pref: '', alts: '' });
	async function create() {
		const labels = nc.alts
			.split(/[,，、\n]/)
			.map((s) => s.trim())
			.filter(Boolean)
			.map((label) => ({ label }));
		busy = true;
		error = null;
		try {
			const c = await api('POST /admin/concepts', { operationId: newOperationId(), body: { pref_label: nc.pref.trim(), labels } });
			replace(c);
			selId = c.id;
			nc = { pref: '', alts: '' };
			toast(`已建立概念「${c.pref_label}」`, 'ok');
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}

	// ---- selected concept ----
	let newLabel = $state('');
	let newHidden = $state(false);
	const addLabel = async () => {
		if (!sel) return;
		const c = sel;
		const label = newLabel.trim();
		const ok = await act(() => api('POST /admin/concepts/{id}/labels', { path: { id: c.id }, operationId: newOperationId(), body: { label, kind: newHidden ? 'hidden' : 'alt' } }), `已加入標籤「${label}」`);
		if (ok) {
			newLabel = '';
			newHidden = false;
		}
	};
	const setLabel = (label: string, body: { kind?: string; status?: string }, done: string) => {
		const c = sel!;
		return act(() => api('PATCH /admin/concepts/{id}/labels/{label}', { path: { id: c.id, label }, operationId: newOperationId(), body }), done);
	};
	function rename() {
		const c = sel!;
		const p = prompt('概念的主要標籤', c.pref_label)?.trim();
		if (p && p !== c.pref_label) act(() => api('PATCH /admin/concepts/{id}', { path: { id: c.id }, operationId: newOperationId(), body: { lock_version: c.lock_version, pref_label: p } }), `已改名為「${p}」`);
	}
	const setStatus = (status: string) => {
		const c = sel!;
		return act(() => api('PATCH /admin/concepts/{id}', { path: { id: c.id }, operationId: newOperationId(), body: { lock_version: c.lock_version, status } }), status === 'retired' ? '已停用這個概念（不再展開）' : '已重新啟用這個概念');
	};
	/** `concept_label_taken`: the other concept, to jump to. */
	const taken = $derived(error instanceof ApiError && error.code === 'concept_label_taken' ? ((error.body?.details ?? {}) as { label?: string; concept_id?: string; pref_label?: string }) : null);
	async function goTaken() {
		if (!taken?.concept_id) return;
		const id = taken.concept_id;
		error = null;
		if (!items.some((c) => c.id === id)) {
			try {
				replace(await api('GET /admin/concepts/{id}', { path: { id } }));
			} catch (e) {
				error = e;
				return;
			}
		}
		selId = id;
	}

	// ---- try a query (§6.13) ----
	let probe = $state('');
	let probing = $state(false);
	let probeResult = $state<{ expanded: Expansion[]; hits: number } | null>(null);
	let probeError = $state<unknown>(null);
	/** What the configuration says the word expands to (active concepts, active labels). */
	const configured = $derived.by(() => {
		const w = probe.trim().toLowerCase();
		if (!w) return [] as { via: string; labels: string[] }[];
		return items
			.filter((c) => c.status === 'active' && c.labels.some((l) => l.status === 'active' && l.label.toLowerCase() === w))
			.map((c) => ({ via: c.pref_label, labels: c.labels.filter((l) => l.status === 'active' && l.label.toLowerCase() !== w).map((l) => (l.kind === 'hidden' ? `${l.label}（隱藏）` : l.label)) }));
	});
	async function runProbe() {
		const text = probe.trim();
		if (!text) return;
		probing = true;
		probeError = null;
		try {
			const r = await api('POST /issues/search', { body: { q: text, limit: 5 } });
			const seen = new Set<string>();
			const expanded: Expansion[] = [];
			for (const it of r.items)
				for (const x of it.match?.expanded ?? []) {
					const k = `${x.from}\u0000${x.label}`;
					if (!seen.has(k)) seen.add(k) && expanded.push(x);
				}
			probeResult = { expanded, hits: r.items.length };
		} catch (e) {
			probeError = e;
		} finally {
			probing = false;
		}
	}
	const KIND: Record<string, string> = { pref: '主要', alt: '別名', hidden: '隱藏（錯字）' };
</script>

<svelte:head><title>概念與別名・oxledger</title></svelte:head>
<div class="page">
	<h1>概念與別名</h1>
	{#if !admin}
		<div class="empty" data-testid="cc-denied">這一頁只有系統管理員可以使用。</div>
	{:else}
		<p class="hint">搜尋時，查詢裡等於某個標籤的詞，也會用同一個概念的其他標籤一起找。例如「登入」也找「Authentication」。</p>
		<ErrorBox error={loadError} onaction={() => load()} />
		<div class="cc">
			<div>
				<input bind:value={q} oninput={onQuery} placeholder="找概念或標籤" style="width:100%;margin-bottom:6px" data-testid="cc-q" />
				<div class="card list" data-testid="cc-list">
					{#each items as c (c.id)}
						<button class="it" class:on={c.id === selId} onclick={() => ((selId = c.id), (error = null), (conflict = false))} data-testid="cc-item">
							<b>{c.pref_label}</b>
							<span class="dim">{c.labels.length} 個標籤</span>
							{#if c.status !== 'active'}<span class="chip c-dim">已停用</span>{/if}
						</button>
					{:else}
						{#if loaded}<div class="hint" style="padding:8px">{q.trim() ? '沒有符合的概念。' : '還沒有概念。'}</div>{/if}
					{/each}
					{#if cursor}<button class="btn sm ghost" onclick={() => load(true)}>載入更多</button>{/if}
				</div>
				<form class="card" onsubmit={(e) => (e.preventDefault(), create())} data-testid="cc-new">
					<h3>新增概念</h3>
					<input bind:value={nc.pref} placeholder="主要標籤（例：Authentication）" style="width:100%;margin-bottom:4px" data-testid="cc-new-pref" />
					<input bind:value={nc.alts} placeholder="別名，用逗號分開（例：登入, 認證）" style="width:100%;margin-bottom:6px" data-testid="cc-new-alts" />
					<button class="btn pri" disabled={busy || !nc.pref.trim()} data-testid="cc-new-submit">建立</button>
				</form>
			</div>
			<div>
				{#if conflict}<div class="banner w">這個概念剛被別人改過，尚未套用。<span class="sp"></span><button class="btn sm" onclick={reloadSel}>載入最新內容</button></div>{/if}
				{#if taken}
					<div class="banner b" data-testid="cc-taken">
						「{taken.label}」已經是「{taken.pref_label}」的標籤
						<span class="sp"></span><button class="btn sm" onclick={goTaken} data-testid="cc-taken-go">前往</button>
					</div>
				{:else}
					<ErrorBox {error} />
				{/if}
				{#if sel}
					<div class="card" data-testid="cc-detail">
						<div class="row" style="gap:8px">
							<h3 style="margin:0" data-testid="cc-pref">{sel.pref_label}</h3>
							{#if sel.status !== 'active'}<span class="chip c-dim">已停用（不展開）</span>{/if}
							<span style="flex:1"></span>
							<button class="btn sm" onclick={rename} disabled={busy}>改名</button>
							<button class="btn sm" onclick={() => setStatus(sel!.status === 'active' ? 'retired' : 'active')} disabled={busy} data-testid="cc-status">{sel.status === 'active' ? '停用概念' : '重新啟用'}</button>
						</div>
						<div class="labels">
							{#each sel.labels as l (l.label)}
								<div class="lab" class:staging={l.status !== 'active'} data-testid="cc-label">
									<span class="nm">{l.label}</span>
									<span class="chip {l.kind === 'hidden' ? 'c-warn' : l.kind === 'pref' ? 'c-act' : 'c-dim'}">{KIND[l.kind] ?? l.kind}</span>
									{#if l.status !== 'active'}<span class="chip c-dim">暫存（不展開）</span>{/if}
									{#if l.kind !== 'pref'}
										<span style="flex:1"></span>
										<button class="btn sm ghost" onclick={() => setLabel(l.label, { kind: l.kind === 'hidden' ? 'alt' : 'hidden' }, l.kind === 'hidden' ? `「${l.label}」改為一般別名` : `「${l.label}」設為隱藏標籤`)} disabled={busy} data-testid="cc-label-hidden">{l.kind === 'hidden' ? '改為別名' : '設為隱藏（錯字）'}</button>
										<button class="btn sm ghost" onclick={() => setLabel(l.label, { status: l.status === 'active' ? 'staging' : 'active' }, l.status === 'active' ? `「${l.label}」改為暫存` : `「${l.label}」已啟用`)} disabled={busy} data-testid="cc-label-status">{l.status === 'active' ? '改為暫存' : '啟用'}</button>
									{/if}
								</div>
							{/each}
						</div>
						<form class="inline-form" onsubmit={(e) => (e.preventDefault(), addLabel())}>
							<input bind:value={newLabel} placeholder="新標籤" data-testid="cc-label-new" />
							<label><input type="checkbox" bind:checked={newHidden} /> 隱藏（錯字或內部用語，只有管理員看得到）</label>
							<button class="btn sm" disabled={busy || !newLabel.trim()} data-testid="cc-label-add">加入</button>
						</form>
					</div>
				{:else}
					<div class="empty">選一個概念來編輯標籤。</div>
				{/if}

				<div class="card" data-testid="cc-probe">
					<h3>試查</h3>
					<form class="inline-form" onsubmit={(e) => (e.preventDefault(), runProbe())}>
						<input bind:value={probe} placeholder="輸入查詢詞" data-testid="cc-probe-q" />
						<button class="btn sm" disabled={probing || !probe.trim()} data-testid="cc-probe-run">試查</button>
					</form>
					{#if configured.length}
						<div style="margin-top:6px" data-testid="cc-probe-config">
							{#each configured as c (c.via)}<div>依設定會展開成：<b>{c.labels.join('、') || '（沒有其他標籤）'}</b>（概念「{c.via}」）</div>{/each}
						</div>
					{:else if probe.trim()}
						<div class="hint" style="margin-top:6px">目前列出的概念裡，沒有標籤等於「{probe.trim()}」。</div>
					{/if}
					<ErrorBox error={probeError} />
					{#if probeResult}
						<div style="margin-top:6px" data-testid="cc-probe-result">
							{#if probeResult.expanded.length}
								前 {probeResult.hits} 筆結果實際用到的展開：
								<ul>{#each probeResult.expanded as x (x.from + x.label)}<li>「{x.from}」→「{x.label}」（概念「{x.via}」）</li>{/each}</ul>
							{:else}
								<span class="hint">前 {probeResult.hits} 筆結果沒有用到展開的詞{probeResult.hits ? '' : '（沒有結果）'}。</span>
							{/if}
						</div>
					{/if}
				</div>
			</div>
		</div>
	{/if}
</div>

<style>
	.cc {
		display: grid;
		grid-template-columns: minmax(220px, 300px) minmax(0, 1fr);
		gap: 14px;
		align-items: start;
	}
	.list {
		padding: 4px;
		max-height: 60vh;
		overflow: auto;
	}
	.it {
		display: flex;
		gap: 8px;
		align-items: center;
		width: 100%;
		text-align: left;
		background: none;
		border: 0;
		border-radius: 6px;
		padding: 6px 8px;
		color: inherit;
		cursor: pointer;
	}
	.it.on,
	.it:hover {
		background: var(--act-a);
	}
	.labels {
		margin: 8px 0;
	}
	.lab {
		display: flex;
		gap: 6px;
		align-items: center;
		padding: 4px 0;
		border-bottom: 1px solid var(--line);
	}
	.lab.staging .nm {
		color: var(--dim);
	}
	@media (max-width: 760px) {
		.cc {
			grid-template-columns: 1fr;
		}
	}
</style>

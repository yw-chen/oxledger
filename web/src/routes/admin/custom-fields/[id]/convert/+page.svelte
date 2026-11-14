<script lang="ts">
	// 換型別精靈（contract 05 §6.11, §3.7）: ① new type and name → ② mapping
	// → ③ preview (creates the new field) → ④ start (step-up) and progress →
	// ⑤ offer to deactivate the old field. Nothing is copied before ④.
	import { page } from '$app/state';
	import type { ConversionPreview, ConversionView, FieldAdminView } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import { PARSE_HINT, REASON, convertTargets, ftypeLabel, mappingKind } from '#lib/fieldadmin.js';
	import { stepped } from '#lib/stepup.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	const admin = $derived(!!session.user?.is_admin);
	const id = $derived(page.params.id ?? '');

	let from = $state<FieldAdminView | null>(null);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	let step = $state(1);

	let to = $state({ ftype: '', name: '', options: '' });
	/** list → list: old option id → new label; text → list: rows of text → label. */
	let optMap = $state<Record<string, string>>({});
	let valueRows = $state<{ text: string; label: string }[]>([{ text: '', label: '' }]);

	let preview = $state<ConversionPreview | null>(null);
	let conv = $state<ConversionView | null>(null);

	async function load() {
		loadError = null;
		try {
			from = (await api('GET /admin/custom-fields/{id}', { path: { id } })).field;
			const t = convertTargets(from.ftype);
			to = { ftype: t[0] ?? '', name: `${from.name}（新）`, options: from.ftype === 'list' ? from.options.filter((o) => o.active).map((o) => o.label).join('\n') : '' };
			// Deactivated options start unmapped (the new field only gets the active ones).
			optMap = Object.fromEntries(from.options.map((o) => [o.id, o.active ? o.label : '']));
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		if (admin && id) load();
	});

	const targets = $derived(from ? convertTargets(from.ftype) : []);
	const kind = $derived(from ? mappingKind(from.ftype, to.ftype) : 'none');
	const newLabels = $derived(
		to.options
			.split('\n')
			.map((s) => s.trim())
			.filter(Boolean)
	);
	const labelOk = (l: string) => newLabels.some((x) => x.toLowerCase() === l.trim().toLowerCase());
	const mappingProblem = $derived.by(() => {
		if (to.ftype === 'list' && !newLabels.length) return '新欄位至少要有一個選項';
		if (kind === 'options') for (const l of Object.values(optMap)) if (l.trim() && !labelOk(l)) return `「${l}」不是新欄位的選項`;
		if (kind === 'values') for (const r of valueRows) if (r.text.trim() && !labelOk(r.label)) return `「${r.label || '（空）'}」不是新欄位的選項`;
		return '';
	});
	function mapping(): Record<string, unknown> {
		if (kind === 'options') return { options: Object.fromEntries(Object.entries(optMap).filter(([, l]) => l.trim()).map(([k, l]) => [k, l.trim()])) };
		if (kind === 'values') return { values: Object.fromEntries(valueRows.filter((r) => r.text.trim()).map((r) => [r.text.trim(), r.label.trim()])) };
		return {};
	}

	async function runPreview() {
		if (!from) return;
		busy = true;
		error = null;
		try {
			preview = await api('POST /admin/custom-fields/{id}/conversions', {
				path: { id },
				operationId: newOperationId(),
				body: {
					to: { name: to.name.trim(), ftype: to.ftype, tracker_ids: from.tracker_ids, is_for_all: from.is_for_all, project_ids: from.project_ids, visible: from.visible, role_ids: from.role_ids, description: from.description || null, ...(to.ftype === 'list' ? { options: newLabels.map((label) => ({ label })) } : {}) },
					mapping: mapping()
				}
			});
			step = 3;
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}

	let timer: ReturnType<typeof setTimeout> | null = null;
	async function poll() {
		if (!conv) return;
		try {
			conv = await api('GET /admin/conversions/{id}', { path: { id: conv.id } });
		} catch (e) {
			error = e;
			return;
		}
		if (conv.state === 'running' || conv.state === 'previewed') timer = setTimeout(poll, 1000);
		else step = 5;
	}
	$effect(() => () => {
		if (timer) clearTimeout(timer);
	});
	async function start() {
		if (!preview) return;
		busy = true;
		error = null;
		try {
			const p = preview;
			const r = await stepped(() => api('POST /admin/conversions/{id}/start', { path: { id: p.conversion_id }, operationId: newOperationId(), body: {} as never }), '開始轉換需要確認身分');
			if (r) {
				conv = r;
				step = 4;
				poll();
			}
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	const report = $derived((conv?.report ?? null) as { converted?: number; failed?: number; skipped?: number; failures?: { key: string; reason: string }[] } | null);

	async function deactivateOld() {
		if (!from) return;
		busy = true;
		error = null;
		try {
			const cur = (await api('GET /admin/custom-fields/{id}', { path: { id } })).field;
			await api('PATCH /admin/custom-fields/{id}', { path: { id }, operationId: newOperationId(), body: { lock_version: cur.lock_version, set: { deactivated: true } } });
			toast(`已停用舊欄位「${from.name}」`, 'ok');
			from = { ...cur, deactivated: true };
		} catch (e) {
			error = e instanceof ApiError && e.code === 'conflict' ? new Error('舊欄位剛被改過，請到欄位頁確認後再停用') : e;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>換型別・oxledger</title></svelte:head>
<div class="page">
	<div class="hint"><a href="/admin/custom-fields/{id}">← 回到欄位</a></div>
	<h1>換型別{from ? `：${from.name}` : ''}</h1>
	{#if !admin}
		<div class="empty" data-testid="cf-denied">這一頁只有系統管理員可以使用。</div>
	{:else}
		<ErrorBox error={loadError} onaction={load} />
		{#if from}
			<ol class="steps" data-testid="cv-steps">
				{#each ['新型別', '對照', '預檢', '轉換', '完成'] as s, i (s)}<li class:on={step === i + 1} class:done={step > i + 1}>{s}</li>{/each}
			</ol>
			<ErrorBox {error} />
			{#if !targets.length}
				<div class="empty">「{ftypeLabel(from.ftype)}」型別的欄位沒有可以轉換的目標型別。</div>
			{:else if step === 1}
				<div class="card" data-testid="cv-step1">
					<p class="hint">目前是「{ftypeLabel(from.ftype)}」。會建立一個新欄位，適用範圍與可見性照舊欄位；確認後才會複製值。</p>
					<label class="f"
						>新型別
						<select bind:value={to.ftype} data-testid="cv-type">
							{#each targets as t (t)}<option value={t}>{ftypeLabel(t)}</option>{/each}
						</select>
					</label>
					<label class="f">新欄位名稱<input bind:value={to.name} data-testid="cv-name" /></label>
					<button class="btn pri" onclick={() => (step = 2)} disabled={!to.name.trim() || !to.ftype} data-testid="cv-next1">下一步</button>
				</div>
			{:else if step === 2}
				<div class="card" data-testid="cv-step2">
					{#if to.ftype === 'list'}
						<label class="f">新欄位的選項（一行一個）<textarea rows="4" bind:value={to.options} data-testid="cv-options"></textarea></label>
					{/if}
					{#if kind === 'options'}
						<h3>選項對照</h3>
						<p class="hint">每個舊選項要轉成哪個新選項；留白的舊選項轉不過去（列為失敗）。</p>
						<table class="t">
							<thead><tr><th>舊選項</th><th>新選項</th></tr></thead>
							<tbody>
								{#each from.options as o (o.id)}
									<tr>
										<td>{o.label}{o.active ? '' : '（已停用）'}</td>
										<td>
											<select bind:value={optMap[o.id]} data-testid="cv-map">
												<option value="">（不轉）</option>
												{#each newLabels as l (l)}<option value={l}>{l}</option>{/each}
												{#if optMap[o.id] && !labelOk(optMap[o.id])}<option value={optMap[o.id]}>{optMap[o.id]}（不在新選項）</option>{/if}
											</select>
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					{:else if kind === 'values'}
						<h3>文字對照</h3>
						<p class="hint">舊值（去掉前後空白後完全相同）要轉成哪個新選項；沒列到的值會列為失敗。</p>
						{#each valueRows as r, i (i)}
							<div class="inline-form" style="margin-bottom:4px">
								<input bind:value={r.text} placeholder="舊的文字" data-testid="cv-val-text" />
								→
								<select bind:value={r.label} data-testid="cv-val-label">
									<option value="">（選擇）</option>
									{#each newLabels as l (l)}<option value={l}>{l}</option>{/each}
								</select>
								<button class="btn sm ghost" onclick={() => (valueRows = valueRows.filter((_, k) => k !== i))} disabled={valueRows.length === 1} aria-label="移除">×</button>
							</div>
						{/each}
						<button class="btn sm" onclick={() => (valueRows = [...valueRows, { text: '', label: '' }])}>＋ 再一列</button>
					{:else if PARSE_HINT[to.ftype]}
						<p data-testid="cv-parse">解析規則：{PARSE_HINT[to.ftype]}</p>
					{:else}
						<p>值會照原樣轉成文字{from.ftype === 'list' ? '（清單用選項名稱）' : ''}；超過長度上限的會列為失敗。</p>
					{/if}
					{#if mappingProblem}<div class="err">{mappingProblem}</div>{/if}
					<div class="row" style="gap:8px;margin-top:10px">
						<button class="btn" onclick={() => (step = 1)}>上一步</button>
						<button class="btn pri" onclick={runPreview} disabled={busy || !!mappingProblem} data-testid="cv-preview">{busy ? '預檢中…' : '預檢'}</button>
					</div>
					<p class="hint">預檢會先建立新欄位「{to.name}」（還不會有值）；之後不轉換的話，請到欄位清單刪除它。</p>
				</div>
			{:else if step === 3 && preview}
				<div class="card" data-testid="cv-step3">
					<p>可以轉換 <b data-testid="cv-ok">{preview.convertible}</b> 張；轉不過去 <b data-testid="cv-fail">{preview.failures.count}</b> 張{preview.failures.sample_issue_keys.length ? `（例：${preview.failures.sample_issue_keys.join('、')}）` : ''}。</p>
					<p class="hint">轉不過去的單會保留舊欄位的值，新欄位留空。新欄位已建立：<a href="/admin/custom-fields/{preview.to_field_id}">cf:{preview.to_field_id}</a>。</p>
					<div class="row" style="gap:8px">
						<button class="btn pri" onclick={start} disabled={busy} data-testid="cv-start">開始轉換</button>
						<a class="btn" href="/admin/custom-fields/{preview.to_field_id}">先不轉換（到新欄位頁刪除）</a>
					</div>
				</div>
			{:else if step === 4 && conv}
				<div class="card" data-testid="cv-step4">
					<div class="sync"><span class="spin"></span>轉換中…已完成 {conv.batches_done} 批</div>
				</div>
			{:else if step === 5 && conv}
				<div class="card" data-testid="cv-step5">
					{#if conv.state === 'succeeded'}
						<p data-testid="cv-done">轉換完成：{report?.converted ?? 0} 張已轉換{report?.failed ? `，${report.failed} 張失敗` : ''}{report?.skipped ? `，${report.skipped} 張略過` : ''}。</p>
					{:else}
						<p class="err">轉換結束，狀態：{conv.state}。</p>
					{/if}
					{#if report?.failures?.length}
						<table class="t">
							<thead><tr><th>議題</th><th>原因</th></tr></thead>
							<tbody>{#each report.failures as f (f.key)}<tr><td><a href="/issue/{f.key}">{f.key}</a></td><td>{REASON[f.reason] ?? f.reason}</td></tr>{/each}</tbody>
						</table>
					{/if}
					{#if from.deactivated}
						<p>舊欄位「{from.name}」已停用。</p>
					{:else}
						<p>要停用舊欄位「{from.name}」嗎？停用後新單不會出現它，既有的值保留。</p>
						<button class="btn pri" onclick={deactivateOld} disabled={busy} data-testid="cv-deactivate">停用舊欄位</button>
					{/if}
					<a class="btn" href="/admin/custom-fields/{conv.to_field_id}">前往新欄位</a>
				</div>
			{/if}
		{/if}
	{/if}
</div>

<style>
	.steps {
		display: flex;
		gap: 6px;
		list-style: none;
		padding: 0;
		margin: 0 0 12px;
		counter-reset: s;
	}
	.steps li {
		counter-increment: s;
		padding: 3px 10px;
		border: 1px solid var(--line);
		border-radius: 99px;
		font-size: 12.5px;
		color: var(--dim);
	}
	.steps li::before {
		content: counter(s) ' ';
	}
	.steps li.on {
		border-color: var(--act);
		color: var(--fg);
		font-weight: 600;
	}
	.steps li.done {
		color: var(--ok);
	}
	.f {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin-bottom: 8px;
	}
	.err {
		color: var(--bad);
		font-size: 12.5px;
	}
</style>

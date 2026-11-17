<script lang="ts">
	// 匯入精靈 (design 06 §6.12, R-I1–R-I15): upload a CSV file, say how to
	// read it, map its columns and values, try it (nothing is written), then
	// run it. The import lives on the server: leaving this page stops
	// nothing, and `?id=` comes back to it.
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import type { ImportDetail, ImportList, ImportRow, ImportStatus, ImportCloseResult } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { downloadFile, saveBlob } from '#lib/exports.js';
	import {
		DATE_FORMATS,
		DELIMITERS,
		ENCODINGS,
		KIND_LABEL,
		STATE_LABEL,
		STEPS,
		fieldLabel,
		importFileProblem,
		mappingProblem,
		progress,
		rowErrorText,
		stepOf,
		templateCsv,
		uploadImport,
		working,
		type ImportKind
	} from '#lib/imports.js';
	import { toast } from '#lib/toast.svelte.js';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	const wanted = $derived(page.url.searchParams.get('id'));
	let kind = $state<ImportKind>(page.url.searchParams.get('kind') === 'time_entries' ? 'time_entries' : 'issues');

	let step = $state(0);
	/** The import with what the mapping steps need (while its file is kept). */
	let detail = $state<ImportDetail | null>(null);
	/** State and progress; polled while the server works. */
	let status = $state<ImportStatus | null>(null);
	let mine = $state<ImportList | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);

	// What the person is editing (saved with PUT settings).
	let encoding = $state('utf-8');
	let delimiter = $state(',');
	let dateFormat = $state('ymd');
	let mapping = $state<string[]>([]);
	let valueMap = $state<Record<string, Record<string, string>>>({});
	let skipTriage = $state(false);
	let notify = $state(false);
	let stripQuote = $state(true);
	let defaultTracker = $state('');
	let mapProblem = $state<string | null>(null);
	/** The settings as the server holds them (to skip a save that changes nothing). */
	let saved = '';
	const current = () => JSON.stringify([dateFormat, mapping, valueMap, skipTriage, notify, stripQuote, defaultTracker]);

	// Step 1.
	let fileProblem = $state<string | null>(null);
	let sending = $state<number | null>(null);
	let over = $state(false);

	// Steps 5 and 6.
	let errorRows = $state<ImportRow[]>([]);
	let errorCursor = $state<string | null>(null);
	let okRows = $state<ImportRow[]>([]);
	let codeFilter = $state('');
	let confirming = $state(false);
	let runOp = newOperationId();
	let undoing = $state(false);
	let undoStatus = $state('');
	let undone = $state<ImportCloseResult | null>(null);

	const id = $derived(status?.id ?? null);
	const fields = $derived(detail?.fields ?? []);
	const label = (k: string | null | undefined) => fieldLabel(fields, k);
	const required = $derived(kind === 'issues' ? 'subject' : 'hours');
	const requiredMapped = $derived(mapping.includes(required));
	const terminal = $derived(!!status && ['done', 'canceled', 'failed', 'expired'].includes(status.state));
	const codes = $derived([...new Set(errorRows.map((r) => r.error_code ?? ''))].filter(Boolean));
	const shownErrors = $derived(codeFilter ? errorRows.filter((r) => r.error_code === codeFilter) : errorRows);
	const unresolved = $derived(
		Object.entries(detail?.values ?? {}).reduce((n, [f, vs]) => n + vs.filter((v) => v.problem && valueMap[f]?.[v.value] === undefined).length, 0)
	);

	function adopt(d: ImportDetail) {
		detail = d;
		status = d;
		kind = d.kind as ImportKind;
		encoding = d.encoding;
		delimiter = d.delimiter;
		dateFormat = d.settings.date_format ?? 'ymd';
		mapping = d.header.map((_, i) => d.settings.mapping?.[i] ?? '');
		valueMap = JSON.parse(JSON.stringify(d.settings.value_map ?? {}));
		skipTriage = d.settings.options?.skip_triage ?? false;
		notify = d.settings.options?.notify ?? false;
		stripQuote = d.settings.options?.strip_formula_quote ?? true;
		defaultTracker = d.settings.options?.default_tracker_id ?? '';
		saved = current();
	}
	/** Nothing of another import stays on screen. */
	function clear() {
		errorRows = [];
		errorCursor = null;
		okRows = [];
		codeFilter = '';
		confirming = false;
		undoing = false;
		undoStatus = '';
		undone = null;
		mapProblem = null;
		fileProblem = null;
	}

	async function loadMine() {
		try {
			mine = await api('GET /projects/{id}/imports', { path: { id: key } });
		} catch (e) {
			error = e;
		}
	}

	async function open(importId: string) {
		error = null;
		clear();
		try {
			const s = await api('GET /imports/{id}', { path: { id: importId } });
			status = s;
			kind = s.kind as ImportKind;
			if (s.file_available && !['done', 'canceled', 'failed', 'expired'].includes(s.state)) {
				adopt(await api('GET /imports/{id}/preview', { path: { id: importId } }));
			} else detail = null;
			step = stepOf(s.state);
			if (s.state === 'validated' || step === 5) await loadRows();
		} catch (e) {
			error = e;
		}
	}

	$effect(() => {
		void key;
		const w = wanted;
		if (w) {
			if (w !== status?.id) open(w);
		} else {
			status = null;
			detail = null;
			step = 0;
			clear();
			loadMine();
		}
	});

	// The server works on; ask once a second while it does.
	$effect(() => {
		if (!working(status) || !id) return;
		const importId = id;
		const t = setInterval(async () => {
			try {
				const s = await api('GET /imports/{id}', { path: { id: importId } });
				if (status?.id !== importId) return;
				const was = status.state;
				status = s;
				if (!working(s)) {
					if (detail) detail = { ...detail, ...s };
					if (was === 'running') step = 5;
					await loadRows();
				}
			} catch (e) {
				error = e;
			}
		}, 1000);
		return () => clearInterval(t);
	});

	async function loadRows() {
		if (!id) return;
		const e = await api('GET /imports/{id}/rows', { path: { id }, query: { state: 'error' } });
		errorRows = e.items;
		errorCursor = e.next_cursor ?? null;
		codeFilter = '';
		okRows = status && status.state !== 'validated' && status.state !== 'validating' ? (await api('GET /imports/{id}/rows', { path: { id }, query: { state: 'ok' } })).items : [];
	}
	async function moreErrors() {
		if (!id || !errorCursor) return;
		const e = await api('GET /imports/{id}/rows', { path: { id }, query: { state: 'error', cursor: errorCursor } });
		errorRows = [...errorRows, ...e.items];
		errorCursor = e.next_cursor ?? null;
	}

	// ---------------------------------------------------------------- step 1
	async function send(file: File | null | undefined) {
		if (!file || sending !== null) return;
		fileProblem = null;
		error = null;
		sending = 0;
		clear();
		try {
			const d = await uploadImport(key, kind, file, (p) => (sending = p));
			adopt(d);
			step = 1;
			await goto(`/project/${key}/imports/new?kind=${kind}&id=${d.id}`, { replaceState: true });
		} catch (e) {
			fileProblem = importFileProblem(e);
			if (!fileProblem) error = e;
		} finally {
			sending = null;
		}
	}
	function template() {
		saveBlob(new Blob([templateCsv(kind)], { type: 'text/csv;charset=utf-8' }), kind === 'issues' ? 'issues-template.csv' : 'time-entries-template.csv');
	}

	// ---------------------------------------------------------------- steps 2–4
	/** Saves what is on screen; false when it was refused (the reason is shown). */
	async function save(reread = false): Promise<boolean> {
		if (!detail || !id) return false;
		busy = true;
		error = null;
		mapProblem = null;
		try {
			const d = await api('PUT /imports/{id}/settings', {
				path: { id },
				body: {
					lock_version: detail.lock_version,
					...(reread ? { encoding, delimiter } : {}),
					date_format: dateFormat,
					mapping: mapping.map((m) => m || null),
					value_map: valueMap,
					options: { strip_formula_quote: stripQuote, skip_triage: skipTriage, notify, ...(defaultTracker ? { default_tracker_id: defaultTracker } : {}) }
				}
			});
			adopt(d);
			errorRows = [];
			return true;
		} catch (e) {
			if (e instanceof ApiError && e.code === 'import_mapping_invalid') mapProblem = mappingProblem(e.details ?? {}, label);
			else if (e instanceof ApiError && e.code === 'import_invalid_file') {
				mapProblem = importFileProblem(e);
				// What was chosen cannot read the file: back to what could.
				encoding = detail.encoding;
				delimiter = detail.delimiter;
			} else error = e;
			return false;
		} finally {
			busy = false;
		}
	}
	async function next() {
		// Nothing changed: a dry run made before still counts.
		if (current() === saved || (await save())) step += 1;
	}
	function setValue(field: string, value: string, to: string) {
		const m = { ...(valueMap[field] ?? {}) };
		if (to === '__auto') delete m[value];
		else m[value] = to === '__empty' ? '' : to;
		valueMap = { ...valueMap, [field]: m };
	}
	const chosen = (field: string, value: string, target: string | null | undefined) => {
		const m = valueMap[field]?.[value];
		return m === undefined ? (target ? '__auto' : '') : m === '' ? '__empty' : m;
	};

	// ---------------------------------------------------------------- step 5
	async function validate() {
		if (!id) return;
		if (current() !== saved && !(await save())) return;
		busy = true;
		try {
			status = await api('POST /imports/{id}/validate', { path: { id }, body: {} });
		} catch (e) {
			if (e instanceof ApiError && e.code === 'import_mapping_invalid') mapProblem = mappingProblem(e.details ?? {}, label);
			else error = e;
		} finally {
			busy = false;
		}
	}

	// ---------------------------------------------------------------- step 6
	async function run() {
		if (!id || !status) return;
		confirming = false;
		busy = true;
		error = null;
		try {
			status = await api('POST /imports/{id}/run', { path: { id }, operationId: runOp, body: { lock_version: status.lock_version } });
			runOp = newOperationId();
			step = 5;
		} catch (e) {
			error = e;
			if (!(e instanceof ApiError && e.retryable)) runOp = newOperationId();
		} finally {
			busy = false;
		}
	}
	async function stop() {
		if (!id) return;
		try {
			status = await api('POST /imports/{id}/cancel', { path: { id }, body: {} });
			if (!working(status)) await loadRows();
		} catch (e) {
			error = e;
		}
	}
	async function undo() {
		if (!id) return;
		busy = true;
		error = null;
		try {
			undone = await api('POST /imports/{id}/close-created', { path: { id }, body: kind === 'issues' ? { status_id: undoStatus } : {} });
			undoing = false;
			toast(kind === 'issues' ? `已關閉 ${undone.closed} 張` : `已刪除 ${undone.closed} 筆`, 'ok');
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	async function report() {
		if (!id) return;
		try {
			await downloadFile(`/imports/${id}/report.csv`, 'import-report.csv');
		} catch (e) {
			error = e;
		}
	}
	async function discard(importId: string) {
		try {
			await api('DELETE /imports/{id}', { path: { id: importId } });
			await loadMine();
		} catch (e) {
			error = e;
		}
	}
	const pct = (s: ImportStatus) => `${Math.round(progress(s) * 100)}%`;
	const back = $derived(kind === 'issues' ? `/project/${key}/issues` : `/project/${key}/time`);
</script>

<svelte:head><title>{key}・匯入{KIND_LABEL[kind]}・oxledger</title></svelte:head>
<ProjectFrame projectKey={key}>
	<div class="wiz" data-testid="import-wizard">
		<p><a href={back}>← 回到{kind === 'issues' ? '議題' : '工時'}</a></p>
		<h2>從 CSV 匯入{KIND_LABEL[kind]}</h2>
		<ol class="steps" aria-label="步驟">
			{#each STEPS as s, i (s)}
				<li class:on={i === step} class:done={i < step} aria-current={i === step ? 'step' : undefined}>
					{#if detail && !working(status) && !terminal && i < step && i > 0}
						<button type="button" class="lnk" onclick={() => (step = i)}>{i + 1} {s}</button>
					{:else}{i + 1} {s}{/if}
				</li>
			{/each}
		</ol>
		<ErrorBox {error} />
		{#if status?.state === 'expired'}
			<div class="banner w" data-testid="import-expired">這個匯入已過期（7 天沒有動作），請重新上傳。<span class="sp"></span><a class="btn sm" href="/project/{key}/imports/new?kind={kind}">重新上傳</a></div>
		{:else if status && !detail && step >= 1 && step <= 4 && !working(status)}
			<div class="banner w" data-testid="import-no-file">這個匯入的檔案已經清除，請重新上傳。<span class="sp"></span><a class="btn sm" href="/project/{key}/imports/new?kind={kind}">重新上傳</a></div>
		{:else if step === 0}
			<div
				class="drop"
				class:over
				role="group"
				aria-label="上傳 CSV 檔"
				ondragover={(e) => (e.preventDefault(), (over = true))}
				ondragleave={() => (over = false)}
				ondrop={(e) => (e.preventDefault(), (over = false), send(e.dataTransfer?.files?.[0]))}
				data-testid="import-drop"
			>
				<p>把 CSV 檔拖到這裡，或</p>
				<label class="btn pri">選擇檔案<input type="file" accept=".csv,text/csv,.txt,.tsv" hidden onchange={(e) => (send(e.currentTarget.files?.[0]), (e.currentTarget.value = ''))} data-testid="import-file" /></label>
				<p class="hint">CSV 檔，最大 10 MB、5,000 列；第一列必須是欄位名稱。</p>
				{#if sending !== null}<progress max="1" value={sending} aria-label="上傳進度"></progress>{/if}
			</div>
			{#if fileProblem}<div class="bad" role="alert" data-testid="import-file-problem">{fileProblem}</div>{/if}
			<div class="row">
				<label>匯入的是
					<select bind:value={kind} data-testid="import-kind" disabled={mine !== null && mine.can_import.length < 2}>
						{#if !mine || mine.can_import.includes('issues')}<option value="issues">議題</option>{/if}
						{#if !mine || mine.can_import.includes('time_entries')}<option value="time_entries">工時</option>{/if}
					</select>
				</label>
				<button type="button" class="btn sm" onclick={template} data-testid="import-template">下載範本</button>
			</div>
			{#if mine && !mine.can_import.length}<div class="banner w">你沒有在這個專案匯入的權限。</div>{/if}
			{#if mine?.items.length}
				<h3>我的匯入</h3>
				<table class="tbl" data-testid="import-mine">
					<thead><tr><th>時間</th><th>類型</th><th>狀態</th><th>列數</th><th></th></tr></thead>
					<tbody>
						{#each mine.items as m (m.id)}
							<tr>
								<td>{new Date(m.created_at).toLocaleString('zh-TW')}</td>
								<td>{KIND_LABEL[m.kind as ImportKind] ?? m.kind}</td>
								<td>{STATE_LABEL[m.state] ?? m.state}{#if m.state === 'done' || m.state === 'canceled'}（成功 {m.ok}、失敗 {m.errors}）{/if}</td>
								<td>{m.row_count}</td>
								<td>
									<a href="/project/{key}/imports/new?kind={m.kind}&id={m.id}">開啟</a>
									{#if !working(m) && m.file_available}<button type="button" class="lnk" onclick={() => discard(m.id)}>刪除檔案</button>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			{/if}
		{:else if detail && step >= 1 && step <= 3}
			<div class="narrow banner i">欄位對應請在較寬的螢幕上操作。</div>
			<div class="wide">
				{#if step === 1}
					{#if detail.encoding_guessed}<div class="banner w" data-testid="import-guessed">我們猜這個檔案是 Big5 編碼，請看下面的預覽是否正確。</div>{/if}
					<div class="row">
						<label>編碼 <select bind:value={encoding} onchange={() => save(true)} disabled={busy} data-testid="import-encoding">{#each ENCODINGS as e (e.value)}<option value={e.value}>{e.label}</option>{/each}</select></label>
						<label>分隔符 <select bind:value={delimiter} onchange={() => save(true)} disabled={busy} data-testid="import-delimiter">{#each DELIMITERS as d (d.value)}<option value={d.value}>{d.label}</option>{/each}</select></label>
						<label>日期格式 <select bind:value={dateFormat} data-testid="import-date-format">{#each DATE_FORMATS as d (d.value)}<option value={d.value}>{d.label}</option>{/each}</select></label>
					</div>
					<p class="hint">共 {detail.row_count} 列資料；以下是前 {detail.preview.length} 列。</p>
					<div class="scroll">
						<table class="tbl" data-testid="import-preview">
							<thead><tr>{#each detail.header as h, i (i)}<th>{h}</th>{/each}</tr></thead>
							<tbody>{#each detail.preview as r, i (i)}<tr>{#each r as c, j (j)}<td>{c}</td>{/each}</tr>{/each}</tbody>
						</table>
					</div>
				{:else if step === 2}
					<table class="tbl" data-testid="import-mapping">
						<thead><tr><th>檔案的欄位</th><th>第一列的值</th><th>匯入成</th></tr></thead>
						<tbody>
							{#each detail.header as h, i (i)}
								<tr>
									<td>{h}</td>
									<td class="sample">{detail.preview[0]?.[i] ?? ''}</td>
									<td>
										<select bind:value={mapping[i]} aria-label="{h} 匯入成" data-testid="import-map">
											<option value="">不匯入</option>
											{#each fields as f (f.key)}<option value={f.key} disabled={mapping.includes(f.key) && mapping[i] !== f.key}>{f.label}{f.required ? '（必填）' : ''}</option>{/each}
										</select>
										{#if mapping[i] && detail.suggested[i] === mapping[i]}<span class="tag">自動</span>{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
					{#if kind === 'issues' && detail.trackers.length > 1}
						<label class="row">沒有填追蹤類型的列用
							<select bind:value={defaultTracker} data-testid="import-default-tracker">
								<option value="">（不指定：這些列會失敗）</option>
								{#each detail.trackers as t (t.id)}<option value={t.id}>{t.label}</option>{/each}
							</select>
						</label>
					{/if}
					{#if !requiredMapped}<div class="bad" data-testid="import-need-required">「{label(required)}」是必填，請選一個檔案欄位對應它。</div>{/if}
				{:else}
					{#each Object.entries(detail.values) as [f, vs] (f)}
						<h3>{label(f)}</h3>
						{#if vs.some((v) => v.problem)}
							<p class="hint">{f === 'assigned_to' ? '只能對應到本專案可指派的成員。' : f === 'user' ? '只能對應到本專案能記工時的人。' : '對應不到的值，那一列會匯入失敗；可以指定一個值或留空。'}</p>
						{/if}
						<table class="tbl" data-testid="import-values">
							<thead><tr><th>檔案裡的值</th><th>對應到</th></tr></thead>
							<tbody>
								{#each vs as v (v.value)}
									{@const sel = chosen(f, v.value, v.target)}
									<tr class:miss={sel === ''}>
										<td>{v.value}</td>
										<td>
											<select value={sel} onchange={(e) => setValue(f, v.value, e.currentTarget.value)} aria-label="{v.value} 對應到" data-testid="import-value">
												{#if sel === ''}<option value="" disabled>{v.problem === 'ambiguous_value' ? '有好幾個同名，請選一個' : '對應不到，請選一個'}</option>{/if}
												{#if v.target}<option value="__auto">{detail.choices[f]?.find((c) => c.id === v.target)?.label ?? '自動'}（自動）</option>{/if}
												<option value="__empty">留空</option>
												{#each detail.choices[f] ?? [] as c (c.id)}<option value={c.id}>{c.label}</option>{/each}
											</select>
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
						{#if vs.length >= 200}<p class="hint">只列出前 200 個不同的值。</p>{/if}
					{:else}
						<div class="empty">沒有需要對應的值。</div>
					{/each}
					{#if unresolved}<div class="banner w" data-testid="import-unresolved">還有 {unresolved} 個值對應不到；不處理的話，用到這些值的列會匯入失敗。</div>{/if}
				{/if}
				{#if mapProblem}<div class="bad" role="alert" data-testid="import-map-problem">{mapProblem}</div>{/if}
				<div class="row">
					{#if step > 1}<button class="btn" onclick={() => (step -= 1)} disabled={busy}>上一步</button>{/if}
					<button class="btn pri" onclick={next} disabled={busy || (step === 2 && !requiredMapped)} title={step === 2 && !requiredMapped ? `「${label(required)}」還沒有對應` : ''} data-testid="import-next">{busy ? '處理中…' : '下一步'}</button>
				</div>
			</div>
		{:else if status && step === 4}
			{#if status.state === 'validating'}
				<p>試跑中… {status.done} / {status.row_count}</p>
				<progress max="1" value={progress(status)} aria-label="試跑進度">{pct(status)}</progress>
				<div class="row"><button class="btn" onclick={stop}>停止試跑</button></div>
			{:else}
				{#if detail}
					<fieldset class="opts">
						<legend>選項</legend>
						<p class="hint">出錯的列略過，其他照常匯入。改了選項要重新試跑。</p>
						{#if detail.can_skip_triage}<label><input type="checkbox" bind:checked={skipTriage} onchange={() => save()} disabled={busy} data-testid="import-skip-triage" /> 直接接受，不進分流</label>{/if}
						{#if kind === 'issues'}<label><input type="checkbox" bind:checked={notify} onchange={() => save()} disabled={busy} data-testid="import-notify" /> 通知被指派的人</label>{/if}
						<label><input type="checkbox" bind:checked={stripQuote} onchange={() => save()} disabled={busy} /> 去掉匯出時為了防公式加上的開頭「'」</label>
					</fieldset>
				{/if}
				{#if mapProblem}<div class="bad" role="alert">{mapProblem}</div>{/if}
				{#if status.state === 'validated'}
					<div class="banner {status.errors ? 'w' : 'i'}" data-testid="import-dry-result">
						可以建立 {status.ok.toLocaleString('en-US')} {kind === 'issues' ? '張' : '筆'}{#if status.errors}，有 {status.errors.toLocaleString('en-US')} 列有問題{/if}。
					</div>
				{/if}
				<div class="row">
					{#if detail}<button class="btn" onclick={() => (step = 3)} disabled={busy}>上一步</button>{/if}
					<button class="btn {status.state === 'validated' ? '' : 'pri'}" onclick={validate} disabled={busy || !detail} data-testid="import-validate">{status.state === 'validated' ? '重新試跑' : '開始試跑'}</button>
					{#if status.state === 'validated'}
						<button class="btn pri" onclick={() => (confirming = true)} disabled={busy || !status.ok || confirming} data-testid="import-run">開始匯入</button>
						{#if status.errors}<button class="btn" onclick={report} data-testid="import-report">下載問題清單</button>{/if}
					{/if}
				</div>
				{#if confirming}
					<div class="banner i" data-testid="import-confirm">
						將建立最多 {status.ok.toLocaleString('en-US')} {kind === 'issues' ? '張議題' : '筆工時'}。執行中可以停止，已建立的不會被刪除。
						<span class="sp"></span>
						<button class="btn sm pri" onclick={run} disabled={busy} data-testid="import-confirm-run">開始匯入</button>
						<button class="btn sm" onclick={() => (confirming = false)}>取消</button>
					</div>
				{/if}
			{/if}
		{:else if status && step === 5}
			{#if status.state === 'running'}
				<p>匯入中… {status.done} / {status.row_count}（成功 {status.ok}、失敗 {status.errors}）</p>
				<progress max="1" value={progress(status)} aria-label="匯入進度">{pct(status)}</progress>
				<div class="row"><button class="btn" onclick={stop} data-testid="import-stop">停止</button></div>
				<p class="hint">可以離開這一頁，匯入會繼續；之後從「我的匯入」回來看結果。</p>
			{:else}
				<div class="banner {status.state === 'done' && !status.errors ? 'i' : 'w'}" data-testid="import-result">
					{STATE_LABEL[status.state] ?? status.state}：成功 {status.ok}、失敗 {status.errors}、略過 {status.skipped}。
					{#if status.state === 'failed'}匯入無法繼續（專案或你的權限可能已變更）。{/if}
				</div>
				<div class="row">
					<button class="btn" onclick={report} data-testid="import-report">下載結果</button>
					{#if status.ok > 0 && !undone}<button class="btn" onclick={() => (undoing = true)} data-testid="import-undo">撤銷這次匯入</button>{/if}
					<a class="btn" href="/project/{key}/imports/new?kind={kind}">再匯入一個檔案</a>
				</div>
				{#if undoing}
					<div class="card" data-testid="import-undo-dialog">
						{#if kind === 'issues'}
							<p>把這次匯入建立、之後沒有人動過的議題改成：</p>
							<select bind:value={undoStatus} aria-label="關閉狀態" data-testid="import-undo-status">
								<option value="">（選一個結案狀態）</option>
								{#each status.undo_statuses as s (s.id)}<option value={s.id}>{s.label}</option>{/each}
							</select>
						{:else}
							<p>刪除這次匯入記下的工時。</p>
						{/if}
						<div class="row">
							<button class="btn pri" onclick={undo} disabled={busy || (kind === 'issues' && !undoStatus)} data-testid="import-undo-run">撤銷</button>
							<button class="btn" onclick={() => (undoing = false)}>取消</button>
						</div>
					</div>
				{/if}
				{#if undone}
					<div class="banner i" data-testid="import-undone">
						已{kind === 'issues' ? '關閉' : '刪除'} {undone.closed}{#if undone.already}，先前已處理 {undone.already}{/if}{#if undone.failed.length}，無法撤銷 {undone.failed.length}{/if}。
					</div>
					{#if undone.failed.length}
						<table class="tbl" data-testid="import-not-undone">
							<thead><tr><th>列</th><th>單號</th><th>原因</th></tr></thead>
							<tbody>
								{#each undone.failed as f (f.row_no)}
									<tr><td>{f.row_no}</td><td>{#if f.issue_key}<a href="/issue/{f.issue_key}">{f.issue_key}</a>{/if}</td><td>{f.reason === 'changed_since' ? '匯入之後有人改過' : rowErrorText(f.reason)}</td></tr>
								{/each}
							</tbody>
						</table>
					{/if}
				{/if}
				{#if okRows.some((r) => r.issue_key)}
					<h3>建立的議題</h3>
					<p class="keys" data-testid="import-created">
						{#each okRows.filter((r) => r.issue_key) as r (r.row_no)}<a href="/issue/{r.issue_key}">{r.issue_key}</a>{' '}{/each}
						{#if status.ok > okRows.length}<span class="hint">…只列出前 {okRows.length} 張；全部請下載結果。</span>{/if}
					</p>
				{/if}
			{/if}
		{/if}
		{#if status && (step === 4 || step === 5) && !working(status) && errorRows.length}
			<h3>有問題的列</h3>
			{#if codes.length > 1}
				<label>錯誤類型 <select bind:value={codeFilter} data-testid="import-code-filter"><option value="">全部</option>{#each codes as c (c)}<option value={c}>{rowErrorText(c)}</option>{/each}</select></label>
			{/if}
			<table class="tbl" data-testid="import-errors">
				<thead><tr><th>列</th><th>欄位</th><th>問題</th></tr></thead>
				<tbody>
					{#each shownErrors as r (r.row_no)}
						<tr><td>{r.row_no}</td><td>{label(r.error_field)}</td><td>{rowErrorText(r.error_code)}</td></tr>
					{/each}
				</tbody>
			</table>
			{#if errorCursor}<button class="btn sm" onclick={moreErrors}>載入更多</button>{/if}
			<p class="hint">列號是資料列（不含第一列的欄位名稱）。</p>
		{/if}
	</div>
</ProjectFrame>

<style>
	.wiz {
		max-width: 900px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 12px;
	}
	.steps {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		list-style: none;
		margin: 0;
		padding: 0;
		font-size: 13px;
		color: var(--mute, #667085);
	}
	.steps li.on {
		font-weight: 600;
		color: inherit;
	}
	.steps li.done {
		color: inherit;
	}
	.lnk {
		background: none;
		border: 0;
		padding: 0;
		color: var(--link, #175cd3);
		cursor: pointer;
		font: inherit;
	}
	.drop {
		border: 2px dashed var(--line2);
		border-radius: 8px;
		padding: 24px;
		text-align: center;
	}
	.drop.over {
		border-color: var(--link, #175cd3);
	}
	.row {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
	}
	.sp {
		flex: 1;
	}
	.scroll {
		overflow: auto;
		max-height: 360px;
	}
	.tbl {
		border-collapse: collapse;
		font-size: 13px;
	}
	.tbl th,
	.tbl td {
		border: 1px solid var(--line2);
		padding: 3px 8px;
		text-align: left;
		vertical-align: top;
		white-space: pre-wrap;
		max-width: 320px;
		overflow-wrap: anywhere;
	}
	.sample {
		color: var(--mute, #667085);
	}
	.tag {
		font-size: 11px;
		border: 1px solid var(--line2);
		border-radius: 8px;
		padding: 0 6px;
		margin-left: 4px;
	}
	.miss td {
		background: color-mix(in srgb, var(--bad, #b42318) 8%, transparent);
	}
	.bad {
		color: var(--bad, #b42318);
		font-size: 13px;
	}
	.opts {
		border: 1px solid var(--line2);
		border-radius: 6px;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.keys {
		line-height: 1.8;
	}
	progress {
		width: 100%;
	}
	.narrow {
		display: none;
	}
	@media (max-width: 640px) {
		.narrow {
			display: block;
		}
		.wide {
			display: none;
		}
	}
</style>

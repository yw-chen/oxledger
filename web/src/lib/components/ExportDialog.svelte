<script lang="ts">
	// 匯出 CSV (design 06 §6.11): which columns, in which order, with or
	// without the description; then the browser downloads the file. What is
	// exported is what the person sees, at most 10,000 rows.
	import { EXPORT_COLUMNS, exportProblem, fetchExport, saveBlob } from '#lib/exports.js';
	import ErrorBox from './ErrorBox.svelte';

	let {
		source,
		initial,
		extra = [],
		onclose
	}: {
		/** A saved view, or conditions (`where`, `project_id`). */
		source: { view_id: string } | { where?: unknown; project_id?: string | null };
		/** The columns checked at first, in order (the view's own). */
		initial: string[];
		/** Custom fields the person can read: `cf:<id>` and its name. */
		extra?: { key: string; label: string }[];
		onclose: () => void;
	} = $props();

	type Col = { key: string; label: string; on: boolean };
	const all = [...EXPORT_COLUMNS, ...extra];
	const labelOf = (k: string) => all.find((c) => c.key === k)?.label ?? k;
	let cols = $state<Col[]>([
		...initial.map((k) => ({ key: k, label: labelOf(k), on: true })),
		...all.filter((c) => !initial.includes(c.key)).map((c) => ({ ...c, on: false }))
	]);
	let description = $state(false);
	let busy = $state(false);
	let problem = $state<string | null>(null);
	let error = $state<unknown>(null);

	const chosen = $derived(cols.filter((c) => c.on).map((c) => c.key));
	function move(i: number, d: -1 | 1) {
		const j = i + d;
		if (j < 0 || j >= cols.length) return;
		[cols[i], cols[j]] = [cols[j], cols[i]];
	}

	async function run() {
		busy = true;
		problem = null;
		error = null;
		try {
			// The view's own columns, untouched: let the server use them as they are.
			const same = chosen.length === initial.length && chosen.every((k, i) => k === initial[i]);
			const body = { ...source, include_description: description, ...(same && 'view_id' in source ? {} : { columns: chosen }) };
			const f = await fetchExport('/exports/issues', body);
			saveBlob(f.blob, f.filename);
			onclose();
		} catch (e) {
			problem = exportProblem(e);
			if (!problem) error = e;
		} finally {
			busy = false;
		}
	}
</script>

<div class="modal-bg" role="presentation">
	<div class="modal" role="dialog" aria-modal="true" aria-label="匯出 CSV" data-testid="export-dialog">
		<h3>匯出 CSV</h3>
		<p class="hint">匯出符合已儲存條件、而且你看得到的所有議題（不含頁面上臨時輸入的搜尋與排序），最多 10,000 列。檔案用 UTF-8 編碼，可以直接用 Excel 開啟。</p>
		<ul class="cols" data-testid="export-columns">
			{#each cols as c, i (c.key)}
				<li>
					<label><input type="checkbox" bind:checked={c.on} data-testid="export-col" data-key={c.key} /> {c.label}</label>
					<span class="sp"></span>
					<button type="button" class="btn sm ghost mv" onclick={() => move(i, -1)} disabled={i === 0} aria-label="{c.label} 往上">↑</button>
					<button type="button" class="btn sm ghost mv" onclick={() => move(i, 1)} disabled={i === cols.length - 1} aria-label="{c.label} 往下">↓</button>
				</li>
			{/each}
		</ul>
		<label><input type="checkbox" bind:checked={description} data-testid="export-description" /> 包含說明</label>
		{#if problem}<div class="err" role="alert" data-testid="export-problem">{problem}</div>{/if}
		<ErrorBox {error} />
		<div class="row" style="margin-top:10px">
			<button class="btn pri" onclick={run} disabled={busy || !chosen.length} data-testid="export-run">{#if busy}<span class="spin"></span> 匯出中…{:else}匯出{/if}</button>
			<button class="btn" onclick={onclose} disabled={busy}>{problem ? '調整篩選' : '取消'}</button>
		</div>
	</div>
</div>

<style>
	.cols {
		list-style: none;
		margin: 8px 0;
		padding: 0;
		max-height: 300px;
		overflow: auto;
		border: 1px solid var(--line2);
		border-radius: 6px;
	}
	.cols li {
		display: flex;
		align-items: center;
		gap: 4px;
		padding: 2px 8px;
	}
	.sp {
		flex: 1;
	}
	.row {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	@media (max-width: 640px) {
		.mv {
			display: none;
		}
	}
</style>

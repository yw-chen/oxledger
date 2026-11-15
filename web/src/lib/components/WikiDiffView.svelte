<script lang="ts">
	// Design 06 §6.6: a diff in two columns (≥ 1024 px) or unified (narrower,
	// or by choice). Deleted lines red, inserted green, the changed words
	// darker, and a `−`／`+` before each line so color is not the only cue.
	// Unchanged stretches between hunks fold into 「⋯ 展開 N 行」; opening one
	// needs the newer text (`newText`, or `loadNew` fetches it once).
	import { onMount } from 'svelte';
	import type { DiffHunk } from '$api/types';
	import { diffRows, lines, splitRows, type Line } from '#lib/wiki.js';

	let {
		hunks,
		truncated = false,
		newText = null,
		loadNew = null,
		leftLabel = '舊',
		rightLabel = '新',
		testid = 'wiki-diff'
	}: {
		hunks: DiffHunk[];
		truncated?: boolean;
		newText?: string | null;
		loadNew?: (() => Promise<string>) | null;
		leftLabel?: string;
		rightLabel?: string;
		testid?: string;
	} = $props();

	let wide = $state(true);
	let mode = $state<'auto' | 'split' | 'unified'>('auto');
	const split = $derived(mode === 'split' || (mode === 'auto' && wide));
	let open = $state<Set<number>>(new Set());
	let loaded = $state<string | null>(null);
	let loadFailed = $state(false);
	const known = $derived(newText ?? loaded);
	const newLines = $derived(known === null ? null : lines(known));
	const rows = $derived(diffRows(hunks, newLines, open));
	const pairs = $derived(splitRows(rows));

	onMount(() => {
		const mq = matchMedia('(min-width: 1024px)');
		const set = () => (wide = mq.matches);
		set();
		mq.addEventListener('change', set);
		return () => mq.removeEventListener('change', set);
	});

	async function expand(id: number) {
		if (known === null && loadNew) {
			try {
				loaded = await loadNew();
			} catch {
				loadFailed = true;
				return;
			}
		}
		open = new Set([...open, id]);
	}
	const sign = (op: string) => (op === 'delete' ? '−' : op === 'insert' ? '+' : ' ');
	const canExpand = $derived(known !== null || !!loadNew);
</script>

{#snippet text(l: Line)}
	{#if l.words?.length}{#each l.words as w, i (i)}<span class:wdel={w.op === 'delete'} class:wins={w.op === 'insert'}>{w.text}</span>{/each}{:else}{l.text}{/if}
{/snippet}
{#snippet gap(g: { id: number; count: number })}
	<button type="button" class="gapbtn" onclick={() => expand(g.id)} disabled={!canExpand || loadFailed} data-testid="{testid}-gap">⋯ 展開 {g.count} 行</button>
{/snippet}

<div class="wd-wrap" data-testid={testid}>
	<div class="row" style="gap:6px;margin-bottom:6px">
		<div class="dtabs" role="group" aria-label="差異顯示方式">
			<button type="button" class:on={split} onclick={() => (mode = 'split')} aria-pressed={split} data-testid="{testid}-split">兩欄</button>
			<button type="button" class:on={!split} onclick={() => (mode = 'unified')} aria-pressed={!split} data-testid="{testid}-unified">統一格式</button>
		</div>
		{#if loadFailed}<span class="hint">無法載入未改動的段落。</span>{/if}
	</div>
	{#if !hunks.length}
		<div class="empty" data-testid="{testid}-same">兩個版本的內容相同。</div>
	{:else if split}
		<table class="wdt split" aria-label="差異（兩欄）">
			<thead><tr><th colspan="3">{leftLabel}</th><th colspan="3">{rightLabel}</th></tr></thead>
			<tbody>
				{#each pairs as p, i (i)}
					{#if p.kind === 'gap'}
						<tr class="g"><td colspan="6">{@render gap(p)}</td></tr>
					{:else}
						<tr>
							{#if p.left}
								<td class="no">{p.left.oldNo ?? ''}</td><td class="sg {p.left.op}">{sign(p.left.op)}</td><td class="tx {p.left.op}">{@render text(p.left)}</td>
							{:else}<td class="no"></td><td class="sg"></td><td class="tx empty-cell"></td>{/if}
							{#if p.right}
								<td class="no">{p.right.newNo ?? ''}</td><td class="sg {p.right.op}">{sign(p.right.op)}</td><td class="tx {p.right.op}">{@render text(p.right)}</td>
							{:else}<td class="no"></td><td class="sg"></td><td class="tx empty-cell"></td>{/if}
						</tr>
					{/if}
				{/each}
			</tbody>
		</table>
	{:else}
		<table class="wdt" aria-label="差異（統一格式）">
			<tbody>
				{#each rows as r, i (i)}
					{#if r.kind === 'gap'}
						<tr class="g"><td colspan="4">{@render gap(r)}</td></tr>
					{:else}
						<tr data-op={r.op}><td class="no">{r.oldNo ?? ''}</td><td class="no">{r.newNo ?? ''}</td><td class="sg {r.op}">{sign(r.op)}</td><td class="tx {r.op}">{@render text(r)}</td></tr>
					{/if}
				{/each}
			</tbody>
		</table>
	{/if}
	{#if truncated}<div class="banner w" style="margin-top:6px" data-testid="{testid}-truncated">差異太大，只顯示前 2,000 行。</div>{/if}
</div>

<style>
	.wd-wrap { overflow-x: auto; }
	.wdt { width: 100%; border-collapse: collapse; font-family: var(--mono, ui-monospace, monospace); font-size: 12.5px; table-layout: fixed; }
	.wdt th { text-align: left; font-family: inherit; font-weight: 600; color: var(--muted); padding: 2px 6px; border-bottom: 1px solid var(--line); }
	.wdt td { padding: 0 6px; vertical-align: top; }
	.wdt .no { width: 40px; color: var(--dim); text-align: right; user-select: none; }
	.wdt .sg { width: 14px; user-select: none; }
	.wdt .tx { white-space: pre-wrap; word-break: break-word; }
	.wdt .delete { background: var(--bad-a, rgba(220, 60, 60, 0.15)); }
	.wdt .insert { background: var(--ok-a, rgba(60, 180, 90, 0.15)); }
	.wdt span.wdel { background: rgba(220, 60, 60, 0.4); border-radius: 2px; }
	.wdt span.wins { background: rgba(60, 180, 90, 0.4); border-radius: 2px; }
	.wdt .empty-cell { background: var(--bg2); }
	.wdt tr.g td { text-align: center; background: var(--bg2); }
	.gapbtn { background: none; border: 0; color: var(--act); cursor: pointer; font: inherit; padding: 2px; }
	.gapbtn:disabled { color: var(--dim); cursor: default; }
	.split .no { width: 34px; }
	/* Column widths: number, sign, text on each side. */
	.split td.tx { width: calc(50% - 48px); }
</style>

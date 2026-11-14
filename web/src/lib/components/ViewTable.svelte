<script lang="ts">
	// A list view's result (contract 05 §6.1): columns, group headers with
	// counts, overdue marked in words, totals over every visible match, and
	// 「載入更多」. Rows open the issue.
	import type { RunInput, ViewRun } from '$api/types';
	import { ApiError } from '#lib/api.js';
	import { cellText, countText } from '#lib/filter.js';
	import ErrorBox from './ErrorBox.svelte';

	let {
		run,
		reloadKey,
		sort = [],
		sortable = [],
		onsort,
		onclear,
		onnarrow,
		onerror
	}: {
		run: (i: RunInput) => Promise<ViewRun>;
		/** Changes whenever the definition or the search text changes. */
		reloadKey: string;
		sort?: { field: string; dir: string }[];
		sortable?: string[];
		onsort?: (field: string) => void;
		onclear?: () => void;
		/** 「縮小範圍」 after `query_timeout` (§6): opens the condition editor. */
		onnarrow?: () => void;
		/** Called with the error, or null after a successful run. */
		onerror?: (e: unknown) => void;
	} = $props();

	let data = $state<ViewRun | null>(null);
	let items = $state<NonNullable<ViewRun['items']>>([]);
	let error = $state<unknown>(null);
	let loading = $state(false);
	let slow = $state(false);
	/** §6: no loading sign in the first 400 ms (avoids a flash). */
	let skeleton = $state(false);
	let more = $state(false);
	let seq = 0;

	async function load(cursor: string | null = null, retried = false) {
		const n = ++seq;
		if (!cursor) {
			loading = true;
			error = null;
		} else more = true;
		const t = setTimeout(() => n === seq && (slow = true), 2000);
		const t0 = cursor ? 0 : setTimeout(() => n === seq && (skeleton = true), 400);
		try {
			const r = await run({ cursor, limit: 50 });
			if (n !== seq) return;
			// `groups` covers this page only: keep the heads of earlier pages.
			const heads = new Map(cursor ? groupHeads : []);
			for (const g of r.groups ?? []) heads.set(JSON.stringify(g.value ?? null), g);
			groupHeads = heads;
			data = r;
			if (cursor) {
				// An issue updated between pages can come back; show it once.
				const seen = new Set(items.map((x) => x.id));
				items = [...items, ...(r.items ?? []).filter((x) => !seen.has(x.id))];
			} else items = r.items ?? [];
			if (!cursor) onerror?.(null);
		} catch (e) {
			if (n !== seq) return;
			// §6: a stale cursor reloads from the top once.
			if (e instanceof ApiError && e.code === 'invalid_cursor' && !retried) return load(null, true);
			error = e;
			onerror?.(e);
		} finally {
			clearTimeout(t);
			clearTimeout(t0);
			if (n === seq) {
				loading = false;
				slow = false;
				skeleton = false;
				more = false;
			}
		}
	}
	$effect(() => {
		void reloadKey;
		load();
	});

	const cols = $derived((data?.columns ?? []).filter((c) => c.key !== 'key'));
	type Head = NonNullable<ViewRun['groups']>[number];
	let groupHeads = $state<Map<string, Head>>(new Map());
	const groups = $derived(groupHeads);
	const sortOf = (k: string) => sort.find((s) => s.field === k);
	const NUMERIC = new Set(['estimated_hours', 'spent_hours']);
	const TOTAL_LABEL: Record<string, string> = { estimated_hours: '預估工時', spent_hours: '已記工時' };
	// Rows with the group header before the first item of each group.
	const rows = $derived.by(() => {
		const out: ({ kind: 'g'; key: string; label: string; count: string } | { kind: 'i'; item: (typeof items)[number] })[] = [];
		let last: string | undefined;
		for (const it of items) {
			if (data?.groups) {
				const k = JSON.stringify(it.group ?? null);
				if (k !== last) {
					const g = groups.get(k);
					out.push({ kind: 'g', key: k, label: g?.label ?? '（未設定）', count: countText(g?.count) });
					last = k;
				}
			}
			out.push({ kind: 'i', item: it });
		}
		return out;
	});
	export function reload() {
		load();
	}
</script>

{#if loading && !data}
	{#if skeleton}
		<div data-testid="view-skeleton" aria-busy="true" aria-label="載入中">
			{#if slow}<div class="hint" data-testid="view-slow">還在載入…</div>{/if}
			<table class="t skel-table">
				<tbody>
					{#each { length: 8 } as _, i (i)}
						<tr><td style="width:90px"><span class="skel"></span></td><td><span class="skel" style="width:{60 + ((i * 37) % 35)}%"></span></td><td style="width:120px"><span class="skel"></span></td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
{:else if error}
	<ErrorBox {error} onaction={(a) => (a === 'narrow' && onnarrow ? onnarrow() : load())} />
{:else if data}
	{#if !items.length}
		<div class="empty" data-testid="view-empty">沒有符合條件的議題。{#if onclear}<button class="btn sm" onclick={onclear}>清除條件</button>{/if}</div>
	{:else}
		<div style="overflow-x:auto">
			<table class="t vtable" data-testid="view-table">
				<thead>
					<tr>
						<th>單號</th>
						{#each cols as c (c.key)}
							<th>
								{#if onsort && sortable.includes(c.key)}
									{@const s = sortOf(c.key)}
									<button onclick={() => onsort?.(c.key)} aria-label="依{c.label}排序" data-testid="sort-{c.key}">{c.label}{s ? (s.dir === 'asc' ? ' ↑' : ' ↓') : ''}</button>
								{:else}{c.label}{/if}
							</th>
						{/each}
					</tr>
				</thead>
				<tbody>
					{#each rows as r, i (r.kind === 'g' ? `g${r.key}${i}` : r.item.id)}
						{#if r.kind === 'g'}
							<tr class="grp" data-testid="view-group"><td colspan={cols.length + 1}>{r.label} <span class="hint">· {r.count}</span></td></tr>
						{:else}
							{@const v = r.item.values ?? {}}
							<tr data-testid="view-row">
								<td><a class="key" href="/issue/{r.item.key}">{r.item.key}</a></td>
								{#each cols as c (c.key)}
									{#if c.key === 'subject'}
										<td><a href="/issue/{r.item.key}" style="color:inherit">{cellText(v.subject)}</a></td>
									{:else if c.key === 'due_date' && v.overdue}
										<td class="overdue">{cellText(v.due_date)} <span class="chip c-bad">逾期</span></td>
									{:else}
										<td class:num={NUMERIC.has(c.key)}>{cellText(v[c.key])}</td>
									{/if}
								{/each}
							</tr>
						{/if}
					{/each}
				</tbody>
				{#if data.totals && Object.keys(data.totals).length}
					<tfoot>
						<tr data-testid="view-totals">
							<td><b>合計</b></td>
							<td colspan={cols.length}>
								{#each Object.entries(data.totals) as [k, t] (k)}
									<span style="margin-right:14px">{TOTAL_LABEL[k] ?? k}：<b class="mono">{t.value ?? '—'}</b>{t.complete ? '' : '（部分）'}</span>
								{/each}
							</td>
						</tr>
					</tfoot>
				{/if}
			</table>
		</div>
		<div class="row" style="gap:10px;margin-top:8px">
			{#if data.next_cursor}<button class="btn" onclick={() => load(data!.next_cursor)} disabled={more} data-testid="view-more">{more ? '載入中…' : '載入更多'}</button>{/if}
			<span class="hint">只包含你看得到的議題。{data.coverage?.truncated ? '文字搜尋只看前 8000 筆相符的。' : ''}</span>
		</div>
	{/if}
{/if}

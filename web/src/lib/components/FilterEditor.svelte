<script lang="ts">
	// The condition editor (contract 05 §6.2): a right-hand drawer shared by
	// views and search. all / any at the top, conditions, one level of
	// sub-groups (two levels in all). The preview says only whether there are
	// results and shows the first five titles — never a total.
	import type { FilterFieldInfo, FilterOption } from '$api/types';
	import {
		countConds,
		defaultValue,
		depth,
		fromGroup,
		groupIncomplete,
		incomplete,
		MAX_CONDS,
		MAX_DEPTH,
		MULTI,
		NO_VALUE,
		opLabel,
		RANGE_LABEL,
		CATEGORY_LABEL,
		TRIAGE_LABEL,
		rangeDates,
		addDays,
		type Cond,
		type Group,
		type Where
	} from '#lib/filter.js';
	import { sourceOptions } from '#lib/filterOptions.js';
	import ErrorBox from './ErrorBox.svelte';

	let {
		fields,
		today,
		weekStart = 1,
		initial,
		projectView = false,
		badField = null,
		preview,
		onapply,
		oncancel,
		applyLabel = '套用'
	}: {
		fields: FilterFieldInfo[];
		today: string;
		weekStart?: number;
		initial: Group;
		/** A project view: restricted fields cannot be used (R-VD3). */
		projectView?: boolean;
		/** A field the server rejected (e.g. restricted in a project view). */
		badField?: string | null;
		preview?: (w: Where) => Promise<{ titles: string[] }>;
		onapply: (g: Group) => void;
		oncancel: () => void;
		applyLabel?: string;
	} = $props();

	// svelte-ignore state_referenced_locally
	let g = $state<Group>(structuredClone($state.snapshot(initial) as Group));
	const fieldOf = (k: string) => fields.find((f) => f.field === k);
	const total = $derived(countConds(g));
	const blocked = $derived(groupIncomplete(g));

	// Options for fields that look them up (people, versions).
	let looked = $state<Record<string, FilterOption[]>>({});
	function ensure(f: FilterFieldInfo | undefined) {
		if (!f || f.options?.length || !f.options_source || looked[f.field]) return;
		looked[f.field] = [];
		sourceOptions(f).then((o) => (looked[f.field] = o));
	}
	$effect(() => {
		const walk = (x: Group) => x.items.forEach((i) => ('mode' in i ? walk(i) : ensure(fieldOf(i.field))));
		walk(g);
	});
	function optionsOf(f: FilterFieldInfo | undefined): FilterOption[] {
		if (!f) return [];
		if (f.field === 'status_category') return Object.entries(CATEGORY_LABEL).map(([id, label]) => ({ id, label }));
		if (f.field === 'triage' && !f.options?.length) return Object.entries(TRIAGE_LABEL).map(([id, label]) => ({ id, label }));
		return f.options?.length ? f.options : (looked[f.field] ?? []);
	}

	function addCond(target: Group) {
		if (total >= MAX_CONDS) return;
		target.items.push({ field: '', op: '', value: undefined });
	}
	function addGroup() {
		if (total >= MAX_CONDS) return;
		g.items.push({ mode: g.mode === 'and' ? 'or' : 'and', items: [{ field: '', op: '', value: undefined }] });
	}
	function remove(target: Group, i: number) {
		target.items.splice(i, 1);
	}
	function setField(c: Cond, k: string) {
		const f = fieldOf(k);
		c.field = k;
		c.op = f?.ops.includes('in') ? 'in' : (f?.ops[0] ?? 'eq');
		c.value = defaultValue(f, c.op, today);
		ensure(f);
	}
	function setOp(c: Cond, op: string) {
		const f = fieldOf(c.field);
		const was = c.op;
		c.op = op;
		// Keep a chosen value when it still fits (one ↔ many converts).
		if (MULTI.has(op) && !MULTI.has(was) && c.value != null && c.value !== '' && !Array.isArray(c.value)) c.value = [c.value];
		else if (!MULTI.has(op) && MULTI.has(was) && Array.isArray(c.value)) c.value = c.value[0] ?? defaultValue(f, op, today);
		else if (NO_VALUE.has(op) || op === 'within' || op === 'between' || NO_VALUE.has(was) || was === 'within' || was === 'between') c.value = defaultValue(f, op, today);
	}
	function toggle(c: Cond, id: unknown) {
		const list = Array.isArray(c.value) ? [...c.value] : [];
		const i = list.findIndex((x) => JSON.stringify(x) === JSON.stringify(id));
		if (i >= 0) list.splice(i, 1);
		else list.push(id);
		c.value = list;
	}
	const has = (c: Cond, id: unknown) => Array.isArray(c.value) && c.value.some((x) => JSON.stringify(x) === JSON.stringify(id));
	const dynamic = (f: FilterFieldInfo | undefined, d: string) => !!f?.dynamic?.includes(d);
	const isDate = (f: FilterFieldInfo | undefined) => !!f && ['date', 'datetime'].includes(f.type);
	const isRel = (v: unknown): v is { rel: string; offset: number } => !!v && typeof v === 'object' && 'rel' in (v as object);
	const md = (iso: string) => `${Number(iso.slice(5, 7))}/${iso.slice(8, 10)}`;
	/** 相對日期 shortcuts: offset days from today (negative = before). */
	const REL_QUICK = [
		{ off: 0, label: '今天' },
		{ off: -7, label: '7 天前' },
		{ off: -30, label: '30 天前' },
		{ off: 7, label: '7 天後' }
	];

	// Preview: 300 ms after the last change, only when every condition is complete.
	let pv = $state<{ titles: string[] } | null>(null);
	let pvError = $state<unknown>(null);
	let pvBusy = $state(false);
	let seq = 0;
	$effect(() => {
		const w = fromGroup($state.snapshot(g) as Group);
		if (!preview || blocked) {
			pv = null;
			return;
		}
		const n = ++seq;
		const t = setTimeout(async () => {
			pvBusy = true;
			try {
				const r = await preview(w);
				if (n === seq) {
					pv = r;
					pvError = null;
				}
			} catch (e) {
				if (n === seq) {
					pv = null;
					pvError = e;
				}
			} finally {
				if (n === seq) pvBusy = false;
			}
		}, 300);
		return () => clearTimeout(t);
	});
	function apply() {
		if (blocked) return;
		onapply($state.snapshot(g) as Group);
	}
	function onkey(e: KeyboardEvent) {
		if (e.key === 'Escape') oncancel();
	}
</script>

<svelte:window onkeydown={onkey} />
<div class="drawer-bg" onclick={oncancel} role="presentation"></div>
<div class="drawer" role="dialog" aria-modal="true" aria-label="條件" data-testid="filter-editor">
	<div class="dh">
		<b>條件</b>
		<span style="flex:1"></span>
		<label class="row" style="gap:4px;font-size:13px">
			<select bind:value={g.mode} aria-label="符合方式" data-testid="fe-mode">
				<option value="and">全部符合</option>
				<option value="or">任一符合</option>
			</select>
		</label>
		<button class="btn sm ghost" onclick={oncancel} aria-label="關閉">✕</button>
	</div>
	<div class="db">
		{#if projectView}<div class="hint" style="margin-bottom:8px">專案視圖的名稱與條件所有成員都看得到；不能使用受限欄位。</div>{/if}
		{#if !g.items.length}<div class="empty" data-testid="fe-empty">還沒有條件，會列出所有你看得到的議題。</div>{/if}
		{@render groupItems(g, true)}
		<div class="row" style="gap:6px">
			<button class="btn sm" onclick={() => addCond(g)} disabled={total >= MAX_CONDS} data-testid="fe-add">＋ 條件</button>
			<button class="btn sm" onclick={addGroup} disabled={total >= MAX_CONDS || depth(g) > MAX_DEPTH} title="條件組最多兩層" data-testid="fe-add-group">＋ 條件組</button>
			{#if total >= MAX_CONDS}<span class="hint">最多 {MAX_CONDS} 個條件</span>{/if}
		</div>
	</div>
	<div class="df">
		<div style="flex-basis:100%" data-testid="fe-preview" aria-live="polite">
			{#if blocked}<span class="hint">有條件還沒填完。</span>
			{:else if pvBusy && !pv}<span class="hint"><span class="spin"></span> 預覽中…</span>
			{:else if pvError}<ErrorBox error={pvError} />
			{:else if pv}
				{#if pv.titles.length}
					<span class="hint">有結果，例如：</span>
					<ul style="margin:2px 0 0;padding-left:18px;font-size:12.5px">{#each pv.titles as t, i (i)}<li>{t}</li>{/each}</ul>
				{:else}<span class="hint">沒有結果。</span>{/if}
			{/if}
		</div>
		<button class="btn pri" onclick={apply} disabled={blocked} data-testid="fe-apply">{applyLabel}</button>
		<button class="btn" onclick={oncancel}>取消</button>
	</div>
</div>

{#snippet groupItems(grp: Group, top: boolean)}
	{#each grp.items as item, i (i)}
		{#if 'mode' in item}
			<div class="subgroup" data-testid="fe-group">
				<div class="row" style="gap:6px;margin-bottom:6px">
					<select bind:value={item.mode} aria-label="條件組的符合方式">
						<option value="and">全部符合</option>
						<option value="or">任一符合</option>
					</select>
					<span style="flex:1"></span>
					<button class="btn sm ghost" onclick={() => remove(grp, i)} aria-label="移除條件組">移除組</button>
				</div>
				{@render groupItems(item, false)}
				<div class="row" style="gap:6px">
					<button class="btn sm" onclick={() => addCond(item)} disabled={total >= MAX_CONDS}>＋ 條件</button>
					<!-- §6.2: a third level is not allowed; the button stays, disabled, with the reason in words. -->
					<button class="btn sm" disabled aria-describedby="fe-depth-{i}" data-testid="fe-add-subgroup">＋ 條件組</button>
					<span class="depth-note" id="fe-depth-{i}" data-testid="fe-depth-limit">最多兩層</span>
				</div>
			</div>
		{:else}
			{@render condRow(grp, item, i)}
		{/if}
		{#if !top && i < grp.items.length - 1}<div class="hint" style="margin:-4px 0 4px">{grp.mode === 'and' ? '而且' : '或'}</div>{/if}
	{/each}
{/snippet}

{#snippet condRow(grp: Group, c: Cond, i: number)}
	{@const f = fieldOf(c.field)}
	{@const bad = (projectView && f?.restricted) || (badField && c.field === badField)}
	<div class="cond" class:bad data-testid="fe-cond">
		<div class="r">
			<select value={c.field} onchange={(e) => setField(c, e.currentTarget.value)} aria-label="欄位" data-testid="fe-field">
				<option value="" disabled>選欄位</option>
				{#each fields as x (x.field)}<option value={x.field}>{x.label}{x.restricted ? '（受限）' : ''}</option>{/each}
			</select>
			{#if f}
				<select value={c.op} onchange={(e) => setOp(c, e.currentTarget.value)} aria-label="比較方式" data-testid="fe-op">
					{#each f.ops as op (op)}<option value={op}>{opLabel(op, f.type)}</option>{/each}
				</select>
			{/if}
			<span style="flex:1"></span>
			<button class="btn sm ghost" onclick={() => remove(grp, i)} aria-label="移除條件" data-testid="fe-remove">✕</button>
		</div>
		{#if bad}<div class="hint" style="color:var(--bad)">專案視圖不能使用受限欄位</div>{/if}
		{#if f && !NO_VALUE.has(c.op)}
			{@render valueInput(c, f)}
		{/if}
		{#if f && incomplete(c)}<div class="hint">還沒選值</div>{/if}
	</div>
{/snippet}

{#snippet valueInput(c: Cond, f: FilterFieldInfo)}
	{@const opts = optionsOf(f)}
	{#if c.op === 'within'}
		{@const v = (c.value ?? { range: 'last_n_days', n: 7 }) as { range: string; n?: number }}
		{@const r = rangeDates(v, today, weekStart)}
		<div class="r" style="margin-top:6px">
			<select value={v.range} onchange={(e) => (c.value = { range: e.currentTarget.value, ...(e.currentTarget.value.endsWith('n_days') ? { n: v.n ?? 7 } : {}) })} aria-label="期間">
				{#each Object.entries(RANGE_LABEL) as [k, l] (k)}<option value={k}>{l}</option>{/each}
			</select>
			{#if v.range.endsWith('n_days')}
				<input type="number" min="1" max="366" value={v.n ?? 7} oninput={(e) => (c.value = { range: v.range, n: Math.max(1, Math.min(366, Number(e.currentTarget.value) || 1)) })} style="width:70px" aria-label="天數" />
			{/if}
			{#if r}<span class="hint">（{md(r[0])}–{md(r[1])}）</span>{/if}
		</div>
	{:else if isDate(f)}
		{@render dateInput(c, f)}
	{:else if f.type === 'bool'}
		<div class="r" style="margin-top:6px">
			<select value={String(c.value ?? true)} onchange={(e) => (c.value = e.currentTarget.value === 'true')} aria-label="值">
				<option value="true">是</option>
				<option value="false">否</option>
			</select>
		</div>
	{:else if opts.length || dynamic(f, 'me') || dynamic(f, 'next')}
		{#if MULTI.has(c.op)}
			<div class="optpick" data-testid="fe-options">
				{#if dynamic(f, 'me')}<label><input type="checkbox" checked={has(c, { me: true })} onchange={() => toggle(c, { me: true })} />我</label>{/if}
				{#if dynamic(f, 'next')}<label><input type="checkbox" checked={has(c, { next: true })} onchange={() => toggle(c, { next: true })} />下一個里程碑</label>{/if}
				{#each opts as o (o.id)}<label><input type="checkbox" checked={has(c, o.id)} onchange={() => toggle(c, o.id)} />{o.label}{o.active === false ? '（停用）' : ''}</label>{/each}
				{#if !opts.length && f.options_source}<span class="hint">載入選項中…</span>{/if}
			</div>
		{:else}
			{@const cur = JSON.stringify(c.value ?? '')}
			<div class="r" style="margin-top:6px">
				<select value={cur} onchange={(e) => (c.value = e.currentTarget.value ? JSON.parse(e.currentTarget.value) : '')} aria-label="值" data-testid="fe-value">
					<option value={'""'}>選一個</option>
					{#if dynamic(f, 'me')}<option value={JSON.stringify({ me: true })}>我</option>{/if}
					{#if dynamic(f, 'next')}<option value={JSON.stringify({ next: true })}>下一個里程碑</option>{/if}
					{#each opts as o (o.id)}<option value={JSON.stringify(o.id)}>{o.label}</option>{/each}
				</select>
			</div>
		{/if}
	{:else if MULTI.has(c.op)}
		<div class="r" style="margin-top:6px">
			<input type="text" value={Array.isArray(c.value) ? c.value.join(', ') : ''} oninput={(e) => (c.value = e.currentTarget.value.split(/[,，]/).map((s) => s.trim()).filter(Boolean))} placeholder="用逗號分隔" aria-label="值" data-testid="fe-value" style="flex:1" />
		</div>
	{:else if c.op === 'between'}
		{@const pair = (Array.isArray(c.value) ? c.value : ['', '']) as string[]}
		<div class="r" style="margin-top:6px">
			<input type="text" value={pair[0]} oninput={(e) => (c.value = [e.currentTarget.value, pair[1]])} aria-label="從" style="width:90px" />
			<span>到</span>
			<input type="text" value={pair[1]} oninput={(e) => (c.value = [pair[0], e.currentTarget.value])} aria-label="到" style="width:90px" />
		</div>
	{:else}
		<div class="r" style="margin-top:6px">
			<input type="text" value={String(c.value ?? '')} oninput={(e) => (c.value = e.currentTarget.value)} aria-label="值" data-testid="fe-value" style="flex:1" />
		</div>
	{/if}
{/snippet}

{#snippet dateInput(c: Cond, f: FilterFieldInfo)}
	{#if c.op === 'between'}
		{@const pair = (Array.isArray(c.value) ? c.value : [today, today]) as string[]}
		<div class="r" style="margin-top:6px">
			<input type="date" value={pair[0]} oninput={(e) => (c.value = [e.currentTarget.value, pair[1]])} aria-label="從" />
			<span>到</span>
			<input type="date" value={pair[1]} oninput={(e) => (c.value = [pair[0], e.currentTarget.value])} aria-label="到" />
		</div>
	{:else}
		{@const rel = isRel(c.value)}
		{#if dynamic(f, 'rel')}
			<div class="dtabs" role="tablist" aria-label="日期種類" style="margin-top:6px">
				<button role="tab" type="button" class:on={!rel} aria-selected={!rel} onclick={() => rel && (c.value = today)} data-testid="fe-date-abs">日期</button>
				<button role="tab" type="button" class:on={rel} aria-selected={rel} onclick={() => !rel && (c.value = { rel: 'today', offset: 0 })} data-testid="fe-date-rel">相對日期</button>
			</div>
		{/if}
		<div class="r" style="margin-top:6px" role={dynamic(f, 'rel') ? 'tabpanel' : undefined}>
			{#if isRel(c.value)}
				{@const off = Number(c.value.offset ?? 0)}
				{#each REL_QUICK as q (q.off)}
					<button type="button" class="btn sm" class:on={off === q.off} onclick={() => (c.value = { rel: 'today', offset: q.off })}>{q.label}</button>
				{/each}
				<input type="number" min="-3650" max="3650" value={off} oninput={(e) => (c.value = { rel: 'today', offset: Math.max(-3650, Math.min(3650, Math.trunc(Number(e.currentTarget.value) || 0))) })} style="width:80px" aria-label="相對天數" data-testid="fe-rel-days" />
				<span class="hint" data-testid="fe-rel-text">天（{off === 0 ? '今天' : off < 0 ? `${-off} 天前` : `${off} 天後`}，{md(addDays(today, off))}）</span>
			{:else}
				<input type="date" value={String(c.value ?? '')} oninput={(e) => (c.value = e.currentTarget.value)} aria-label="日期" />
			{/if}
		</div>
	{/if}
{/snippet}

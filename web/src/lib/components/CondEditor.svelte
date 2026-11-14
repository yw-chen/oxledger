<script lang="ts">
	// The form condition editor (contract 05 §6.10 ③): like the search
	// condition editor (§6.2) but only the form language's ops, at most two
	// levels and 5 items per group, and no restricted fields. `group` is a
	// deep $state object: edits mutate it in place.
	import { CORE_COND, OPS, OP_LABEL, CATEGORY_LABEL, type CondCatalog, type Cond, type Group, type Item, isGroup, items, wrapValue } from '#lib/formcond.js';
	import Self from './CondEditor.svelte';

	let { group, cat, depth = 1, onremove }: { group: Group; cat: CondCatalog; depth?: number; onremove?: () => void } = $props();

	const list = $derived(items(group));
	const mode = $derived('all' in group ? 'all' : 'any');
	const fields = $derived([...cat.entries()]);

	function setMode(m: 'all' | 'any') {
		if (m === mode) return;
		const l = items(group);
		const g = group as Record<string, unknown>;
		delete g[mode];
		g[m] = l;
	}
	const add = () => list.push({ field: '', op: 'eq', value: '' } as Cond);
	const addGroup = () => list.push({ any: [{ field: '', op: 'eq', value: '' }] } as Group);
	const removeAt = (i: number) => list.splice(i, 1);

	const kindOf = (field: string) => cat.get(field)?.kind ?? CORE_COND[field]?.kind ?? 'text';
	function plain(v: unknown): unknown {
		return v && typeof v === 'object' && !Array.isArray(v) ? Object.values(v as object)[0] : v;
	}
	function setField(c: Cond, field: string) {
		c.field = field;
		const ops = OPS[kindOf(field)];
		if (!ops.includes(c.op)) c.op = ops[0];
		c.value = c.op === 'in' ? [] : '';
	}
	function setOp(c: Cond, op: string) {
		const wasIn = c.op === 'in';
		c.op = op;
		if (op === 'is_set' || op === 'not_set') delete c.value;
		else if (op === 'in' && !wasIn) c.value = c.value === undefined || c.value === '' ? [] : [c.value];
		else if (op !== 'in' && wasIn) c.value = Array.isArray(c.value) && c.value.length ? c.value[0] : '';
		else if (c.value === undefined) c.value = '';
	}
	function setValue(c: Cond, raw: string) {
		const k = kindOf(c.field);
		const v: unknown = k === 'bool' ? (raw === '' ? '' : raw === 'true') : raw;
		c.value = wrapValue(cat, c.field, v);
	}
	function toggleIn(c: Cond, id: string, on: boolean) {
		const cur = Array.isArray(c.value) ? c.value : [];
		const has = cur.some((x) => plain(x) === id);
		if (on && !has) c.value = [...cur, wrapValue(cat, c.field, id)];
		if (!on && has) c.value = cur.filter((x) => plain(x) !== id);
	}
	function setInText(c: Cond, raw: string) {
		c.value = raw
			.split(/[,，]/)
			.map((s) => s.trim())
			.filter(Boolean)
			.map((s) => wrapValue(cat, c.field, s));
	}
	const optionsOf = (field: string) => (field === 'status_category' ? Object.entries(CATEGORY_LABEL).map(([id, label]) => ({ id, label })) : cat.get(field)?.options);
</script>

<div class="cg" class:nested={depth > 1} data-testid="cond-group">
	<div class="row hd">
		<select value={mode} onchange={(e) => setMode(e.currentTarget.value as 'all' | 'any')} aria-label="符合方式" data-testid="cond-mode">
			<option value="all">全部符合</option>
			<option value="any">任一符合</option>
		</select>
		{#if onremove}<button type="button" class="btn sm ghost" onclick={onremove} aria-label="移除這一組">移除這一組</button>{/if}
	</div>
	{#each list as it, i (i)}
		{#if isGroup(it as Item)}
			<Self group={it as Group} {cat} depth={depth + 1} onremove={() => removeAt(i)} />
		{:else}
			{@const c = it as Cond}
			{@const k = kindOf(c.field)}
			{@const opts = optionsOf(c.field)}
			<div class="row cr" data-testid="cond-row">
				<select value={c.field} onchange={(e) => setField(c, e.currentTarget.value)} aria-label="欄位" data-testid="cond-field">
					<option value="" disabled>選欄位</option>
					{#each fields as [key, f] (key)}<option value={key}>{f.name}</option>{/each}
					{#if c.field && !cat.has(c.field)}<option value={c.field}>{c.field}（不能用）</option>{/if}
				</select>
				<select value={c.op} onchange={(e) => setOp(c, e.currentTarget.value)} aria-label="條件" data-testid="cond-op" disabled={!c.field}>
					{#each OPS[k] as op (op)}<option value={op}>{OP_LABEL[op]}</option>{/each}
				</select>
				{#if c.op !== 'is_set' && c.op !== 'not_set' && c.field}
					{#if c.op === 'in' && opts?.length}
						<span class="ins">
							{#each opts as o (o.id)}<label><input type="checkbox" checked={Array.isArray(c.value) && c.value.some((x) => String(plain(x)).toLowerCase() === o.id.toLowerCase())} onchange={(e) => toggleIn(c, o.id, e.currentTarget.checked)} /> {o.label}</label>{/each}
						</span>
					{:else if c.op === 'in'}
						<input value={Array.isArray(c.value) ? c.value.map(plain).join(', ') : ''} onchange={(e) => setInText(c, e.currentTarget.value)} placeholder="用逗號分開" aria-label="值" data-testid="cond-value" />
					{:else if k === 'bool'}
						<select value={plain(c.value) === true ? 'true' : plain(c.value) === false ? 'false' : ''} onchange={(e) => setValue(c, e.currentTarget.value)} aria-label="值" data-testid="cond-value">
							<option value="" disabled>選擇</option><option value="true">是</option><option value="false">否</option>
						</select>
					{:else if opts?.length}
						<select value={String(plain(c.value) ?? '')} onchange={(e) => setValue(c, e.currentTarget.value)} aria-label="值" data-testid="cond-value">
							<option value="" disabled>選擇</option>
							{#each opts as o (o.id)}<option value={o.id}>{o.label}</option>{/each}
						</select>
					{:else if k === 'date'}
						<input type="date" value={String(plain(c.value) ?? '')} oninput={(e) => setValue(c, e.currentTarget.value)} aria-label="值" data-testid="cond-value" />
					{:else}
						<input value={String(plain(c.value) ?? '')} oninput={(e) => setValue(c, e.currentTarget.value)} inputmode={k === 'number' ? 'decimal' : undefined} placeholder={k === 'ref' ? 'id' : ''} aria-label="值" data-testid="cond-value" />
					{/if}
				{/if}
				<button type="button" class="btn sm ghost" onclick={() => removeAt(i)} aria-label="移除條件" data-testid="cond-remove">×</button>
			</div>
		{/if}
	{/each}
	<div class="row ft">
		<button type="button" class="btn sm" onclick={add} disabled={list.length >= 5} data-testid="cond-add">＋ 條件</button>
		{#if depth < 2}<button type="button" class="btn sm ghost" onclick={addGroup} disabled={list.length >= 5}>＋ 一組（{mode === 'all' ? '任一' : '全部'}）</button>{/if}
		{#if list.length >= 5}<span class="hint">每一組最多 5 條</span>{/if}
	</div>
</div>

<style>
	.cg {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.cg.nested {
		border-left: 2px solid var(--line2);
		padding-left: 8px;
		margin: 2px 0;
	}
	.row {
		gap: 6px;
		flex-wrap: wrap;
	}
	.ins {
		display: inline-flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	select,
	input {
		width: auto;
	}
</style>

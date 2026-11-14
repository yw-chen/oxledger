<script lang="ts" module>
	export type MdOpts = {
		context?: { issue: string } | { project: string } | null;
		notice?: 'plain' | 'agent' | null;
		draft?: string | null;
		guard?: boolean;
		/** The saved text (dirty and drafts compare to it); moves on a reload. */
		base?: string;
	};
</script>

<script lang="ts">
	// One editable field, driven by the static schema (§5.1). Values are what
	// a command document carries: ids for references, YYYY-MM-DD for dates.
	import type { Assignee, FieldSchema, VersionView } from '$api/types';
	import { fieldLabel } from '$lib/format';
	import { LIMIT, type Mentionable } from '$lib/markdown';
	import MarkdownEditor from './MarkdownEditor.svelte';

	let {
		field,
		value = $bindable(),
		assignees = [],
		versions = [],
		required = false,
		disabled = false,
		md = null
	}: {
		field: FieldSchema;
		value: unknown;
		assignees?: Assignee[];
		versions?: VersionView[];
		required?: boolean;
		disabled?: boolean;
		/** The description as Markdown (design 06 §6.2): the editor instead of a textarea. */
		md?: MdOpts | null;
	} = $props();

	const label = $derived(field.name ?? fieldLabel(field.key));
	const id = $derived(`f-${field.key.replace(/[^a-z0-9]/gi, '_')}`);
	const str = (v: unknown) => (v === null || v === undefined ? '' : String(v));
	function set(v: unknown) {
		value = v;
	}
	function num(s: string) {
		if (s.trim() === '') return null;
		const n = Number(s);
		return Number.isFinite(n) ? n : s;
	}
</script>

<label class="f" for={id}>
	<span>{label}{#if required}<em> *</em>{/if}</span>
	{#if field.type === 'text' && field.key === 'description' && md}
		<MarkdownEditor
			bind:value={() => str(value), (v) => set(v)}
			people={assignees as Mentionable[]}
			context={md.context ?? null}
			limit={LIMIT.description}
			notice={md.notice ?? null}
			draft={md.draft ?? null}
			guard={md.guard ?? true}
			base={md.base}
			{disabled}
			{id}
			{label}
			testid="md-description"
		/>
	{:else if field.type === 'text'}
		<textarea {id} {disabled} value={str(value)} oninput={(e) => set(e.currentTarget.value)}></textarea>
	{:else if field.type === 'date'}
		<input {id} {disabled} type="date" value={str(value)} oninput={(e) => set(e.currentTarget.value || null)} style="background:var(--bg2);border:1px solid var(--line2);border-radius:7px;padding:4px 8px" />
	{:else if field.type === 'int' || field.type === 'float'}
		<input {id} {disabled} type="text" inputmode="decimal" value={str(value)} oninput={(e) => set(num(e.currentTarget.value))} />
	{:else if field.type === 'bool'}
		<select {id} {disabled} value={value === true ? 'true' : value === false ? 'false' : ''} onchange={(e) => set(e.currentTarget.value === '' ? null : e.currentTarget.value === 'true')}>
			<option value="">（空）</option><option value="true">是</option><option value="false">否</option>
		</select>
	{:else if field.type === 'principal_ref' || field.type === 'user'}
		<select {id} {disabled} value={str(value)} onchange={(e) => set(e.currentTarget.value || null)}>
			<option value="">（未指派）</option>
			{#each assignees as a (a.id)}<option value={a.id}>{a.name}{a.kind === 'group' ? '（群組）' : ''}</option>{/each}
			{#if value && !assignees.some((a) => a.id === value)}<option value={str(value)}>（目前的值）</option>{/if}
		</select>
	{:else if field.type === 'version_ref'}
		<select {id} {disabled} value={str(value)} onchange={(e) => set(e.currentTarget.value || null)}>
			<option value="">（沒有里程碑）</option>
			{#each versions as v (v.id)}<option value={v.id} disabled={v.status !== 'open'}>{v.name}{v.effective_date ? `（${v.effective_date}）` : ''}{v.status !== 'open' ? '・不可選' : ''}</option>{/each}
			{#if value && !versions.some((v) => v.id === value)}<option value={str(value)}>（目前的值）</option>{/if}
		</select>
	{:else if field.options}
		<select {id} {disabled} value={str(value)} onchange={(e) => set(e.currentTarget.value || null)}>
			<option value="">（空）</option>
			{#each field.options as o (o.id)}<option value={o.id} disabled={!o.active && o.id !== value}>{o.label}{o.active ? '' : '（已停用）'}</option>{/each}
		</select>
	{:else}
		<input {id} {disabled} type="text" value={str(value)} oninput={(e) => set(e.currentTarget.value)} />
	{/if}
</label>

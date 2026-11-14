<script lang="ts">
	// Fields in the sections of a form layout (contract 05 §6.14, §3.6.1):
	// columns, spans, help, and `show_if` evaluated as the person types
	// (§9 B-10; only a hint, the server decides). Fields the layout does not
	// place go last under 「其他」.
	import type { Assignee, FieldSchema, VersionView } from '$api/types';
	import { evalWhen, type LayoutLike } from '#lib/formcond.js';
	import FieldInput, { type MdOpts } from './FieldInput.svelte';

	let {
		fields,
		layout,
		form = $bindable(),
		values,
		assignees = [],
		versions = [],
		disabled = false,
		marks = new Set<string>(),
		md = null
	}: {
		fields: { f: FieldSchema; required: boolean }[];
		layout: LayoutLike | null | undefined;
		form: Record<string, unknown>;
		/** Condition values (formcond `condValues`) of the form as it is now. */
		values: Record<string, unknown>;
		assignees?: Assignee[];
		versions?: VersionView[];
		disabled?: boolean;
		/** Keys to flag (a form rule the issue breaks). */
		marks?: Set<string>;
		/** The description's Markdown editor options (FieldInput `md`). */
		md?: MdOpts | null;
	} = $props();

	type Placed = { e: { f: FieldSchema; required: boolean }; span: number; help: string | null };
	const sections = $derived.by(() => {
		const byKey = new Map(fields.map((e) => [e.f.key, e]));
		const used = new Set<string>();
		const out: { id: string; title: string; columns: number; fields: Placed[] }[] = [];
		for (const s of layout?.sections ?? []) {
			if (s.show_if && !evalWhen(s.show_if, values)) {
				for (const f of s.fields) used.add(f.key);
				continue;
			}
			const cols = Math.min(3, Math.max(1, s.columns ?? 1));
			const placed: Placed[] = [];
			for (const f of s.fields) {
				const e = byKey.get(f.key);
				if (!e || used.has(f.key)) continue;
				used.add(f.key);
				if (f.show_if && !evalWhen(f.show_if, values)) continue;
				placed.push({ e, span: Math.min(cols, Math.max(1, f.span ?? 1)), help: f.help ?? null });
			}
			if (placed.length) out.push({ id: s.id, title: s.title, columns: cols, fields: placed });
		}
		const rest = fields.filter((e) => !used.has(e.f.key)).map((e) => ({ e, span: 1, help: null }));
		if (rest.length) {
			const other = out.find((s) => s.id === '_other');
			if (other) other.fields.push(...rest);
			else out.push({ id: '_rest', title: layout?.sections?.length ? '其他' : '', columns: 1, fields: rest });
		}
		return out;
	});
</script>

{#each sections as s (s.id)}
	<section class="fsec" data-testid="form-section" data-section={s.id}>
		{#if s.title}<h4>{s.title}</h4>{/if}
		<div class="fgrid" style="grid-template-columns:repeat({s.columns},minmax(0,1fr))">
			{#each s.fields as p (p.e.f.key)}
				<div class="fcell" class:marked={marks.has(p.e.f.key)} style="grid-column:span {p.span}" data-testid="form-field" data-key={p.e.f.key}>
					<FieldInput field={p.e.f} bind:value={form[p.e.f.key]} {assignees} {versions} required={p.e.required} {disabled} {md} />
					{#if p.help}<div class="hint">{p.help}</div>{/if}
				</div>
			{/each}
		</div>
	</section>
{/each}

<style>
	.fsec {
		margin-bottom: 8px;
	}
	.fsec h4 {
		margin: 6px 0 4px;
		font-size: 12.5px;
		color: var(--dim);
		font-weight: 600;
	}
	.fgrid {
		display: grid;
		gap: 4px 12px;
	}
	.fcell {
		min-width: 0;
	}
	.fcell.marked {
		outline: 1px solid var(--warn);
		outline-offset: 2px;
		border-radius: 6px;
	}
	@media (max-width: 640px) {
		.fgrid {
			grid-template-columns: 1fr !important;
		}
		.fcell {
			grid-column: auto !important;
		}
	}
</style>

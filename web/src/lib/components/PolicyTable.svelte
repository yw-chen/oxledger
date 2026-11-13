<script lang="ts">
	// One delegation's policy table (contract 05 §6.18 ④, 02 §16.3): every
	// command of the ceiling with direct／review／off, and the fields of
	// 修改欄位 underneath. `onpick` decides what a click does (the wizard edits
	// in place; the detail page saves); `canPick` greys out what this viewer
	// may not choose (a project manager may only make it stricter).
	import type { PolicyMode } from '$api/types';
	import { MODE_LABELS } from '$lib/format';
	import { commandLabel, MODES, type Policies } from '$lib/aiteam';

	let {
		policies,
		fieldName,
		onpick,
		canPick = () => true,
		disabled = false,
		name
	}: {
		policies: Policies;
		fieldName: (key: string) => string;
		onpick: (kind: 'commands' | 'fields', key: string, mode: PolicyMode) => void;
		canPick?: (from: PolicyMode, to: PolicyMode) => boolean;
		disabled?: boolean;
		/** Unique per table on the page (radio groups). */
		name: string;
	} = $props();
	let showFields = $state(false);
	const fields = $derived(Object.keys(policies.fields));
</script>

<table class="t" data-testid="policy-table">
	<thead><tr><th>操作</th>{#each MODES as m (m)}<th>{MODE_LABELS[m]}</th>{/each}</tr></thead>
	<tbody>
		{#each Object.entries(policies.commands) as [op, cur] (op)}
			<tr data-testid="policy-row" data-op={op}>
				<td>
					{commandLabel(op)}
					{#if op === 'update_fields' && fields.length}<button type="button" class="btn sm ghost" onclick={() => (showFields = !showFields)} data-testid="policy-fields">{showFields ? '收合欄位' : '逐欄設定'}</button>{/if}
				</td>
				{#each MODES as m (m)}
					<td><input type="radio" name="{name}-c-{op}" checked={cur === m} disabled={disabled || (cur !== m && !canPick(cur, m))} onchange={() => onpick('commands', op, m)} aria-label="{commandLabel(op)}：{MODE_LABELS[m]}" /></td>
				{/each}
			</tr>
			{#if op === 'update_fields' && showFields && cur !== 'off'}
				{#each fields as f (f)}
					{@const fm = policies.fields[f]}
					<tr class="hint">
						<td style="padding-left:18px">└ {fieldName(f)}</td>
						{#each MODES as m (m)}
							<td><input type="radio" name="{name}-f-{f}" checked={fm === m} disabled={disabled || (fm !== m && !canPick(fm, m))} onchange={() => onpick('fields', f, m)} aria-label="{fieldName(f)}：{MODE_LABELS[m]}" /></td>
						{/each}
					</tr>
				{/each}
			{/if}
		{/each}
	</tbody>
</table>

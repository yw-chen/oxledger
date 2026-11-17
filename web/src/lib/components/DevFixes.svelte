<script lang="ts">
	// Design 06 §6.8 修正連結: what a `fixes AUTH-1` in a merged PR or a
	// pushed commit did to this issue. `applied`: the rule moved it;
	// `suggested`/`skipped`: why nothing moved, and a way to move it by hand
	// with the viewer's own permissions.
	import type { DevFix } from '$api/types';
	import { fixSource, fixText } from '#lib/fixes.js';
	import { fmtTime } from '#lib/format.js';

	let {
		fixes,
		canTransition = false,
		ontransition
	}: {
		fixes: DevFix[];
		/** The viewer has a transition to offer (the ordinary dialog). */
		canTransition?: boolean;
		/** Opens the transition dialog; the rule's target status, when known. */
		ontransition?: (target: string | null) => void;
	} = $props();

	const shown = $derived(fixes.map((f) => ({ f, t: fixText(f) })).filter((x) => x.t !== null));
</script>

{#if shown.length}
	<div class="fixes" data-testid="dev-fixes">
		{#each shown as { f, t }, i (i)}
			{#if t?.kind === 'applied'}
				<div class="line" data-testid="dev-fix-applied">
					<span class="chip c-ok" title="{f.sponsor ? `${f.sponsor} 的規則・` : ''}{fmtTime(f.at)}">{t.text}</span>
					<span class="hint">{fixSource(f)}</span>
				</div>
			{:else if t}
				<div class="line" data-testid="dev-fix-hint">
					<span class="chip c-warn">{fixSource(f)} {t.text}</span>
					{#if canTransition}<button type="button" class="btn sm" onclick={() => ontransition?.(f.target_status_id ?? null)} data-testid="dev-fix-transition">轉換狀態</button>{/if}
				</div>
			{/if}
		{/each}
	</div>
{/if}

<style>
	.fixes {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 8px;
	}
	.line {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-wrap: wrap;
	}
	.chip {
		white-space: normal;
	}
</style>

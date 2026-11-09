<script lang="ts">
	// One error, said the §15.5 way: what happened, whether data changed, next step.
	import { describeError, type Shown } from '$lib/errors';
	let { error, onaction }: { error: unknown; onaction?: (a: NonNullable<Shown['action']>) => void } = $props();
	const shown = $derived(describeError(error));
</script>

{#if error}
	<div class="err" role="alert" data-testid="error">
		<b>{shown.title}</b>
		<div class="chg">{shown.changed}</div>
		{#if shown.next && shown.action && onaction}
			<button class="btn sm" style="margin-top:6px" onclick={() => onaction?.(shown.action!)}>{shown.next}</button>
		{:else if shown.next}
			<div class="hint">下一步：{shown.next}</div>
		{/if}
	</div>
{/if}

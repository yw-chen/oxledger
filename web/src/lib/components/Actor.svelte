<script lang="ts">
	// People and agents side by side (§16.2): an agent shows its handle with an
	// 〔agent〕 mark; "on behalf of" goes in the tooltip.
	let { name, kind = 'user', approver = null }: { name: string; kind?: string; approver?: string | null } = $props();
</script>

{#if kind === 'agent'}
	<span class="agname" title="agent（代表委派者操作）">{name}</span><span class="agent-label" style="margin-left:4px">agent</span>
{:else}
	<span>{name}</span>
{/if}
{#if approver}<span class="dim">· {approver} 核准</span>{/if}

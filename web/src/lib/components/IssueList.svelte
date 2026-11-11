<script lang="ts">
	// A compact list of search hits (search results are read with the caller's
	// visibility; there is never a total, §4.5 / Q-S6).
	import type { SearchItem } from '$api/types';
	let { items, compact = false }: { items: SearchItem[]; compact?: boolean } = $props();
</script>

{#if !items.length}
	<div class="empty">沒有符合的議題</div>
{:else}
	<table class="t" data-testid="issue-list">
		{#if !compact}
			<thead><tr><th>單號</th><th>標題</th><th>狀態</th><th>指派</th></tr></thead>
		{/if}
		<tbody>
			{#each items as it (it.key)}
				<tr>
					<td><a class="key" href="/issue/{it.key}">{it.key}</a></td>
					<td>
						{it.subject ?? ''}
						{#if it.is_private}<span class="chip c-warn">私有</span>{/if}
					</td>
					<td class="muted" style="white-space:nowrap">{it.status ?? ''}</td>
					{#if !compact}<td class="muted">{it.assignee ?? '—'}</td>{/if}
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

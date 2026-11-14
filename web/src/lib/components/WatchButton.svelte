<script lang="ts">
	// 關注 (contract 05 §6.16): optimistic toggle; on failure the old state
	// comes back with a toast. People only (agents never watch).
	import type { WatchInfo } from '$api/types';
	import { api } from '#lib/api.js';
	import { toast } from '#lib/toast.svelte.js';
	import { describeError } from '#lib/errors.js';

	let { issueKey, watch }: { issueKey: string; watch: WatchInfo } = $props();
	// svelte-ignore state_referenced_locally
	let w = $state<WatchInfo>({ ...watch });
	$effect(() => {
		w = { ...watch };
	});
	let busy = $state(false);
	let hover = $state(false);

	const AUTO: Record<string, string> = { author: '你是這張單的作者，所以自動關注', assignee: '這張單指派給你，所以自動關注', note: '你在這張單留過言，所以自動關注' };
	const title = $derived(w.watching ? (w.auto_reason ? (AUTO[w.auto_reason] ?? '自動關注') : '取消關注') : w.opted_out ? '你已取消關注，不會再自動關注這張單' : '有更新時通知我');
	const count = $derived(w.watcher_count_capped ? '100+ 人關注' : `${w.watcher_count} 人關注`);

	async function toggle() {
		const before = { ...w };
		const on = !w.watching;
		w = { ...w, watching: on, opted_out: !on, auto_reason: on ? null : w.auto_reason, watcher_count: Math.max(0, w.watcher_count + (on ? 1 : -1)) };
		busy = true;
		try {
			const r = on ? await api('PUT /issues/{key}/watch', { path: { key: issueKey }, body: {} as never }) : await api('DELETE /issues/{key}/watch', { path: { key: issueKey } });
			w = { ...w, watching: r.watching, watcher_count: r.watcher_count, watcher_count_capped: r.watcher_count_capped };
		} catch (e) {
			w = before;
			toast(`${on ? '關注' : '取消關注'}失敗：${describeError(e).title}`, 'bad');
		} finally {
			busy = false;
		}
	}
</script>

<span class="watch">
	<button class="btn sm" class:on={w.watching} onclick={toggle} disabled={busy} {title} aria-pressed={w.watching} onmouseenter={() => (hover = true)} onmouseleave={() => (hover = false)} data-testid="watch">
		👁 {w.watching ? (hover ? '取消關注' : '關注中 ✓') : '關注'}
	</button>
	<span class="hint" data-testid="watch-count">{count}</span>
</span>

<style>
	.watch { display: inline-flex; align-items: center; gap: 6px; }
</style>

<script lang="ts">
	// 通知中心 (contract 05 §6.15): 未讀／全部, project filter, mark all read,
	// one row per issue; J/K move, Enter opens, E marks read.
	import { goto } from '$app/navigation';
	import { untrack } from 'svelte';
	import type { NotificationItem } from '$api/types';
	import { api, newOperationId } from '$lib/api';
	import { ago, budgetLine, eventLine, moreCount, reasonLabels } from '$lib/notify';
	import { refreshUnread, unread } from '$lib/notifs.svelte';
	import { session } from '$lib/session.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	let filter = $state<'unread' | 'all'>('unread');
	let project = $state('');
	let items = $state<NotificationItem[] | null>(null);
	let next = $state<string | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	let sel = $state(0);
	/** read-all marks only what was there when the list loaded (§6.15). */
	let before = $state<string | null>(null);
	let seq = 0;

	async function load(more = false) {
		const s = ++seq;
		error = null;
		try {
			const r = await api('GET /notifications', { query: { filter, project: project || undefined, cursor: more ? next : undefined, limit: 30 } });
			if (s !== seq) return;
			items = more && items ? [...items, ...r.items] : r.items;
			next = r.next_cursor;
			if (!more) {
				before = r.items[0]?.last_at ?? null;
				sel = 0;
			}
		} catch (e) {
			if (s === seq) error = e;
		}
	}
	$effect(() => {
		void filter;
		void project;
		items = null;
		load();
	});
	// 「查看全部」 from the bell while already here: read again.
	$effect(() => {
		if (unread.rev) untrack(() => load());
	});

	async function setRead(n: NotificationItem, read: boolean) {
		error = null;
		try {
			const r = await api(read ? 'POST /notifications/{id}/read' : 'POST /notifications/{id}/unread', { path: { id: n.id }, operationId: newOperationId(), body: {} as never });
			if (r.merged_into) {
				// Joined another unread row of the same issue: reload to show it once.
				await load();
			} else if (filter === 'unread' && read) {
				items = items?.filter((x) => x.id !== n.id) ?? null;
				sel = Math.min(sel, Math.max(0, (items?.length ?? 1) - 1));
			} else n.read = read;
			refreshUnread();
		} catch (e) {
			error = e;
		}
	}
	function open(n: NotificationItem) {
		if (!n.read) setRead(n, true);
		goto(n.kind === 'ai_budget' ? '/admin/ai' : `/issue/${n.issue?.key}`);
	}
	async function readAll() {
		if (!before) return;
		busy = true;
		error = null;
		try {
			await api('POST /notifications/read-all', { operationId: newOperationId(), body: { before } });
			toast('已標為已讀', 'ok');
			refreshUnread();
			await load();
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	function keys(e: KeyboardEvent) {
		const t = e.target as HTMLElement;
		if (e.ctrlKey || e.metaKey || e.altKey || t.closest('input,select,textarea,[contenteditable]') || !items?.length) return;
		const k = e.key.toLowerCase();
		if (k === 'j') sel = Math.min(sel + 1, items.length - 1);
		else if (k === 'k') sel = Math.max(sel - 1, 0);
		else if (k === 'enter' && !t.closest('button,a')) open(items[sel]);
		else if (k === 'e') setRead(items[sel], true);
		else return;
		e.preventDefault();
		document.querySelector(`[data-row="${sel}"]`)?.scrollIntoView({ block: 'nearest' });
	}
</script>

<svelte:window onkeydown={keys} />
<svelte:head><title>通知・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px;flex-wrap:wrap;margin-bottom:10px">
		<h1 style="margin:0">通知</h1>
		<div class="tabs" role="tablist">
			<button role="tab" class:on={filter === 'unread'} aria-selected={filter === 'unread'} onclick={() => (filter = 'unread')} data-testid="nt-unread">未讀</button>
			<button role="tab" class:on={filter === 'all'} aria-selected={filter === 'all'} onclick={() => (filter = 'all')} data-testid="nt-all">全部</button>
		</div>
		<select bind:value={project} aria-label="專案" data-testid="nt-project">
			<option value="">所有專案</option>
			{#each session.projects as p (p.id)}<option value={p.key}>{p.key} {p.name}</option>{/each}
		</select>
		<span style="flex:1"></span>
		<button class="btn sm" onclick={readAll} disabled={busy || !before || !!project} title={project ? '「全部標為已讀」會標掉所有專案的通知；請先選「所有專案」' : undefined} data-testid="nt-read-all">全部標為已讀</button>
		<a class="btn sm ghost" href="/me/notifications" title="通知偏好" aria-label="通知偏好" data-testid="nt-prefs">⚙</a>
	</div>
	<ErrorBox {error} onaction={() => load()} />
	{#if items}
		<div class="nlist" data-testid="nt-list">
			{#each items as n, i (n.id)}
				<div class="nrow" class:unread={!n.read} class:sel={i === sel} data-row={i} data-testid="nt-row">
					<span class="dot" aria-hidden="true">{n.read ? '' : '●'}</span>
					<button class="nmain" onclick={() => open(n)} onfocus={() => (sel = i)}>
						{#if n.kind === 'ai_budget'}
							<span class="t1">{budgetLine(n)}</span>
						{:else if n.issue}
							<span class="t1"><span class="mono">{n.issue.key}</span> {n.issue.subject}{#if !n.read}<span class="sr">（未讀）</span>{/if}</span>
							<span class="t2">{#each reasonLabels(n.reasons) as r (r)}<span class="chip c-dim">{r}</span>{/each}</span>
							{#if n.events[0]}<span class="t3" data-testid="nt-event">{eventLine(n.events[0])}</span>{/if}
							{#if moreCount(n)}<span class="hint">還有 {moreCount(n)} 則更新</span>{/if}
						{/if}
					</button>
					<span class="ntime" title={n.last_at}>{ago(n.last_at)}</span>
					<button class="btn sm ghost nact" onclick={() => setRead(n, !n.read)} data-testid="nt-toggle">{n.read ? '標為未讀' : '標為已讀'}</button>
				</div>
			{:else}
				{#if filter === 'unread'}
					<div class="empty" data-testid="nt-empty">沒有未讀通知。<button class="btn sm" onclick={() => (filter = 'all')}>查看全部</button></div>
				{:else}
					<div class="empty" data-testid="nt-empty">還沒有通知。關注議題後，有更新時會出現在這裡。</div>
				{/if}
			{/each}
		</div>
		{#if next}<button class="btn sm" style="margin-top:8px" onclick={() => load(true)}>載入更多</button>{/if}
		<p class="hint">鍵盤：J／K 上下、Enter 開啟、E 標為已讀。</p>
	{:else if !error}
		<div class="sync"><span class="spin"></span>載入中</div>
	{/if}
</div>

<style>
	.tabs { display: flex; gap: 2px; }
	select { width: auto; }
	.tabs button { background: none; border: 1px solid var(--line); padding: 3px 12px; border-radius: 6px; }
	.tabs button.on { background: var(--act-a); border-color: var(--act); }
	.nlist { border: 1px solid var(--line); border-radius: 10px; overflow: hidden; }
	.nrow { display: flex; align-items: flex-start; gap: 8px; padding: 9px 12px; border-bottom: 1px solid var(--line); }
	.nrow:last-child { border-bottom: 0; }
	.nrow.sel { background: var(--card2); }
	.dot { width: 12px; color: var(--act); font-size: 11px; line-height: 22px; }
	.nmain { flex: 1; min-width: 0; display: flex; flex-direction: column; align-items: flex-start; gap: 2px; text-align: left; background: none; border: 0; padding: 0; }
	.t1 { font-size: 14px; }
	.unread .t1 { font-weight: 700; }
	.t2 { display: flex; gap: 4px; }
	.t3 { font-size: 13px; color: var(--muted); }
	.ntime { font-size: 12px; color: var(--dim); white-space: nowrap; }
	.nact { opacity: 0; white-space: nowrap; }
	.nrow:hover .nact, .nrow.sel .nact, .nact:focus-visible { opacity: 1; }
	@media (hover: none) { .nact { opacity: 1; } }
	@media (max-width: 600px) { .nrow { flex-wrap: wrap; } .nact { margin-left: 20px; } }
	.sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); }
</style>

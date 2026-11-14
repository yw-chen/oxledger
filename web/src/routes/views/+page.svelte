<script lang="ts">
	// My views (contract 05 §3.3): the built-in ones, my personal views and
	// the project views of the projects I can see. 「＋ 新視圖」 makes a
	// personal list or board.
	import type { ViewSummary } from '$api/types';
	import { api } from '$lib/api';
	import { goto } from '$app/navigation';
	import { session } from '$lib/session.svelte';
	import { fmtShort } from '$lib/format';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	let items = $state<ViewSummary[]>([]);
	let error = $state<unknown>(null);
	let loading = $state(true);
	/** §6: no loading sign in the first 400 ms, then 8 skeleton rows. */
	let skeleton = $state(false);
	let seq = 0;
	async function load() {
		const n = ++seq;
		loading = true;
		error = null;
		const t = setTimeout(() => n === seq && (skeleton = true), 400);
		try {
			const mine = await api('GET /views', { query: { scope: 'all' } });
			const perProject = await Promise.all(session.projects.map((p) => api('GET /views', { query: { scope: 'all', project: p.key } }).then((r) => r.items).catch(() => [])));
			if (n !== seq) return; // a newer load (projects arrived) wins
			const seen = new Set<string>();
			items = [...mine.items, ...perProject.flat()].filter((v) => !seen.has(v.id) && !!seen.add(v.id));
		} catch (e) {
			if (n === seq) error = e;
		} finally {
			clearTimeout(t);
			if (n === seq) {
				loading = false;
				skeleton = false;
			}
		}
	}
	$effect(() => {
		void session.projects.length;
		load();
	});
	const SECTIONS = [
		{ scope: 'builtin', title: '內建' },
		{ scope: 'personal', title: '個人' },
		{ scope: 'project', title: '專案' }
	];
	const KIND: Record<string, string> = { list: '列表', board: '看板' };

	let creating = $state<{ name: string; kind: string; project: string } | null>(null);
	let createError = $state<unknown>(null);
	let busy = $state(false);
	async function create() {
		if (!creating) return;
		busy = true;
		createError = null;
		const p = session.projects.find((x) => x.key === creating!.project);
		const def =
			creating.kind === 'board'
				? { kind: 'board', query: { where: null, q: null }, lanes: { by: 'status_category', items: [ { id: 'todo', title: '待處理', values: ['todo'] }, { id: 'doing', title: '進行中', values: ['doing'] }, { id: 'done', title: '完成', values: ['done'] } ], show_empty: true }, card_fields: ['key', 'subject', 'assignee', 'priority', 'due_date'], done_window_days: 14 }
				: { kind: 'list', query: { where: { field: 'status_category', op: 'in', value: ['todo', 'doing'] }, q: null }, columns: ['key', 'subject', 'status', 'assignee', 'priority', 'due_date'], sort: [{ field: 'updated_at', dir: 'desc' }] };
		try {
			const v = await api('POST /views', { body: { scope: 'personal', kind: creating.kind, name: creating.name.trim(), def, project_id: p?.id ?? null } });
			creating = null;
			goto(`/views/${encodeURIComponent(v.id)}`);
		} catch (e) {
			createError = e;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>我的視圖・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px">
		<h1 style="margin:0">我的視圖</h1>
		<span style="flex:1"></span>
		<button class="btn pri" onclick={() => (creating = { name: '', kind: 'list', project: '' })} data-testid="new-view">＋ 新視圖</button>
	</div>
	<ErrorBox {error} onaction={load} />
	{#if loading && !items.length}
		{#if skeleton}
			<div data-testid="views-skeleton" aria-busy="true" aria-label="載入中">
				{#each { length: 8 } as _, i (i)}<span class="skel" style="width:{40 + ((i * 29) % 45)}%;margin:10px 0"></span>{/each}
			</div>
		{/if}
	{:else}
		{#each SECTIONS as s (s.scope)}
			{@const list = items.filter((v) => v.scope === s.scope)}
			<h2>{s.title}</h2>
			{#each list as v (v.id)}
				<div class="row" style="gap:8px;padding:4px 0;border-bottom:1px solid var(--line)" data-testid="view-item">
					<a href={v.scope === 'project' && v.project ? `/project/${v.project.key}/views/${encodeURIComponent(v.id)}` : `/views/${encodeURIComponent(v.id)}`}>{v.name}</a>
					<span class="chip c-dim">{KIND[v.kind] ?? v.kind}</span>
					{#if v.project}<span class="mono hint">{v.project.key}</span>{/if}
					{#if v.project_unavailable}<span class="chip c-warn" title="綁定的專案你已經看不到">專案已無法存取</span>{/if}
					<span style="flex:1"></span>
					{#if v.updated_at}<span class="hint">{v.updated_by?.name ?? ''} {fmtShort(v.updated_at)}</span>{/if}
				</div>
			{:else}
				<div class="empty">{s.scope === 'personal' ? '還沒有個人視圖。' : s.scope === 'project' ? '你看得到的專案還沒有專案視圖。' : '—'}</div>
			{/each}
		{/each}
	{/if}
</div>

{#if creating}
	<div class="modal-bg" role="presentation">
		<div class="modal" role="dialog" aria-modal="true" aria-label="新視圖" data-testid="new-view-form">
			<h3>新視圖</h3>
			<label class="f"><span>名稱</span><input type="text" bind:value={creating.name} maxlength="60" data-testid="nv-name" /></label>
			<label class="f" style="margin-top:6px"><span>種類</span>
				<select bind:value={creating.kind} data-testid="nv-kind"><option value="list">列表</option><option value="board">看板</option></select>
			</label>
			<label class="f" style="margin-top:6px"><span>專案</span>
				<select bind:value={creating.project} data-testid="nv-project">
					<option value="">跨專案</option>
					{#each session.projects as p (p.id)}<option value={p.key}>{p.key}・{p.name}</option>{/each}
				</select>
			</label>
			<div class="hint">個人視圖只有你看得到。要給專案成員用，建立後從「⋯」複製成專案視圖。</div>
			<ErrorBox error={createError} />
			<div class="row" style="gap:8px;margin-top:10px">
				<button class="btn pri" onclick={create} disabled={busy || !creating.name.trim()} data-testid="nv-submit">{busy ? '建立中…' : '建立'}</button>
				<button class="btn" onclick={() => ((creating = null), (createError = null))}>取消</button>
			</div>
		</div>
	</div>
{/if}

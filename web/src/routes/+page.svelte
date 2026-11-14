<script lang="ts">
	// 首頁 (contract 05 §6.12): the widgets saved on the server (`/me/home`),
	// a 3-column grid; 「編輯首頁」 reorders (drag or ↑↓), widens 1–3, removes
	// and adds. 「需要你決定」 cannot be removed (U22).
	import type { HomeView, ViewSummary, Widget } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { ADDABLE, homeProblem, MAX_WIDGETS, moveTo, newWidget, PROTECTED, toInput, WIDGET_LABEL, widgetTitle } from '#lib/home.js';
	import { me } from '#lib/me.svelte.js';
	import { session } from '#lib/session.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import HomeWidget from '#lib/components/HomeWidget.svelte';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	let home = $state<HomeView | null>(null);
	let error = $state<unknown>(null);
	let editing = $state(false);
	let draft = $state<Widget[]>([]);
	let saveError = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		try {
			home = await api('GET /me/home');
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void me.homeRev;
		load();
	});

	function edit() {
		draft = JSON.parse(JSON.stringify(home?.widgets ?? []));
		saveError = null;
		editing = true;
		loadViews();
	}
	async function save() {
		if (!home) return;
		busy = true;
		saveError = null;
		try {
			home = await api('PUT /me/home', { operationId: newOperationId(), body: { lock_version: home.lock_version, widgets: draft.map(toInput) } });
			editing = false;
			toast('首頁已儲存', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') {
				await load();
				editing = false;
				toast('首頁在別處被修改過，已載入最新版本', 'w');
			} else saveError = e;
		} finally {
			busy = false;
		}
	}

	// ---- editing ----
	let dragFrom = $state<number | null>(null);
	let dragOver = $state<number | null>(null);
	function drop(i: number) {
		if (dragFrom !== null) draft = moveTo(draft, dragFrom, i);
		dragFrom = dragOver = null;
	}
	const remove = (i: number) => (draft = draft.filter((_, k) => k !== i));

	let adding = $state('');
	let addView = $state('');
	let addProject = $state('');
	let views = $state<ViewSummary[]>([]);
	async function loadViews() {
		try {
			views = (await api('GET /views', { query: { limit: 100 } })).items;
		} catch {
			views = [];
		}
	}
	function add() {
		if (!adding) return;
		const extra: Partial<Widget> = {};
		if (adding === 'view') {
			const v = views.find((x) => x.id === addView);
			if (!v) return;
			Object.assign(extra, { view_id: v.id, view_name: v.name, view_kind: v.kind });
		}
		if (adding === 'milestones' && addProject) {
			const p = session.projects.find((x) => x.id === addProject);
			Object.assign(extra, { project_id: addProject, project_key: p?.key, project_name: p?.name });
		}
		draft = [...draft, newWidget(adding, extra)];
		adding = addView = addProject = '';
	}
	const problem = $derived(editing ? homeProblem(draft) : '');
	const shown = $derived(editing ? draft : (home?.widgets ?? []));
</script>

<svelte:head><title>首頁・oxledger</title></svelte:head>
<div class="page wide">
	<div class="row" style="margin-bottom:12px">
		<h1>{session.user?.name}，你好</h1>
		<span class="sp" style="flex:1"></span>
		{#if editing}
			<button class="btn sm" onclick={() => (editing = false)} disabled={busy} data-testid="home-cancel">取消</button>
			<button class="btn sm pri" onclick={save} disabled={busy || !!problem} data-testid="home-save">儲存</button>
		{:else}
			<button class="btn sm" onclick={edit} disabled={!home} data-testid="edit-home">編輯首頁</button>
		{/if}
	</div>
	<ErrorBox {error} onaction={load} />
	{#if editing}
		<div class="card" data-testid="home-editor">
			<div class="row" style="gap:6px;flex-wrap:wrap">
				<b>＋ 小工具</b>
				<select bind:value={adding} disabled={draft.length >= MAX_WIDGETS} aria-label="小工具類型" data-testid="home-add-type">
					<option value="">選類型…</option>
					{#each ADDABLE as t (t)}<option value={t}>{WIDGET_LABEL[t]}</option>{/each}
				</select>
				{#if adding === 'view'}
					<select bind:value={addView} aria-label="視圖" data-testid="home-add-view">
						<option value="">選視圖…</option>
						{#each views as v (v.id)}<option value={v.id}>{v.name}{v.project ? `（${v.project.key}）` : ''}</option>{/each}
					</select>
				{:else if adding === 'milestones'}
					<select bind:value={addProject} aria-label="專案" data-testid="home-add-project">
						<option value="">所有專案</option>
						{#each session.projects as p (p.id)}<option value={p.id}>{p.key} {p.name}</option>{/each}
					</select>
				{/if}
				<button class="btn sm" onclick={add} disabled={!adding || (adding === 'view' && !addView) || draft.length >= MAX_WIDGETS} data-testid="home-add">加入</button>
				<span class="hint">{draft.length}／{MAX_WIDGETS}・拖拉或用 ↑↓ 排序</span>
			</div>
			{#if problem}<div class="banner w" data-testid="home-problem">{problem}</div>{/if}
			<ErrorBox error={saveError} />
		</div>
	{/if}
	{#if home}
		<div class="hgrid" data-testid="home-grid">
			{#each shown as w, i (w.id)}
				<section
					class="widget"
					class:editing
					class:over={editing && dragOver === i && dragFrom !== i}
					style="grid-column: span {Math.min(3, Math.max(1, w.width))}"
					draggable={editing}
					ondragstart={() => (dragFrom = i)}
					ondragover={(e) => {
						if (!editing) return;
						e.preventDefault();
						dragOver = i;
					}}
					ondragend={() => (dragFrom = dragOver = null)}
					ondrop={(e) => {
						e.preventDefault();
						drop(i);
					}}
					aria-label={widgetTitle(w)}
					data-testid="widget"
					data-type={w.type}
				>
					<div class="wh">
						<b>{widgetTitle(w)}</b>
						<span style="flex:1"></span>
						{#if editing}
							<button class="btn sm" title="往前" aria-label="往前" onclick={() => (draft = moveTo(draft, i, i - 1))} disabled={i === 0} data-testid="w-up">↑</button>
							<button class="btn sm" title="往後" aria-label="往後" onclick={() => (draft = moveTo(draft, i, i + 1))} disabled={i === draft.length - 1} data-testid="w-down">↓</button>
							<select bind:value={draft[i].width} aria-label="寬度" data-testid="w-width">
								<option value={1}>1 欄</option><option value={2}>2 欄</option><option value={3}>3 欄</option>
							</select>
							{#if w.type === 'view'}
								<input type="number" min="3" max="20" bind:value={draft[i].limit} aria-label="列數" title="列幾筆（3–20）" style="width:56px" data-testid="w-limit" />
							{/if}
							{#if w.type === PROTECTED}
								<span class="hint" title="這個區塊不能移除" data-testid="w-protected">🔒</span>
							{:else}
								<button class="btn sm bad" onclick={() => remove(i)} data-testid="w-remove">移除</button>
							{/if}
						{/if}
					</div>
					{#if editing}
						<div class="hint">{WIDGET_LABEL[w.type] ?? w.type}{w.type === 'view' ? `・${w.limit ?? 8} 筆` : ''}</div>
					{:else}
						<HomeWidget {w} />
					{/if}
				</section>
			{/each}
		</div>
	{:else if !error}
		<div class="sync"><span class="spin"></span>載入中</div>
	{/if}
</div>

<style>
	.hgrid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; }
	@media (max-width: 1024px) { .hgrid { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
	@media (max-width: 700px) { .hgrid { grid-template-columns: minmax(0, 1fr); } .hgrid > :global(*) { grid-column: auto !important; } }
	.widget.editing { cursor: grab; }
	.wh b { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; }
	.widget select, .widget input, .card select { width: auto; }
	.widget input[type=number] { background: var(--bg2); border: 1px solid var(--line2); border-radius: 7px; padding: 4px 6px; }
	.widget .wh { flex-wrap: wrap; }
	.widget.over { outline: 2px dashed var(--act); }
</style>

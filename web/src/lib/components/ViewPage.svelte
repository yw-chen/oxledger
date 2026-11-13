<script lang="ts">
	// One view (contract 05 §6.1, §6.3): title with its scope, the condition
	// chips, the search box (override.q), then the list or the board. Changes
	// to conditions or sorting are a draft run through /views/preview until
	// saved (「條件已修改，尚未儲存」). Built-in views are copied before editing.
	import type { FilterFields, FilterOption, RunInput, ViewDetail, ViewEvent } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { goto } from '$app/navigation';
	import { chips, fromGroup, toGroup, type Group, type Where } from '$lib/filter';
	import { sourceOptions } from '$lib/filterOptions';
	import { fmtTime } from '$lib/format';
	import { toast } from '$lib/toast.svelte';
	import { session } from '$lib/session.svelte';
	import ErrorBox from './ErrorBox.svelte';
	import FilterEditor from './FilterEditor.svelte';
	import ViewTable from './ViewTable.svelte';
	import Board from './Board.svelte';
	import BoardSettings from './BoardSettings.svelte';

	let { viewId, projectKey = null, inFrame = false }: { viewId: string; projectKey?: string | null; inFrame?: boolean } = $props();

	type Def = Record<string, unknown> & { kind: string; query?: { where?: unknown; q?: string | null }; sort?: { field: string; dir: string }[] | null; lanes?: { by: string } };
	let view = $state<ViewDetail | null>(null);
	let def = $state<Def | null>(null);
	let loadError = $state<unknown>(null);
	let ff = $state<FilterFields | null>(null);
	let q = $state('');
	let qApplied = $state('');
	let editing = $state(false);
	let settings = $state(false);
	let saveError = $state<unknown>(null);
	let conflict = $state<ViewDetail | null>(null);
	let badField = $state<string | null>(null);
	let saving = $state(false);
	let menu = $state(false);

	const saved = $derived(view ? JSON.stringify(view.def) : '');
	const dirty = $derived(!!def && JSON.stringify(def) !== saved);
	const scopeLabel: Record<string, string> = { personal: '個人', project: '專案', builtin: '內建' };
	const pkey = $derived(view?.project?.key ?? projectKey);
	const pid = $derived(view?.project?.id ?? null);

	async function load(id: string) {
		loadError = null;
		view = null;
		def = null;
		conflict = null;
		try {
			const v = await api('GET /views/{id}', { path: { id } });
			view = v;
			def = structuredClone(v.def) as Def;
			ff = await api('GET /filter-fields', { query: { project: v.project?.key ?? projectKey ?? undefined } }).catch(() => null);
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		load(viewId);
	});
	// Search box: 300 ms after typing.
	$effect(() => {
		const v = q;
		const t = setTimeout(() => (qApplied = v.trim()), 300);
		return () => clearTimeout(t);
	});

	// Names for chips: options of the fields used.
	let names = $state<Record<string, FilterOption[]>>({});
	$effect(() => {
		const where = def?.query?.where;
		if (!ff || !where) return;
		const used = new Set<string>();
		const walk = (w: unknown) => {
			if (!w || typeof w !== 'object') return;
			const o = w as Record<string, unknown>;
			if (Array.isArray(o.and)) o.and.forEach(walk);
			else if (Array.isArray(o.or)) o.or.forEach(walk);
			else if (typeof o.field === 'string') used.add(o.field);
		};
		walk(where);
		for (const f of ff.fields) if (used.has(f.field) && !f.options?.length && f.options_source && !names[f.field]) {
			names[f.field] = [];
			sourceOptions(f).then((o) => (names[f.field] = o));
		}
	});
	const label = (field: string, id: string) => names[field]?.find((o) => o.id === id)?.label ?? (field === 'project' ? session.projects.find((p) => p.id === id)?.key : undefined);
	const group = $derived(toGroup(def?.query?.where));
	const chipList = $derived(ff ? chips(group, ff.fields, ff.today, label, ff.week_start) : []);
	const statusCategory = $derived(Object.fromEntries((ff?.fields.find((f) => f.field === 'status')?.options ?? []).map((o) => [o.id, o.category ?? ''])));

	const runKey = $derived(JSON.stringify({ d: dirty ? def : null, q: qApplied, id: view?.id }));
	async function run(i: RunInput) {
		const override = qApplied ? { q: qApplied } : null;
		if (dirty && def) {
			return api('POST /views/preview', { body: { ...i, def, override, scope: view?.scope === 'project' ? 'project' : 'personal', project_id: pid } });
		}
		return api('POST /views/{id}/run', { path: { id: view!.id }, body: { ...i, override } });
	}
	async function previewWhere(w: Where) {
		if (!def) return { titles: [] };
		const d = { ...def, kind: 'list', columns: ['key', 'subject'], query: { ...(def.query ?? {}), where: w } } as Record<string, unknown>;
		delete d.lanes;
		delete d.swimlanes;
		delete d.card_fields;
		delete d.done_window_days;
		delete d.group_by;
		const r = await api('POST /views/preview', { body: { def: d, limit: 5, scope: view?.scope === 'project' ? 'project' : 'personal', project_id: pid } });
		return { titles: (r.items ?? []).map((x) => `${x.key} ${String(x.values?.subject ?? '')}`) };
	}
	function applyWhere(g: Group) {
		if (!def) return;
		def.query = { ...(def.query ?? {}), where: fromGroup(g) };
		editing = false;
		badField = null;
	}
	function clearWhere() {
		if (!def) return;
		// Project views keep their project condition (the server adds it anyway).
		def.query = { ...(def.query ?? {}), where: null };
	}
	function sortBy(field: string) {
		if (!def) return;
		const cur = def.sort?.[0];
		def.sort = [{ field, dir: cur?.field === field && cur.dir === 'desc' ? 'asc' : 'desc' }];
	}
	function discard() {
		if (view) def = structuredClone($state.snapshot(view.def)) as Def;
		conflict = null;
		saveError = null;
		badField = null;
	}
	function onSaveError(e: unknown) {
		saveError = e;
		if (e instanceof ApiError && e.code === 'invalid_view') {
			const d = e.details as { reason?: string; path?: string };
			if (d.reason === 'restricted_field' || d.reason === 'unknown_field') {
				const m = /(cf:\d+)/.exec(JSON.stringify(e.details));
				badField = m?.[1] ?? null;
				editing = true;
			}
		}
	}
	async function save(force = false) {
		if (!view || !def) return;
		saving = true;
		saveError = null;
		try {
			let lock = view.lock_version ?? 0;
			if (force && conflict) lock = conflict.lock_version ?? lock;
			const v = await api('PATCH /views/{id}', { path: { id: view.id }, body: { lock_version: lock, def } });
			view = v;
			def = structuredClone(v.def) as Def;
			conflict = null;
			toast('已儲存視圖', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') {
				conflict = ((e.details as { current?: ViewDetail }).current ?? null) as ViewDetail | null;
				if (!conflict) conflict = await api('GET /views/{id}', { path: { id: view.id } }).catch(() => null);
			} else onSaveError(e);
		} finally {
			saving = false;
		}
	}
	function takeLatest() {
		if (!conflict) return;
		view = conflict;
		def = structuredClone($state.snapshot(conflict.def)) as Def;
		conflict = null;
	}

	// Save as / copy.
	let saveAs = $state<{ name: string; scope: string; mode: 'draft' | 'copy' } | null>(null);
	function openSaveAs(mode: 'draft' | 'copy', scope = 'personal') {
		menu = false;
		saveError = null;
		saveAs = { name: `${view?.name ?? '視圖'}（副本）`, scope, mode };
	}
	async function doSaveAs() {
		if (!view || !def || !saveAs) return;
		saving = true;
		saveError = null;
		try {
			const projectId = pid ?? (projectKey ? session.projects.find((p) => p.key === projectKey)?.id : null) ?? null;
			const v =
				saveAs.mode === 'copy' && !dirty
					? await api('POST /views/{id}/copy', { path: { id: view.id }, body: { name: saveAs.name, scope: saveAs.scope, project_id: projectId } })
					: await api('POST /views', { body: { name: saveAs.name, scope: saveAs.scope, kind: def.kind, def, project_id: projectId } });
			saveAs = null;
			toast('已建立視圖', 'ok');
			goto(viewHref(v.id, v.project?.key ?? null, v.scope === 'project'));
		} catch (e) {
			onSaveError(e);
		} finally {
			saving = false;
		}
	}
	function viewHref(id: string, key: string | null, project = false) {
		return (inFrame || project) && key ? `/project/${key}/views/${encodeURIComponent(id)}` : `/views/${encodeURIComponent(id)}`;
	}
	/** A conflict on rename / delete: show the latest version, keep the person's input (02 §15.4). */
	async function latestAfterConflict(e: unknown): Promise<boolean> {
		if (!(e instanceof ApiError && e.code === 'conflict') || !view) return false;
		const cur = ((e.details as { current?: ViewDetail })?.current ?? (await api('GET /views/{id}', { path: { id: view.id } }).catch(() => null))) as ViewDetail | null;
		if (cur) {
			// Only the header (name, version) moves to the latest; an unsaved
			// draft stays the person's.
			view = dirty ? { ...cur, def: view.def } : cur;
			if (!dirty) def = structuredClone($state.snapshot(cur.def)) as Def;
		}
		toast(`這個視圖剛被${cur?.updated_by?.name ?? '別人'}改過，已載入最新版本，請再試一次。`, 'bad');
		return true;
	}
	async function remove() {
		menu = false;
		if (!view || !confirm(`刪除視圖「${view.name}」？`)) return;
		try {
			await api('DELETE /views/{id}', { path: { id: view.id }, query: { lock_version: view.lock_version ?? 0 } });
			toast('已刪除視圖', 'ok');
			goto(inFrame && pkey ? `/project/${pkey}` : '/views');
		} catch (e) {
			if (!(await latestAfterConflict(e))) saveError = e;
		}
	}
	let rename = $state<string | null>(null);
	async function doRename() {
		if (!view || !rename?.trim()) return;
		try {
			const v = await api('PATCH /views/{id}', { path: { id: view.id }, body: { lock_version: view.lock_version ?? 0, name: rename.trim() } });
			// Keep an unsaved draft: only the name and version come from the server.
			view = dirty ? { ...v, def: view.def } : v;
			rename = null;
		} catch (e) {
			// The typed name stays in the box for another try.
			if (!(await latestAfterConflict(e))) saveError = e;
		}
	}
	let history = $state<ViewEvent[] | null>(null);
	async function openHistory() {
		menu = false;
		try {
			history = (await api('GET /views/{id}/events', { path: { id: view!.id } })).items;
		} catch (e) {
			saveError = e;
		}
	}
	const fieldUnavailable = (e: unknown) => e instanceof ApiError && e.code === 'invalid_filter' && (e.details as { reason?: string }).reason === 'field_unavailable';
	let runError = $state<unknown>(null);
	const EV: Record<string, string> = { created: '建立', updated: '修改', renamed: '改名', deleted: '刪除', copied: '複製' };
</script>

{#if loadError}
	{#if loadError instanceof ApiError && loadError.code === 'not_found'}
		<div class="empty" data-testid="view-missing">找不到這個視圖，或你沒有權限查看。<a class="btn sm" href="/views">回到我的視圖</a></div>
	{:else}
		<ErrorBox error={loadError} onaction={() => load(viewId)} />
	{/if}
{:else if !view || !def}
	<div class="sync" style="opacity:.6"><span class="spin"></span></div>
{:else}
	<div class="row" style="gap:8px;margin:4px 0 8px;flex-wrap:wrap">
		<h2 style="margin:0" data-testid="view-name">{view.name}</h2>
		<span class="scope-badge" data-testid="view-scope">{scopeLabel[view.scope] ?? view.scope}</span>
		{#if view.scope === 'project'}<span class="hint">所有成員都看得到</span>{/if}
		<span style="flex:1"></span>
		{#if def.kind === 'board'}
			<button class="btn sm" onclick={() => (settings = true)} data-testid="board-settings">⚙ 看板設定</button>
		{/if}
		<button class="btn sm" onclick={() => (editing = true)} data-testid="edit-conditions">編輯條件</button>
		<div class="menu">
			<button class="btn sm" onclick={() => (menu = !menu)} aria-haspopup="true" aria-expanded={menu} aria-label="更多" data-testid="view-menu">⋯</button>
			{#if menu}
				<div class="mlist" role="menu">
					<button role="menuitem" disabled={!view.editable} title={view.editable ? '' : '內建視圖不能改，可以複製'} onclick={() => ((menu = false), (rename = view!.name))}>改名稱</button>
					<button role="menuitem" onclick={() => openSaveAs('copy', 'personal')} data-testid="copy-personal">複製成個人視圖</button>
					{#if pkey}<button role="menuitem" onclick={() => openSaveAs('copy', 'project')} data-testid="copy-project">複製成專案視圖</button>{/if}
					<button role="menuitem" disabled={view.scope === 'builtin'} onclick={openHistory}>歷史</button>
					<button role="menuitem" disabled={!view.editable} title={view.editable ? '' : '你不能刪除這個視圖'} onclick={remove} data-testid="view-delete">刪除</button>
				</div>
			{/if}
		</div>
	</div>

	<div class="row" style="gap:6px;flex-wrap:wrap;margin-bottom:8px" data-testid="chips">
		{#each chipList as c, i (i)}<button class="fchip" onclick={() => (editing = true)}>{c}</button>{:else}<span class="hint">沒有條件：列出所有你看得到的議題。</span>{/each}
		<span style="flex:1"></span>
		<input type="search" bind:value={q} placeholder="在這個視圖裡搜尋" aria-label="在這個視圖裡搜尋" style="width:220px" data-testid="view-q" />
	</div>

	{#if conflict}
		<div class="banner w" data-testid="view-conflict">
			這個視圖剛被{conflict.updated_by?.name ?? '別人'}改過{conflict.updated_at ? `（${fmtTime(conflict.updated_at)}）` : ''}，你的修改還沒存，也還在畫面上。{#if conflict.name !== view.name}最新名稱：「{conflict.name}」。{/if}
			<button class="btn sm" onclick={takeLatest}>放棄我的修改，載入最新</button>
			{#if view.editable}<button class="btn sm" onclick={() => save(true)}>用我的版本覆蓋</button>{/if}
		</div>
	{:else if dirty}
		<div class="banner i" data-testid="view-dirty">
			條件已修改，尚未儲存。
			{#if view.editable}<button class="btn sm pri" onclick={() => save()} disabled={saving} data-testid="view-save">{saving ? '儲存中…' : '儲存'}</button>{/if}
			<button class="btn sm" onclick={() => openSaveAs('draft', view?.scope === 'project' ? 'project' : 'personal')} data-testid="view-save-as">另存為新視圖</button>
			<button class="btn sm" onclick={discard} data-testid="view-discard">捨棄修改</button>
		</div>
	{/if}
	{#if fieldUnavailable(runError)}
		<div class="banner w">這個視圖有條件用到你讀不到的欄位，無法執行。{#if view.editable}<button class="btn sm" onclick={() => (editing = true)}>編輯條件</button>{/if}</div>
	{/if}
	<ErrorBox error={saveError} />

	{#if def.kind === 'board'}
		<Board {run} reloadKey={runKey} onerror={(e) => (runError = e)} fieldType={ff?.fields.find((f) => f.field === String(def!.lanes?.by))?.type} doneDays={Number(def.done_window_days ?? 14)} by={String(def.lanes?.by ?? 'status_category')} {statusCategory} canSettings={view.editable || view.scope === 'builtin'} onsettings={() => (settings = true)} canCreate createHref={pkey ? `/new?project=${pkey}` : '/new'} />
	{:else}
		<ViewTable {run} reloadKey={runKey} sort={def.sort ?? []} sortable={ff?.sortable ?? []} onsort={sortBy} onclear={def.query?.where ? clearWhere : undefined} onerror={(e) => (runError = e)} />
	{/if}

	{#if editing && ff}
		<FilterEditor fields={ff.fields} today={ff.today} weekStart={ff.week_start} initial={group} projectView={view.scope === 'project'} {badField} preview={previewWhere} onapply={applyWhere} oncancel={() => (editing = false)} />
	{/if}
	{#if settings && def.kind === 'board'}
		<BoardSettings
			{def}
			statuses={ff?.fields.find((f) => f.field === 'status')?.options ?? []}
			builtin={view.scope === 'builtin'}
			onapply={(d) => {
				def = d as Def;
				settings = false;
			}}
			oncopy={() => {
				settings = false;
				openSaveAs('copy', 'project');
			}}
			oncancel={() => (settings = false)}
		/>
	{/if}
{/if}

{#if saveAs}
	<div class="modal-bg" role="presentation">
		<div class="modal" role="dialog" aria-modal="true" aria-label="另存為新視圖" data-testid="save-as">
			<h3>{saveAs.mode === 'copy' ? '複製視圖' : '另存為新視圖'}</h3>
			<label class="f"><span>名稱</span><input type="text" bind:value={saveAs.name} maxlength="60" data-testid="save-as-name" /></label>
			<label class="f" style="margin-top:6px"
				><span>給誰看</span>
				<select bind:value={saveAs.scope} data-testid="save-as-scope">
					<option value="personal">只有我（個人視圖）</option>
					{#if pkey}<option value="project">{pkey} 的所有成員（專案視圖）</option>{/if}
				</select>
			</label>
			{#if saveAs.scope === 'project'}<div class="hint">專案視圖的名稱與條件所有成員都看得到；不能使用受限欄位；需要「管理公開視圖」權限。</div>{/if}
			<ErrorBox error={saveError} />
			<div class="row" style="gap:8px;margin-top:10px">
				<button class="btn pri" onclick={doSaveAs} disabled={saving || !saveAs.name.trim()} data-testid="save-as-submit">{saving ? '建立中…' : '建立'}</button>
				<button class="btn" onclick={() => ((saveAs = null), (saveError = null))}>取消</button>
			</div>
		</div>
	</div>
{/if}

{#if rename !== null}
	<div class="modal-bg" role="presentation">
		<div class="modal" role="dialog" aria-modal="true" aria-label="改名稱">
			<h3>改名稱</h3>
			<input type="text" bind:value={rename} maxlength="60" style="width:100%" />
			<ErrorBox error={saveError} />
			<div class="row" style="gap:8px;margin-top:10px">
				<button class="btn pri" onclick={doRename} disabled={!rename.trim()}>儲存</button>
				<button class="btn" onclick={() => ((rename = null), (saveError = null))}>取消</button>
			</div>
		</div>
	</div>
{/if}

{#if history}
	<div class="modal-bg" role="presentation" onclick={() => (history = null)}>
		<div class="modal" role="dialog" aria-modal="true" aria-label="視圖歷史" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.key === 'Escape' && (history = null)} tabindex="-1">
			<h3>歷史</h3>
			{#each history as ev, i (i)}
				<div class="ev"><b>{EV[ev.kind] ?? ev.kind}</b>・{ev.actor?.name ?? '—'}<span class="dim">・{fmtTime(ev.at)}</span></div>
			{:else}<div class="empty">沒有紀錄。</div>{/each}
			<button class="btn" style="margin-top:8px" onclick={() => (history = null)}>關閉</button>
		</div>
	</div>
{/if}

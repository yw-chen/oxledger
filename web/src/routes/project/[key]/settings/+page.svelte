<script lang="ts">
	// 專案設定 (§18.6, §17.12, §19.4): shared tabs (whole-list replace with
	// lock_version, or reset), milestone-health thresholds (inherit the org
	// default or override), and GitHub repo links. The server checks
	// manage_project / manage_versions; this page only hides what it can.
	import { page } from '$app/state';
	import type { LinkableRepos, RepoLinks, ScheduleSettings, TabInput, TabSettings, ViewSummary } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { projectByKey } from '$lib/session.svelte';
	import { fmtShort } from '$lib/format';
	import { toast } from '$lib/toast.svelte';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	const projectId = $derived(projectByKey(key)?.id ?? key);
	let reload = $state(0);

	// ---- tabs ----
	type Row = TabInput & { id?: string | null; builtin: boolean; uid: number };
	let tabs = $state<TabSettings | null>(null);
	let rows = $state<Row[]>([]);
	let defaultTab = $state('');
	let tabError = $state<unknown>(null);
	let tabBusy = $state(false);
	// Someone else saved the tabs meanwhile: the person's rows stay on screen
	// until they choose to drop them or save over the newer version.
	let tabConflict = $state(false);
	let uid = 0;
	function adopt(t: TabSettings) {
		tabs = t;
		rows = t.tabs.map((x) => ({ id: x.id, kind: x.kind, title: x.title, query: x.query ?? null, url: x.url ?? null, view_id: x.view_id ?? null, hidden: x.hidden, builtin: x.kind === 'builtin', uid: ++uid }));
		defaultTab = t.default_tab;
		tabConflict = false;
	}
	// Project views of this project, for 「視圖」 tabs and the board tab.
	let projectViews = $state<ViewSummary[]>([]);
	async function loadViews() {
		try {
			const r = await api('GET /views', { query: { scope: 'all', project: key } });
			projectViews = r.items.filter((v) => v.scope === 'project');
		} catch {
			projectViews = [];
		}
	}
	async function loadTabs() {
		tabError = null;
		loadViews();
		try {
			adopt(await api('GET /projects/{id}/tabs', { path: { id: projectId } }));
		} catch (e) {
			tabError = e;
		}
	}
	function move(i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= rows.length) return;
		const r = [...rows];
		[r[i], r[j]] = [r[j], r[i]];
		rows = r;
	}
	function add(kind: 'query' | 'link' | 'view') {
		const first = projectViews.find((v) => v.kind === 'list') ?? projectViews[0];
		rows = [...rows, { id: null, kind, title: kind === 'query' ? '新查詢' : kind === 'view' ? (first?.name ?? '新視圖') : '新連結', query: kind === 'query' ? `project:${key} ` : null, url: kind === 'link' ? 'https://' : null, view_id: kind === 'view' ? (first?.id ?? null) : null, hidden: false, builtin: false, uid: ++uid }];
	}
	function remove(i: number) {
		rows = rows.filter((_, k) => k !== i);
	}
	async function saveTabs(overwrite = false) {
		if (!tabs) return;
		tabBusy = true;
		tabError = null;
		try {
			const lock = overwrite ? (await api('GET /projects/{id}/tabs', { path: { id: projectId } })).lock_version : tabs.lock_version;
			const body = { lock_version: lock, default_tab: defaultTab, tabs: rows.map((r) => ({ id: r.id || undefined, kind: r.kind, title: r.title.trim(), query: r.kind === 'query' ? r.query : undefined, url: r.kind === 'link' ? r.url : undefined, view_id: r.kind === 'view' || r.id === 'board' ? r.view_id || undefined : undefined, hidden: r.hidden })) };
			adopt(await api('PUT /projects/{id}/tabs', { path: { id: projectId }, body }));
			reload++;
			toast('已儲存分頁', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') tabConflict = true;
			else tabError = e;
		} finally {
			tabBusy = false;
		}
	}
	async function resetTabs() {
		if (!tabs || !confirm('把分頁恢復成預設？自訂的查詢與連結分頁會被移除。')) return;
		tabBusy = true;
		tabError = null;
		try {
			adopt(await api('POST /projects/{id}/tabs/reset', { path: { id: projectId }, body: { lock_version: tabs.lock_version } }));
			reload++;
			toast('已恢復預設分頁', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') tabConflict = true;
			else tabError = e;
		} finally {
			tabBusy = false;
		}
	}
	const defaultChoices = $derived(rows.filter((r) => r.id && !r.hidden && r.kind !== 'link'));
	// Hiding the default tab leaves no valid default: move it to the first
	// remaining choice so the select never shows blank (R-T3).
	$effect(() => {
		if (defaultChoices.length && !defaultChoices.some((r) => r.id === defaultTab)) defaultTab = defaultChoices[0].id!;
	});
	const canTabs = $derived(!!tabs?.can_manage);
	const host = (u: string | null | undefined) => {
		try {
			return u ? new URL(u).host : '';
		} catch {
			return '';
		}
	};

	// ---- health thresholds ----
	let sched = $state<ScheduleSettings | null>(null);
	let inherit = $state(true);
	let days = $state<number | null>(null);
	let pct = $state<number | null>(null);
	let schedError = $state<unknown>(null);
	let schedConflict = $state(false);
	// manage_versions (server-decided; the flag only hides controls).
	let schedReadOnly = $state(false);
	function adoptSched(s: ScheduleSettings) {
		sched = s;
		inherit = s.project.inherit_org;
		// Project override: the stored values as they are (empty = that rule
		// is off); inheriting: start from the org values.
		days = s.project.inherit_org ? s.org.health_at_risk_days : s.project.health_at_risk_days;
		pct = s.project.inherit_org ? s.org.health_at_risk_open_pct : s.project.health_at_risk_open_pct;
		schedConflict = false;
		schedReadOnly = !s.can_manage;
	}
	const blank = (v: unknown) => v === null || v === undefined || v === '';
	const schedInvalid = $derived(
		inherit ? '' : !blank(days) && !(Number(days) >= 0 && Number(days) <= 90 && Number.isInteger(Number(days))) ? '天數要是 0–90 的整數' : !blank(pct) && !(Number(pct) >= 0 && Number(pct) <= 100 && Number.isInteger(Number(pct))) ? '比例要是 0–100 的整數' : ''
	);
	async function loadSched() {
		schedError = null;
		try {
			adoptSched(await api('GET /projects/{id}/schedule-settings', { path: { id: projectId } }));
		} catch (e) {
			schedError = e;
		}
	}
	async function saveSched() {
		if (!sched) return;
		schedError = null;
		try {
			adoptSched(
				await api('PUT /projects/{id}/schedule-settings', {
					path: { id: projectId },
					body: inherit
						? { lock_version: sched.lock_version, inherit_org: true }
						: { lock_version: sched.lock_version, inherit_org: false, health_at_risk_days: blank(days) ? null : Number(days), health_at_risk_open_pct: blank(pct) ? null : Number(pct) }
				})
			);
			toast('已儲存健康度門檻', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') schedConflict = true;
			else {
				schedError = e;
				if (e instanceof ApiError && e.code === 'forbidden_op') schedReadOnly = true;
			}
		}
	}

	// ---- repos ----
	let repos = $state<RepoLinks | null>(null);
	let linkable = $state<LinkableRepos | null>(null);
	let pick = $state('');
	let repoError = $state<unknown>(null);
	async function loadRepos() {
		repoError = null;
		try {
			repos = await api('GET /projects/{id}/repos', { path: { id: projectId } });
			linkable = repos.can_manage ? await api('GET /projects/{id}/repos/linkable', { path: { id: projectId } }) : null;
		} catch (e) {
			repoError = e;
		}
	}
	async function link() {
		if (!pick) return;
		repoError = null;
		try {
			const r = await api('POST /projects/{id}/repos', { path: { id: projectId }, body: { repo_id: pick } });
			toast(`已連動 ${r.full_name}，正在回補歷史資料`, 'ok');
			pick = '';
			await loadRepos();
		} catch (e) {
			repoError = e;
		}
	}
	async function unlink(linkId: string, name: string) {
		if (!confirm(`解除 ${name} 的連動？已同步的 PR 資料會從這個專案的畫面移除。`)) return;
		repoError = null;
		try {
			await api('DELETE /projects/{id}/repos/{link_id}', { path: { id: projectId, link_id: linkId } });
			toast(`已解除 ${name}`, 'ok');
			await loadRepos();
		} catch (e) {
			repoError = e;
		}
	}
	const SYNC: Record<string, string> = { backfilling: '回補中', ok: '同步中', paused: '暫停（GitHub 限流）' };

	$effect(() => {
		void projectId;
		loadTabs().then(() => {
			if (!tabs) return; // unknown project: one error, not three
			loadSched();
			loadRepos();
		});
	});
</script>

<svelte:head><title>{key}・專案設定・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active={null} {reload}>
	{#if !tabs && tabError}
		<ErrorBox error={tabError} onaction={loadTabs} />
	{:else}
	<h2>分頁</h2>
	<ErrorBox error={tabError} onaction={loadTabs} />
	{#if tabs && !tabs.can_manage}<div class="banner i">你沒有管理這個專案的權限，只能檢視。</div>{/if}
	{#if tabConflict}
		<div class="banner w" data-testid="tab-conflict">
			分頁設定在你開啟之後被別人改過，你的修改還沒存。
			<span class="sp"></span>
			<button class="btn sm" onclick={loadTabs}>放棄我的修改，載入最新</button>
			<button class="btn sm" onclick={() => saveTabs(true)} disabled={tabBusy}>用我的版本覆蓋</button>
		</div>
	{/if}
	{#if tabs}
		<div class="hint">{tabs.is_default ? '目前是預設分頁。' : '已自訂。'}分頁是整個專案共用的：所有成員都看得到標題、查詢條件與連結，不要放私有資訊。查詢分頁用每個人自己的權限執行。</div>
		<table class="t" data-testid="tab-table">
			<thead><tr><th></th><th>標題</th><th>種類</th><th>內容</th><th>隱藏</th><th></th></tr></thead>
			<tbody>
				{#each rows as r, i (r.uid)}
					<tr data-testid="tab-row">
						<td style="white-space:nowrap"><button class="btn sm ghost" onclick={() => move(i, -1)} disabled={!canTabs || i === 0} aria-label="上移">↑</button><button class="btn sm ghost" onclick={() => move(i, 1)} disabled={!canTabs || i === rows.length - 1} aria-label="下移">↓</button></td>
						<td><input bind:value={r.title} maxlength="40" aria-label="標題" disabled={!canTabs} /></td>
						<td class="hint">{r.builtin ? '內建' : r.kind === 'query' ? '查詢' : r.kind === 'view' ? '視圖' : '連結'}</td>
						<td>
							{#if r.kind === 'query'}<input class="mono" style="width:100%" bind:value={r.query} aria-label="查詢條件" disabled={!canTabs} />
							{:else if r.kind === 'link'}<input style="width:100%" bind:value={r.url} aria-label="網址" disabled={!canTabs} />{#if host(r.url)}<div class="hint">開啟 {host(r.url)}</div>{/if}
							{:else if r.kind === 'view'}
								<select bind:value={r.view_id} aria-label="視圖" disabled={!canTabs} data-testid="tab-view">
									{#if !r.view_id}<option value={null}>（選一個專案視圖）</option>{/if}
									{#each projectViews as v (v.id)}<option value={v.id}>{v.name}（{v.kind === 'board' ? '看板' : '列表'}）</option>{/each}
								</select>
							{:else if r.id === 'board'}
								<select bind:value={r.view_id} aria-label="看板視圖" disabled={!canTabs} data-testid="tab-board-view">
									<option value={null}>內建看板</option>
									{#each projectViews.filter((v) => v.kind === 'board') as v (v.id)}<option value={v.id}>{v.name}</option>{/each}
								</select>
							{:else}<span class="hint">—</span>{/if}
						</td>
						<td><input type="checkbox" bind:checked={r.hidden} aria-label="隱藏" disabled={!canTabs} /></td>
						<td>{#if !r.builtin && canTabs}<button class="btn sm ghost" onclick={() => remove(i)}>移除</button>{/if}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		<div class="row" style="gap:8px;margin-top:6px;flex-wrap:wrap">
			<button class="btn sm" onclick={() => add('query')} disabled={!tabs.can_manage}>＋查詢分頁</button>
			<button class="btn sm" onclick={() => add('link')} disabled={!tabs.can_manage}>＋連結分頁</button>
			<button class="btn sm" onclick={() => add('view')} disabled={!tabs.can_manage || !projectViews.length} title={projectViews.length ? '' : '這個專案還沒有專案視圖；從視圖的「⋯」複製成專案視圖'} data-testid="add-view-tab">＋視圖分頁</button>
			<label class="row" style="gap:4px">預設分頁
				<select bind:value={defaultTab} disabled={!canTabs}>{#each defaultChoices as r (r.uid)}<option value={r.id}>{r.title}</option>{/each}</select>
			</label>
			<span style="flex:1"></span>
			<button class="btn" onclick={resetTabs} disabled={tabBusy || !tabs.can_manage || tabs.is_default}>恢復預設</button>
			<button class="btn pri" onclick={() => saveTabs()} disabled={tabBusy || !tabs.can_manage} data-testid="save-tabs">儲存分頁</button>
		</div>
		<div class="hint">新加的分頁要先儲存，才能設為預設分頁。</div>
	{/if}

	<h2 style="margin-top:22px">里程碑健康度門檻</h2>
	<ErrorBox error={schedError} onaction={loadSched} />
	{#if schedConflict}
		<div class="banner w" data-testid="health-conflict">
			門檻在你開啟之後被別人改過，你的修改還沒存。
			<span class="sp"></span><button class="btn sm" onclick={loadSched}>載入最新（放棄我的修改）</button>
		</div>
	{/if}
	{#if sched}
		<div class="card" data-testid="health-settings">
			{#if schedReadOnly}<div class="hint">要有「管理里程碑」權限才能修改。</div>{/if}
			<label class="row" style="gap:4px"><input type="checkbox" bind:checked={inherit} disabled={schedReadOnly} /> 沿用組織預設（{sched.org.health_at_risk_days ?? '—'} 天內、未完成超過 {sched.org.health_at_risk_open_pct ?? '—'}%）</label>
			{#if !inherit}
				<div class="row" style="gap:8px;margin-top:6px">
					<label class="f" for="h-days"><span>距離里程碑幾天內</span><input id="h-days" type="number" min="0" max="90" step="1" bind:value={days} disabled={schedReadOnly} /></label>
					<label class="f" for="h-pct"><span>未完成比例超過（%）</span><input id="h-pct" type="number" min="0" max="100" step="1" bind:value={pct} disabled={schedReadOnly} /></label>
				</div>
			{/if}
			<div class="hint">符合這兩個條件的里程碑會標成「有風險」。只看日期與單的狀態，不用 AI 判斷。{#if !inherit}留空表示不檢查那一項。{/if}</div>
			{#if schedInvalid}<div class="banner w" data-testid="health-invalid">{schedInvalid}</div>{/if}
			{#if !schedReadOnly}<button class="btn pri" style="margin-top:6px" onclick={saveSched} disabled={!!schedInvalid} data-testid="save-health">儲存門檻</button>{/if}
		</div>
	{/if}

	<h2 style="margin-top:22px">GitHub repo 連動</h2>
	<ErrorBox error={repoError} onaction={loadRepos} />
	{#if repos}
		{#each repos.items as r (r.link_id)}
			<div class="card row" style="gap:8px" data-testid="repo-link">
				<b>{r.full_name}</b>
				{#if r.is_private}<span class="chip c-dim">私有</span>{/if}
				<span class="chip {r.sync_state === 'ok' ? 'c-ok' : 'c-warn'}">{SYNC[r.sync_state] ?? r.sync_state}</span>
				{#if r.repo_state !== 'accessible'}<span class="chip c-bad">App 已無法讀取・資料停在 {r.synced_at ? fmtShort(r.synced_at) : '—'}</span>{/if}
				{#if r.paused_until}<span class="hint">{fmtShort(r.paused_until)} 恢復</span>{/if}
				<span style="flex:1"></span>
				<span class="hint">連動於 {fmtShort(r.linked_at)}</span>
				{#if repos.can_manage}<button class="btn sm ghost" onclick={() => unlink(r.link_id, r.full_name)}>解除</button>{/if}
			</div>
		{:else}
			<div class="empty">這個專案還沒連動 repo。</div>
		{/each}
		{#if repos.can_manage && linkable}
			<div class="row" style="gap:8px;margin-top:6px">
				<select bind:value={pick} aria-label="可連動的 repo">
					<option value="">選一個 repo…</option>
					{#each linkable.items as l (l.repo_id)}<option value={l.repo_id}>{l.full_name}{l.is_private ? '（私有）' : ''}</option>{/each}
				</select>
				<button class="btn pri" onclick={link} disabled={!pick} data-testid="link-repo">連動</button>
			</div>
			{#if !linkable.items.length}<div class="hint">沒有其他可連動的 repo：要先由系統管理者在「GitHub 連動」頁把 repo 設成可連動。</div>{/if}
		{/if}
	{/if}
	{/if}
</ProjectFrame>

<script lang="ts">
	// 偏好 (contract 05 §6.12): interface roles (then 「套用新預設／保留我的」),
	// 以角色預覽 (the template, read with your own permissions), theme,
	// density and the order of the left-nav shortcuts.
	import type { ShortcutInput, Template, ViewSummary, Widget } from '$api/types';
	import { api, newOperationId } from '$lib/api';
	import { adoptPrefs, loadMe, me, ROLE_LABEL, ROLES, setPrefs, SHORTCUT_LABEL } from '$lib/me.svelte';
	import { DENSITY_LABEL, THEME_LABEL, THEMES, type Density, type Theme } from '$lib/prefs.svelte';
	import { BUILTIN_VIEW, moveTo, widgetTitle } from '$lib/home';
	import { toast } from '$lib/toast.svelte';
	import HomeWidget from '$lib/components/HomeWidget.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	$effect(() => {
		loadMe();
		api('GET /views', { query: { limit: 100 } })
			.then((l) => (views = l.items))
			.catch(() => (views = []));
	});
	const p = $derived(me.prefs);

	// ---- interface roles ----
	let askApply = $state<string[] | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	async function toggleRole(r: string, on: boolean) {
		if (!p) return;
		const roles = ROLES.filter((x) => (x === r ? on : p.interface_roles.includes(x)));
		if (await setPrefs({ interface_roles: roles })) askApply = roles;
		else redraw();
	}
	async function apply(roles: string[]) {
		busy = true;
		error = null;
		try {
			const r = await api('POST /me/home/apply-template', { operationId: newOperationId(), body: { roles, mode: 'replace' } });
			adoptPrefs(r.preferences);
			me.homeRev++;
			askApply = null;
			shortcuts = null;
			toast('已套用新的預設首頁與捷徑', 'ok');
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}

	/** A failed save leaves the checkbox or radio as clicked: draw them again from the saved settings. */
	let rev = $state(0);
	const redraw = () => rev++;
	async function setOrRedraw(set: Parameters<typeof setPrefs>[0]) {
		if (!(await setPrefs(set))) redraw();
	}

	// ---- preview as a role ----
	let previewRole = $state('');
	let preview = $state<Template | null>(null);
	let previewError = $state<unknown>(null);
	$effect(() => {
		const r = previewRole;
		preview = null;
		previewError = null;
		if (!r) return;
		api('GET /home/templates', { query: { roles: r } })
			.then((t) => {
				if (previewRole === r) preview = t;
			})
			.catch((e) => (previewError = e));
	});

	// ---- shortcuts ----
	let views = $state<ViewSummary[]>([]);
	let shortcuts = $state<ShortcutInput[] | null>(null);
	let scKind = $state('');
	let scView = $state('');
	$effect(() => {
		if (p && shortcuts === null) shortcuts = p.nav_shortcuts.map((s) => ({ kind: s.kind, id: s.id ?? null, name: s.name ?? null }));
	});
	const scDirty = $derived(!!p && !!shortcuts && JSON.stringify(shortcuts.map((s) => [s.kind, s.id ?? null])) !== JSON.stringify(p.nav_shortcuts.map((s) => [s.kind, s.id ?? null])));
	const scName = (s: ShortcutInput) => (s.kind === 'view' ? (s.name ?? views.find((v) => v.id === s.id)?.name ?? '視圖') : (SHORTCUT_LABEL[s.kind] ?? s.kind));
	function addShortcut() {
		if (!shortcuts || !scKind) return;
		if (scKind === 'view') {
			const v = views.find((x) => x.id === scView);
			if (!v) return;
			shortcuts = [...shortcuts, { kind: 'view', id: v.id, name: v.name }];
		} else shortcuts = [...shortcuts, { kind: scKind, id: null }];
		scKind = scView = '';
	}
	async function saveShortcuts() {
		if (!shortcuts) return;
		if (await setPrefs({ nav_shortcuts: shortcuts.map((s) => ({ kind: s.kind, id: s.id ?? null })) })) {
			shortcuts = null;
			toast('左欄捷徑已儲存', 'ok');
		} else shortcuts = null;
	}
	const used = $derived(new Set((shortcuts ?? []).filter((s) => s.kind !== 'view').map((s) => s.kind)));
</script>

<svelte:head><title>偏好・oxledger</title></svelte:head>
<div class="page">
	<div class="tabs" role="tablist" style="margin-bottom:10px">
		<a role="tab" class="on" aria-selected="true" href="/me/preferences">偏好</a>
		<a role="tab" aria-selected="false" href="/me/notifications">通知</a>
	</div>
	<h1>偏好</h1>
	{#if !p}
		<div class="sync"><span class="spin"></span>載入中</div>
	{:else}
		<h2>介面角色</h2>
		<div class="card" data-testid="pf-roles">
			{#key rev}<div class="row" style="gap:14px">
				{#each ROLES as r (r)}
					<label class="row" style="gap:4px"><input type="checkbox" checked={p.interface_roles.includes(r)} onchange={(e) => toggleRole(r, e.currentTarget.checked)} data-testid="pf-role-{r}" />{ROLE_LABEL[r]}</label>
				{/each}
			</div>{/key}
			<div class="hint">只影響預設的首頁和捷徑，不影響權限。</div>
			{#if askApply}
				<div class="banner i" data-testid="pf-apply-ask">
					要依新的角色換成預設的首頁和捷徑嗎？
					<span class="sp"></span>
					<button class="btn sm pri" onclick={() => apply(askApply!)} disabled={busy} data-testid="pf-apply">套用新預設</button>
					<button class="btn sm" onclick={() => (askApply = null)} data-testid="pf-keep">保留我的</button>
				</div>
			{/if}
			<ErrorBox {error} />
		</div>

		<h2>以角色預覽</h2>
		<div class="card">
			<label class="row" style="gap:6px">角色
				<select bind:value={previewRole} data-testid="pf-preview-role">
					<option value="">（不預覽）</option>
					{#each ROLES as r (r)}<option value={r}>{ROLE_LABEL[r]}</option>{/each}
				</select>
			</label>
			<ErrorBox error={previewError} />
			{#if preview}
				<div class="banner i" style="margin-top:8px" data-testid="pf-preview-banner">預覽：{ROLE_LABEL[previewRole]}的預設首頁（用你自己的權限顯示）</div>
				<div class="hint">左欄捷徑：{preview.nav_shortcuts.map((s) => (s.kind === 'view' ? (BUILTIN_VIEW[s.id ?? ''] ?? '視圖') : (SHORTCUT_LABEL[s.kind] ?? s.kind))).join('、') || '—'}</div>
				<div class="pgrid" data-testid="pf-preview">
					{#each preview.widgets as w (w.id)}
						<section class="widget" style="grid-column: span {Math.min(3, Math.max(1, w.width))}" data-testid="pf-preview-widget" data-type={w.type}>
							<div class="wh"><b>{widgetTitle(w as Widget)}</b></div>
							<HomeWidget w={w as Widget} />
						</section>
					{/each}
				</div>
			{/if}
		</div>

		<h2>外觀</h2>
		{#key rev}<div class="card">
			<div class="row" style="gap:12px;flex-wrap:wrap" role="radiogroup" aria-label="主題">
				<span class="k">主題</span>
				{#each THEMES as t (t)}
					<label class="row" style="gap:4px"><input type="radio" name="theme" checked={p.theme === t} onchange={() => setOrRedraw({ theme: t as Theme })} data-testid="pf-theme-{t}" />{THEME_LABEL[t]}</label>
				{/each}
			</div>
			<div class="row" style="gap:12px;margin-top:6px" role="radiogroup" aria-label="密度">
				<span class="k">密度</span>
				{#each Object.entries(DENSITY_LABEL) as [d, l] (d)}
					<label class="row" style="gap:4px"><input type="radio" name="density" checked={p.density === d} onchange={() => setOrRedraw({ density: d as Density })} data-testid="pf-density-{d}" />{l}</label>
				{/each}
			</div>
		</div>{/key}

		<h2>左欄捷徑</h2>
		<div class="card" data-testid="pf-shortcuts">
			{#if shortcuts}
				{#each shortcuts as s, i (i)}
					<div class="row" style="gap:6px;padding:2px 0" data-testid="pf-shortcut">
						<span style="flex:1">{scName(s)}</span>
						<button class="btn sm" aria-label="上移" onclick={() => (shortcuts = moveTo(shortcuts!, i, i - 1))} disabled={i === 0}>↑</button>
						<button class="btn sm" aria-label="下移" onclick={() => (shortcuts = moveTo(shortcuts!, i, i + 1))} disabled={i === shortcuts.length - 1}>↓</button>
						<button class="btn sm ghost" onclick={() => (shortcuts = shortcuts!.filter((_, k) => k !== i))} data-testid="pf-shortcut-remove">移除</button>
					</div>
				{:else}
					<div class="empty">沒有捷徑。</div>
				{/each}
				<div class="row" style="gap:6px;margin-top:6px;flex-wrap:wrap">
					<select bind:value={scKind} aria-label="捷徑類型" data-testid="pf-shortcut-kind">
						<option value="">＋ 捷徑…</option>
						{#each ['view', 'triage', 'board', 'timeline', 'timesheet'] as k (k)}
							{#if k === 'view' || !used.has(k)}<option value={k}>{SHORTCUT_LABEL[k]}</option>{/if}
						{/each}
					</select>
					{#if scKind === 'view'}
						<select bind:value={scView} aria-label="視圖" data-testid="pf-shortcut-view">
							<option value="">選視圖…</option>
							{#each views as v (v.id)}<option value={v.id}>{v.name}</option>{/each}
						</select>
					{/if}
					<button class="btn sm" onclick={addShortcut} disabled={!scKind || (scKind === 'view' && !scView)} data-testid="pf-shortcut-add">加入</button>
					<span style="flex:1"></span>
					<button class="btn sm pri" onclick={saveShortcuts} disabled={!scDirty} data-testid="pf-shortcut-save">儲存捷徑</button>
				</div>
				<div class="hint">看板和時程捷徑開啟你目前所在的專案（不在專案裡時開第一個專案）。</div>
			{/if}
		</div>
	{/if}
</div>

<style>
	.tabs { display: flex; gap: 2px; }
	.tabs a { border: 1px solid var(--line); padding: 3px 12px; border-radius: 6px; text-decoration: none; color: inherit; }
	.tabs a.on { background: var(--act-a); border-color: var(--act); }
	.pgrid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 10px; margin-top: 8px; }
	@media (max-width: 900px) { .pgrid { grid-template-columns: minmax(0, 1fr); } .pgrid > section { grid-column: auto !important; } }
</style>

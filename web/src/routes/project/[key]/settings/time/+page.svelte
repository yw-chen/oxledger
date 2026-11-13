<script lang="ts">
	// 專案工時設定 (contract 05 §6.9, manage_project): the time tracking
	// module and which site activities this project turns off. Turning the
	// module on or off changes the project's security epoch: agents' work
	// contexts on it must be started again, so the person confirms first.
	import { page } from '$app/state';
	import type { ProjectTimeSettings } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { toast } from '$lib/toast.svelte';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	let s = $state<ProjectTimeSettings | null>(null);
	let on = $state<Record<string, boolean>>({});
	let error = $state<unknown>(null);
	let saveError = $state<unknown>(null);
	let conflict = $state(false);
	let busy = $state(false);

	function adopt(t: ProjectTimeSettings) {
		s = t;
		// No override = the site's own active flag.
		on = Object.fromEntries(t.activities.map((a) => [a.id, t.overrides.find((o) => o.activity_id === a.id)?.active ?? a.active]));
		conflict = false;
	}
	async function load() {
		error = null;
		try {
			adopt(await api('GET /projects/{id}/time-activities', { path: { id: key } }));
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void key;
		load();
	});

	async function put(module_enabled?: boolean) {
		if (!s) return;
		busy = true;
		saveError = null;
		try {
			// Only differences from the site list are overrides.
			const overrides = s.activities.filter((a) => a.active && on[a.id] !== a.active).map((a) => ({ activity_id: a.id, active: on[a.id] }));
			adopt(await api('PUT /projects/{id}/time-activities', { path: { id: key }, body: { lock_version: s.lock_version, overrides, ...(module_enabled === undefined ? {} : { module_enabled }) } }));
			toast(module_enabled === undefined ? '已儲存活動設定' : module_enabled ? '已開啟工時' : '已關閉工時', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
			else saveError = e;
		} finally {
			busy = false;
		}
	}
	function toggleModule() {
		if (!s) return;
		const next = !s.module_enabled;
		const msg = `${next ? '開啟' : '關閉'}工時模組會讓這個專案的 agent 委派需要重新建立（它們的工作脈絡會失效）。確定要${next ? '開啟' : '關閉'}嗎？`;
		if (confirm(msg)) put(next);
	}
</script>

<svelte:head><title>{key}・工時設定・oxledger</title></svelte:head>
<ProjectFrame projectKey={key}>
	<p><a href="/project/{key}/settings">← 專案設定</a></p>
	<h2>工時設定</h2>
	<ErrorBox {error} onaction={load} />
	{#if s}
		{#if !s.can_manage}<div class="banner i">你可以查看，但不能修改這個專案的工時設定。</div>{/if}
		<div class="card" data-testid="time-settings">
			<div class="row" style="gap:8px">
				<b>工時模組</b><span class="chip {s.module_enabled ? 'c-ok' : 'c-dim'}" data-testid="tm-state">{s.module_enabled ? '已開啟' : '已關閉'}</span>
				{#if s.can_manage}<button class="btn sm" onclick={toggleModule} disabled={busy} data-testid="tm-toggle">{s.module_enabled ? '關閉工時' : '開啟工時'}</button>{/if}
			</div>
			<p class="hint">開啟或關閉會讓這個專案的 agent 委派需要重新建立。</p>
			<h3>這個專案可用的活動</h3>
			{#each s.activities as a (a.id)}
				<label class="act">
					<input type="checkbox" bind:checked={on[a.id]} disabled={!s.can_manage || !a.active} data-testid="tm-act" />
					{a.name}{a.is_default ? '（預設）' : ''}{a.active ? '' : '（全站已停用）'}
				</label>
			{/each}
			{#if conflict}<div class="banner w" data-testid="tm-conflict">設定剛被別人改過，尚未儲存。<span class="sp"></span><button class="btn sm" onclick={load}>重新載入</button></div>{/if}
			<ErrorBox error={saveError} />
			{#if s.can_manage}<button class="btn pri" onclick={() => put()} disabled={busy || conflict} data-testid="tm-save">儲存活動設定</button>{/if}
		</div>
	{/if}
</ProjectFrame>

<style>
	.act {
		display: block;
		padding: 2px 0;
	}
</style>

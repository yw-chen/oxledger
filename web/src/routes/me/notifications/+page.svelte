<script lang="ts">
	// 通知偏好 (contract 05 §6.17): what to notify about, what to watch
	// automatically, muted projects. Saved with the button; `conflict` loads the
	// latest settings and says so.
	import type { NotificationPreferences } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { session } from '$lib/session.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	type Flags = Omit<NotificationPreferences, 'lock_version' | 'muted_projects'>;
	let saved = $state<NotificationPreferences | null>(null);
	let form = $state<NotificationPreferences | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		try {
			saved = await api('GET /me/notification-preferences');
			form = JSON.parse(JSON.stringify(saved));
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		load();
	});
	const dirty = $derived(!!form && !!saved && JSON.stringify(form) !== JSON.stringify(saved));

	async function save() {
		if (!form) return;
		busy = true;
		error = null;
		try {
			const { lock_version, ...rest } = form;
			saved = await api('PUT /me/notification-preferences', { operationId: newOperationId(), body: { lock_version, ...rest } });
			form = JSON.parse(JSON.stringify(saved));
			toast('通知偏好已儲存', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') {
				await load();
				toast('設定在別處被修改過，已載入最新版本', 'w');
			} else error = e;
		} finally {
			busy = false;
		}
	}
	function mute(id: string, on: boolean) {
		if (!form) return;
		form.muted_projects = on ? [...form.muted_projects, id] : form.muted_projects.filter((x) => x !== id);
	}

	const NOTIFY: [keyof Flags, string][] = [
		['on_watching', '關注的議題有更新'],
		['on_assigned', '指派給我'],
		['on_author', '我建立的議題有更新'],
		['on_triage_decided', '我回報的單分流結果'],
		['on_my_agents', '我的 agent 做的變更'],
		['on_self', '我自己做的變更']
	];
	const AUTO: [keyof Flags, string][] = [
		['auto_watch_author', '我建立的'],
		['auto_watch_assignee', '指派給我的'],
		['auto_watch_note', '我留言的']
	];
</script>

<svelte:head><title>通知偏好・oxledger</title></svelte:head>
<div class="page">
	<div class="tabs" role="tablist" style="margin-bottom:10px">
		<a role="tab" aria-selected="false" href="/me/preferences">偏好</a>
		<a role="tab" class="on" aria-selected="true" href="/me/notifications">通知</a>
	</div>
	<h1>通知偏好</h1>
	<ErrorBox {error} onaction={load} />
	{#if form}
		<div class="card" data-testid="np-notify">
			<div class="k">通知我：</div>
			{#each NOTIFY as [k, l] (k)}
				<label class="row" style="gap:6px"><input type="checkbox" bind:checked={form[k]} data-testid="np-{k}" />{l}</label>
			{/each}
		</div>
		<div class="card" data-testid="np-auto">
			<div class="k">自動關注：</div>
			{#each AUTO as [k, l] (k)}
				<label class="row" style="gap:6px"><input type="checkbox" bind:checked={form[k]} data-testid="np-{k}" />{l}</label>
			{/each}
		</div>
		<div class="card" data-testid="np-muted">
			<div class="k">靜音的專案：</div>
			{#each session.projects as p (p.id)}
				<label class="row" style="gap:6px"><input type="checkbox" checked={form.muted_projects.includes(p.id)} onchange={(e) => mute(p.id, e.currentTarget.checked)} data-testid="np-mute-{p.key}" /><span class="mono">{p.key}</span> {p.name}</label>
			{/each}
			<div class="hint">靜音的專案不會產生任何通知。</div>
		</div>
		<div class="row" style="gap:6px">
			<button class="btn pri" onclick={save} disabled={busy || !dirty} data-testid="np-save">儲存</button>
			{#if dirty}<button class="btn" onclick={() => (form = JSON.parse(JSON.stringify(saved)))} disabled={busy}>還原</button>{/if}
		</div>
		<p class="hint">email 通知還沒有提供。</p>
	{:else if !error}
		<div class="sync"><span class="spin"></span>載入中</div>
	{/if}
</div>

<style>
	.tabs { display: flex; gap: 2px; }
	.tabs a { border: 1px solid var(--line); padding: 3px 12px; border-radius: 6px; text-decoration: none; color: inherit; }
	.tabs a.on { background: var(--act-a); border-color: var(--act); }
	.card { margin-bottom: 10px; }
</style>

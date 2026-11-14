<script lang="ts">
	// Triage 設定 (contract 05 §6.5, manage_project): turn the inbox on, which
	// new issues enter it, who is on duty, and the closed status that decline
	// and duplicate move to.
	import { page } from '$app/state';
	import type { Assignee, TriageSettings, TriageWarning } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { statusCategories } from '#lib/triage.js';
	import { refreshTriageCounts } from '#lib/counts.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	let s = $state<TriageSettings | null>(null);
	let form = $state({ enabled: false, entry_rule: 'creator_without_triage', responsible: '', decline: '' });
	let users = $state<Assignee[]>([]);
	let closed = $state<{ id: string; label: string }[]>([]);
	let warnings = $state<TriageWarning[]>([]);
	let error = $state<unknown>(null);
	let saveError = $state<unknown>(null);
	let conflict = $state(false);
	let busy = $state(false);
	let opId = newOperationId();

	function adopt(t: TriageSettings) {
		s = t;
		form = { enabled: t.enabled, entry_rule: t.entry_rule, responsible: t.responsible?.id ?? '', decline: t.decline_status?.id ?? '' };
	}
	async function load() {
		error = null;
		conflict = false;
		try {
			const [t, a, cats] = await Promise.all([
				api('GET /projects/{id}/triage-settings', { path: { id: key } }),
				api('GET /projects/{id}/assignees', { path: { id: key } }).catch(() => ({ assignees: [] as Assignee[], project_id: '' })),
				statusCategories()
			]);
			adopt(t);
			users = a.assignees.filter((x) => x.kind === 'user');
			closed = [...cats].filter(([, v]) => v.category === 'done').map(([id, v]) => ({ id, label: v.label }));
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void key;
		load();
	});

	async function save() {
		if (!s) return;
		busy = true;
		saveError = null;
		try {
			const r = await api('PUT /projects/{id}/triage-settings', {
				path: { id: key },
				operationId: opId,
				body: { lock_version: s.lock_version, enabled: form.enabled, entry_rule: form.entry_rule, responsible_user_id: form.responsible || null, decline_status_id: form.decline || null }
			});
			opId = newOperationId();
			adopt(r);
			warnings = r.warnings ?? [];
			toast('已儲存 Triage 設定', 'ok');
			refreshTriageCounts();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
			else saveError = e;
			if (!(e instanceof ApiError && e.retryable)) opId = newOperationId();
		} finally {
			busy = false;
		}
	}
	const RULE = [
		{ v: 'creator_without_triage', t: '沒有分流權限的人建立的單（建議）' },
		{ v: 'all_new', t: '所有新單' }
	];
</script>

<svelte:head><title>{key}・Triage 設定・oxledger</title></svelte:head>
<ProjectFrame projectKey={key}>
	<p><a href="/project/{key}/settings">← 專案設定</a></p>
	<h2>Triage 設定</h2>
	<ErrorBox {error} onaction={load} />
	{#if s && !s.can_manage}
		<div class="banner i">你可以查看，但不能修改這個專案的 Triage 設定。</div>
	{/if}
	{#if s}
		<div class="card" data-testid="triage-settings">
			<label><input type="checkbox" bind:checked={form.enabled} disabled={!s.can_manage} data-testid="ts-enabled" /> 開啟 Triage</label>
			<fieldset class="fs">
				<legend>哪些新單進收件匣</legend>
				{#each RULE as r (r.v)}
					<label><input type="radio" bind:group={form.entry_rule} value={r.v} disabled={!s.can_manage} data-testid="ts-rule-{r.v}" /> {r.t}</label>
				{/each}
				<div class="hint">agent 建立的單一律進收件匣。</div>
			</fieldset>
			<label class="tfield">負責人（顯示在收件匣標題）
				<select bind:value={form.responsible} disabled={!s.can_manage} data-testid="ts-responsible">
					<option value="">（不指定）</option>
					{#each users as u (u.id)}<option value={u.id}>{u.name}</option>{/each}
				</select>
			</label>
			<label class="tfield">拒絕／重複時轉到
				<select bind:value={form.decline} disabled={!s.can_manage} data-testid="ts-decline">
					<option value="">（請選擇）</option>
					{#each closed as c (c.id)}<option value={c.id}>{c.label}</option>{/each}
				</select>
			</label>
			{#if form.enabled && !form.decline}<div class="hint">開啟時要選一個已結案狀態。</div>{/if}
			{#each warnings as w, i (i)}
				<div class="banner w" data-testid="ts-warning">{w.message}{w.roles.length ? `：${w.roles.map((r) => r.name).join('、')}` : ''}</div>
			{/each}
			{#if conflict}
				<div class="banner w" data-testid="ts-conflict">設定剛被別人改過，尚未儲存。<span class="sp"></span><button class="btn sm" onclick={load}>重新載入</button></div>
			{/if}
			<ErrorBox error={saveError} />
			{#if s.can_manage}
				<div class="row">
					<button class="btn pri" onclick={save} disabled={busy || conflict || (form.enabled && !form.decline)} data-testid="ts-save">{busy ? '儲存中…' : '儲存'}</button>
				</div>
			{/if}
			<p class="hint">要讓成員在專案裡看到收件匣，請到 <a href="/project/{key}/settings">專案設定</a> 把「Triage」分頁設為顯示。也可以直接開 <a href="/project/{key}/triage">收件匣</a>。</p>
		</div>
	{/if}
</ProjectFrame>

<style>
	.fs {
		border: 1px solid var(--line2);
		border-radius: 6px;
		margin: 10px 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.tfield {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin-bottom: 8px;
		max-width: 360px;
	}
</style>

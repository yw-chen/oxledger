<script lang="ts">
	// 自訂欄位（contract 05 §6.11）: the list and a new-field form.
	import { goto } from '$app/navigation';
	import type { Candidate, FieldAdminView } from '$api/types';
	import { api, newOperationId } from '$lib/api';
	import { session } from '$lib/session.svelte';
	import { listTrackers } from '$lib/trackers';
	import { FTYPES, fieldState, ftypeLabel } from '$lib/fieldadmin';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	const admin = $derived(!!session.user?.is_admin);
	let fields = $state<FieldAdminView[]>([]);
	let trackers = $state<Candidate[]>([]);
	let loadError = $state<unknown>(null);
	let loaded = $state(false);

	let adding = $state(false);
	let nf = $state({ name: '', ftype: 'string', description: '', trackers: [] as string[], options: '' });
	let addError = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		loadError = null;
		try {
			[fields, trackers] = await Promise.all([api('GET /admin/custom-fields'), listTrackers()]);
			fields = [...fields].sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
			loaded = true;
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		if (admin) load();
	});

	const tname = (id: string) => trackers.find((t) => t.id === id)?.name ?? '（已刪除的類型）';
	const optionLabels = $derived(
		nf.options
			.split('\n')
			.map((s) => s.trim())
			.filter(Boolean)
	);
	const canAdd = $derived(!!nf.name.trim() && nf.trackers.length > 0 && (nf.ftype !== 'list' || optionLabels.length > 0));

	async function create() {
		busy = true;
		addError = null;
		try {
			const f = await api('POST /admin/custom-fields', {
				operationId: newOperationId(),
				body: { name: nf.name.trim(), ftype: nf.ftype, description: nf.description.trim() || null, tracker_ids: nf.trackers, is_for_all: true, ...(nf.ftype === 'list' ? { options: optionLabels.map((label) => ({ label })) } : {}) }
			});
			toast(f.index_state === 'pending' ? `已建立「${f.name}」；索引建立中，篩選可用但較慢` : `已建立「${f.name}」`, 'ok');
			goto(`/admin/custom-fields/${f.id}`);
		} catch (e) {
			addError = e;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>自訂欄位・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px">
		<h1 style="margin:0">自訂欄位</h1>
		<span style="flex:1"></span>
		{#if admin && loaded}<button class="btn pri" onclick={() => (adding = !adding)} data-testid="cf-new">＋ 新增欄位</button>{/if}
	</div>
	{#if !admin}
		<div class="empty" data-testid="cf-denied">這一頁只有系統管理員可以使用。</div>
	{:else}
		<ErrorBox error={loadError} onaction={load} />
		{#if adding}
			<div class="card" data-testid="cf-add">
				<h3>新增欄位</h3>
				<div class="cfg">
					<label class="f">名稱<input bind:value={nf.name} data-testid="cf-add-name" /></label>
					<label class="f"
						>型別
						<select bind:value={nf.ftype} data-testid="cf-add-type">
							{#each FTYPES as [k, l] (k)}<option value={k}>{l}</option>{/each}
						</select>
					</label>
					<label class="f" style="grid-column:1/-1">說明（選填）<input bind:value={nf.description} /></label>
				</div>
				<div class="hint">適用的類型（至少一個；之後可以在詳情頁改適用專案與可見性）</div>
				<div class="row" style="gap:10px;flex-wrap:wrap;margin:4px 0 8px">
					{#each trackers as t (t.id)}
						<label><input type="checkbox" value={t.id} bind:group={nf.trackers} data-testid="cf-add-tracker" /> {t.name}</label>
					{/each}
				</div>
				{#if nf.ftype === 'list'}
					<label class="f">選項（一行一個）<textarea bind:value={nf.options} rows="4" data-testid="cf-add-options"></textarea></label>
				{/if}
				<ErrorBox error={addError} />
				<div class="row" style="gap:8px;margin-top:8px">
					<button class="btn pri" onclick={create} disabled={busy || !canAdd} data-testid="cf-add-submit">建立</button>
					<button class="btn" onclick={() => (adding = false)}>取消</button>
				</div>
			</div>
		{/if}
		{#if loaded && !fields.length}<div class="empty">還沒有自訂欄位。</div>{/if}
		{#if fields.length}
			<table class="t" data-testid="cf-list">
				<thead><tr><th>名稱</th><th>型別</th><th>適用類型</th><th>可見性</th><th>必填</th><th>狀態</th></tr></thead>
				<tbody>
					{#each fields as f (f.id)}
						{@const st = fieldState(f)}
						<tr class:dis={f.deactivated || f.index_state === 'dropping'} data-testid="cf-row">
							<td><a href="/admin/custom-fields/{f.id}">{f.name}</a> <span class="mono dim">{f.key}</span></td>
							<td>{ftypeLabel(f.ftype)}</td>
							<td>{f.tracker_ids.map(tname).join('、') || '—'}{f.is_for_all ? '' : '・指定專案'}</td>
							<td>{f.visible ? '所有人' : `受限（${f.role_ids.length} 個角色）`}</td>
							<td>{f.is_required ? '必填' : ''}</td>
							<td class={st.cls}>{st.label}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		{/if}
	{/if}
</div>

<style>
	.cfg {
		display: grid;
		grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
		gap: 8px;
	}
	.f {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
</style>

<script lang="ts">
	// 自訂欄位詳情（contract 05 §6.11, §3.7）: basics, scope, visibility,
	// options and the danger zone. Changes to visibility or scope warn that
	// agent delegations of those projects must be made again (U1).
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import type { Candidate, FieldAdminView, FieldDetail, FieldSet, RequiredImpact } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { session } from '$lib/session.svelte';
	import { listTrackers } from '$lib/trackers';
	import { fieldState, ftypeLabel } from '$lib/fieldadmin';
	import { stepped } from '$lib/stepup.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	const admin = $derived(!!session.user?.is_admin);
	const id = $derived(page.params.id ?? '');

	let field = $state<FieldAdminView | null>(null);
	let trackers = $state<Candidate[]>([]);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let conflict = $state(false);
	let busy = $state(false);
	let impact = $state<RequiredImpact | null>(null);

	type Form = { name: string; description: string; is_required: boolean; trackers: string[]; is_for_all: boolean; projects: string[]; visible: boolean; roles: string[] };
	let form = $state<Form>({ name: '', description: '', is_required: false, trackers: [], is_for_all: true, projects: [], visible: true, roles: [] });
	let roleInput = $state('');

	function adopt(f: FieldAdminView, keepForm = false) {
		field = f;
		conflict = false;
		if (keepForm) return;
		form = { name: f.name, description: f.description, is_required: f.is_required, trackers: [...f.tracker_ids], is_for_all: f.is_for_all, projects: [...f.project_ids], visible: f.visible, roles: [...f.role_ids] };
		conflict = false;
	}
	async function load() {
		loadError = null;
		try {
			const [d, t] = await Promise.all([api('GET /admin/custom-fields/{id}', { path: { id } }), listTrackers()]);
			trackers = t;
			adopt(d.field);
			impact = null;
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		if (admin && id) load();
	});

	const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every((x) => b.includes(x));
	const changes = $derived.by((): FieldSet => {
		const f = field;
		if (!f) return {};
		const s: FieldSet = {};
		if (form.name.trim() !== f.name) s.name = form.name.trim();
		if (form.description.trim() !== f.description) s.description = form.description.trim();
		if (form.is_required !== f.is_required) s.is_required = form.is_required;
		if (!sameSet(form.trackers, f.tracker_ids)) s.tracker_ids = form.trackers;
		if (form.is_for_all !== f.is_for_all) s.is_for_all = form.is_for_all;
		if (!form.is_for_all && !sameSet(form.projects, f.project_ids)) s.project_ids = form.projects;
		if (form.visible !== f.visible) s.visible = form.visible;
		if (!form.visible && !sameSet(form.roles, f.role_ids)) s.role_ids = form.roles;
		return s;
	});
	const dirty = $derived(Object.keys(changes).length > 0);
	const touchesAudience = $derived(['tracker_ids', 'is_for_all', 'project_ids', 'visible', 'role_ids'].some((k) => k in changes));
	const invalid = $derived(!form.name.trim() ? '名稱不能空白' : !form.trackers.length ? '至少要適用一個類型' : !form.is_for_all && !form.projects.length ? '指定專案時至少選一個專案' : '');

	async function patch(set: FieldSet, done: string): Promise<FieldDetail | null> {
		if (!field) return null;
		busy = true;
		error = null;
		try {
			const d = await api('PATCH /admin/custom-fields/{id}', { path: { id }, operationId: newOperationId(), body: { lock_version: field.lock_version, set } });
			adopt(d.field);
			impact = d.required_impact ?? null;
			toast(done, 'ok');
			return d;
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
			else error = e;
			return null;
		} finally {
			busy = false;
		}
	}
	async function save() {
		if (touchesAudience && !confirm('這會讓已委派這些專案的 agent 需要重新建立委派。要繼續嗎？')) return;
		await patch(changes, '已儲存');
	}
	/** Before making the field required: how many issues would break (B-11). */
	async function previewRequired() {
		error = null;
		try {
			const d = await api('GET /admin/custom-fields/{id}', { path: { id }, query: { preview_required: true } });
			impact = d.required_impact ?? null;
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		if (field && form.is_required && !field.is_required) previewRequired();
		else if (!form.is_required) impact = null;
	});

	function addRole() {
		const r = roleInput.trim();
		if (r && !form.roles.includes(r)) form.roles = [...form.roles, r];
		roleInput = '';
	}

	// ---- options (list fields) ----
	let newOption = $state('');
	async function optionCall(fn: () => Promise<FieldAdminView>, done: string) {
		busy = true;
		error = null;
		try {
			adopt(await fn(), dirty);
			toast(done, 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
			else error = e;
		} finally {
			busy = false;
		}
	}
	const addOption = () => {
		const label = newOption.trim();
		if (!label) return;
		newOption = '';
		return optionCall(() => api('POST /admin/custom-fields/{id}/options', { path: { id }, operationId: newOperationId(), body: { label } }), `已新增選項「${label}」`);
	};
	const optPatch = (oid: string, body: { label?: string; active?: boolean; position?: number }, done: string) =>
		optionCall(() => api('PATCH /admin/custom-fields/{id}/options/{oid}', { path: { id, oid }, operationId: newOperationId(), body: { lock_version: field!.lock_version, ...body } }), done);
	const removeOption = (oid: string, label: string) => {
		if (!confirm(`刪除選項「${label}」？已有單用到時不能刪，只能停用。`)) return;
		return optionCall(() => api('DELETE /admin/custom-fields/{id}/options/{oid}', { path: { id, oid }, query: { lock_version: field!.lock_version } }), `已刪除選項「${label}」`);
	};
	function renameOption(oid: string, old: string) {
		const label = prompt('新的選項名稱', old)?.trim();
		if (label && label !== old) optPatch(oid, { label }, `已改名為「${label}」`);
	}
	const options = $derived([...(field?.options ?? [])].sort((a, b) => a.position - b.position));
	/** Swap with the neighbour: the server does not renumber, so both move
	 *  (two PATCHes, the second with the new lock_version). Equal positions
	 *  (older data) are spread out first. */
	function moveOption(i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= options.length) return;
		const a = options[i];
		const b = options[j];
		const pa = a.position === b.position ? (d < 0 ? b.position : b.position + 1) : b.position;
		const pb = a.position === b.position ? (d < 0 ? b.position + 1 : b.position) : a.position;
		return optionCall(async () => {
			const first = await api('PATCH /admin/custom-fields/{id}/options/{oid}', { path: { id, oid: a.id }, operationId: newOperationId(), body: { lock_version: field!.lock_version, position: pa } });
			return api('PATCH /admin/custom-fields/{id}/options/{oid}', { path: { id, oid: b.id }, operationId: newOperationId(), body: { lock_version: first.lock_version, position: pb } });
		}, '已調整順序');
	}

	// ---- danger zone ----
	const setActive = (on: boolean) => {
		if (!on && !confirm('停用後新單不會出現這個欄位，既有的值保留（可以再啟用）。要停用嗎？')) return;
		return patch({ deactivated: !on }, on ? '已重新啟用' : '已停用');
	};
	let deleteName = $state('');
	async function remove() {
		if (!field) return;
		busy = true;
		error = null;
		try {
			const f = field;
			const r = await stepped(() => api('DELETE /admin/custom-fields/{id}', { path: { id }, query: { lock_version: f.lock_version } }), '刪除自訂欄位需要確認身分');
			if (r) {
				toast(`「${f.name}」刪除中：值、索引與定義在背景清除`, 'ok');
				goto('/admin/custom-fields');
			}
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict' && (e.body?.details as { kind?: string } | undefined)?.kind !== 'conversion_running') conflict = true;
			else error = e;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>{field?.name ?? '自訂欄位'}・oxledger</title></svelte:head>
<div class="page">
	<div class="hint"><a href="/admin/custom-fields">← 自訂欄位</a></div>
	{#if !admin}
		<div class="empty" data-testid="cf-denied">這一頁只有系統管理員可以使用。</div>
	{:else}
		<ErrorBox error={loadError} onaction={load} />
		{#if field}
			{@const st = fieldState(field)}
			<div class="row" style="gap:8px;margin:6px 0 10px">
				<h1 style="margin:0" data-testid="cf-title">{field.name}</h1>
				<span class="mono dim">{field.key}</span>
				<span class="chip">{ftypeLabel(field.ftype)}</span>
				<span class="chip {st.cls}" data-testid="cf-state">{st.label}</span>
			</div>
			{#if field.index_state === 'pending'}<div class="banner i">索引建立中：篩選可用，但會比較慢。</div>{/if}
			{#if field.index_state === 'dropping'}<div class="banner w">這個欄位正在刪除，不能再修改。</div>{/if}
			{#if conflict}
				<div class="banner w" data-testid="cf-conflict">這個欄位剛被別人改過，尚未儲存你的變更。<span class="sp"></span><button class="btn sm" onclick={load}>載入最新內容</button></div>
			{/if}
			<ErrorBox {error} />

			<fieldset class="card" disabled={field.index_state === 'dropping' || busy}>
				<h3>基本資料</h3>
				<label class="f">名稱<input bind:value={form.name} data-testid="cf-name" /></label>
				<label class="f">說明<input bind:value={form.description} data-testid="cf-desc" /></label>
				<label><input type="checkbox" bind:checked={form.is_required} data-testid="cf-required" /> 必填</label>
				{#if impact && form.is_required}
					<div class="banner {impact.violating ? 'w' : 'o'}" data-testid="cf-required-impact">
						{#if impact.violating}
							{impact.complete ? '' : '至少'} {impact.violating} 張現有議題這個欄位是空的（例：{impact.sample_keys.join('、')}）。改成必填後，這些單下次修改時要補齊。
						{:else}
							掃描了 {impact.scanned} 張議題，都已經有值{impact.complete ? '' : '（只掃描了一部分）'}。
						{/if}
					</div>
				{/if}

				<h3 style="margin-top:12px">適用範圍</h3>
				<div class="row wrapr">
					{#each trackers as t (t.id)}<label><input type="checkbox" value={t.id} bind:group={form.trackers} data-testid="cf-tracker" /> {t.name}</label>{/each}
				</div>
				<div class="row wrapr">
					<label><input type="radio" name="cf-scope" checked={form.is_for_all} onchange={() => (form.is_for_all = true)} data-testid="cf-all-projects" /> 所有專案</label>
					<label><input type="radio" name="cf-scope" checked={!form.is_for_all} onchange={() => (form.is_for_all = false)} data-testid="cf-some-projects" /> 指定專案</label>
				</div>
				{#if !form.is_for_all}
					<div class="row wrapr">
						{#each session.projects as p (p.id)}<label><input type="checkbox" value={p.id} bind:group={form.projects} /> {p.key}</label>{/each}
						{#each form.projects.filter((x) => !session.projects.some((p) => p.id === x)) as x (x)}<span class="chip c-dim" title={x}>看不到的專案</span>{/each}
					</div>
				{/if}

				<h3 style="margin-top:12px">可見性</h3>
				<div class="row wrapr">
					<label><input type="radio" name="cf-vis" checked={form.visible} onchange={() => (form.visible = true)} data-testid="cf-visible" /> 所有人</label>
					<label><input type="radio" name="cf-vis" checked={!form.visible} onchange={() => (form.visible = false)} data-testid="cf-restricted" /> 受限：只有管理員與下列角色</label>
				</div>
				{#if !form.visible}
					<div class="chips" style="margin:4px 0">
						{#each form.roles as r (r)}
							<span class="chip" title={r}>角色 {r.slice(-6)} <button type="button" class="x" onclick={() => (form.roles = form.roles.filter((x) => x !== r))} aria-label="移除角色">×</button></span>
						{:else}
							<span class="hint">還沒有角色：只有管理員看得到。</span>
						{/each}
					</div>
					<div class="inline-form">
						<input bind:value={roleInput} placeholder="角色 id" style="width:300px" data-testid="cf-role-input" />
						<button type="button" class="btn sm" onclick={addRole} disabled={!roleInput.trim()}>加入</button>
					</div>
					<div class="hint">伺服器還沒有角色清單（G31），暫時用角色 id 指定。受限欄位不會交給 agent。</div>
				{/if}

				<div class="row" style="gap:8px;margin-top:12px">
					<button class="btn pri" onclick={save} disabled={!dirty || !!invalid || conflict} data-testid="cf-save">儲存</button>
					<button class="btn" onclick={() => field && adopt(field)} disabled={!dirty}>還原</button>
					{#if invalid && dirty}<span class="err">{invalid}</span>{/if}
				</div>
			</fieldset>

			{#if field.ftype === 'list'}
				<fieldset class="card" disabled={field.index_state === 'dropping' || busy} data-testid="cf-options">
					<h3>選項</h3>
					{#each options as o, i (o.id)}
						<div class="row orow" class:dim={!o.active} data-testid="cf-option">
							<button type="button" class="btn sm ghost" onclick={() => moveOption(i, -1)} disabled={i === 0} aria-label="上移">↑</button>
							<button type="button" class="btn sm ghost" onclick={() => moveOption(i, 1)} disabled={i === options.length - 1} aria-label="下移">↓</button>
							<span class="lbl" data-testid="cf-option-label">{o.label}</span>
							{#if !o.active}<span class="chip c-dim">已停用</span>{/if}
							<span style="flex:1"></span>
							<button type="button" class="btn sm" onclick={() => renameOption(o.id, o.label)}>改名</button>
							<button type="button" class="btn sm" onclick={() => optPatch(o.id, { active: !o.active }, o.active ? `已停用「${o.label}」` : `已啟用「${o.label}」`)} data-testid="cf-option-toggle">{o.active ? '停用' : '啟用'}</button>
							<button type="button" class="btn sm" onclick={() => removeOption(o.id, o.label)} data-testid="cf-option-delete">刪除</button>
						</div>
					{/each}
					<form class="inline-form" style="margin-top:6px" onsubmit={(e) => (e.preventDefault(), addOption())}>
						<input bind:value={newOption} placeholder="新選項" data-testid="cf-option-new" />
						<button class="btn sm" disabled={!newOption.trim()} data-testid="cf-option-add">新增</button>
					</form>
				</fieldset>
			{/if}

			<fieldset class="card b" disabled={field.index_state === 'dropping' || busy} data-testid="cf-danger">
				<h3>危險區</h3>
				<div class="row dz">
					<div><b>{field.deactivated ? '重新啟用' : '停用'}</b><div class="hint">停用後新單不會出現這個欄位，既有的值保留。</div></div>
					<button class="btn" onclick={() => setActive(field!.deactivated)} data-testid="cf-deactivate">{field.deactivated ? '重新啟用' : '停用'}</button>
				</div>
				<div class="row dz">
					<div><b>換型別</b><div class="hint">型別不能直接改：建立新欄位並把值轉過去，舊欄位保留到你停用它。</div></div>
					<a class="btn" href="/admin/custom-fields/{field.id}/convert" data-testid="cf-convert">換型別…</a>
				</div>
				<div class="row dz">
					<div>
						<b>刪除</b>
						<div class="hint">所有單上的值、索引與定義都會刪除，無法復原。輸入欄位名稱「{field.name}」確認。</div>
						<input bind:value={deleteName} placeholder={field.name} style="margin-top:4px" data-testid="cf-delete-name" />
					</div>
					<button class="btn bad" onclick={remove} disabled={deleteName.trim() !== field.name} data-testid="cf-delete">刪除欄位</button>
				</div>
			</fieldset>
		{/if}
	{/if}
</div>

<style>
	fieldset {
		min-width: 0;
	}
	.f {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin-bottom: 6px;
	}
	.wrapr {
		gap: 12px;
		flex-wrap: wrap;
		margin: 4px 0;
	}
	.orow {
		gap: 6px;
		padding: 3px 0;
		border-bottom: 1px solid var(--line);
	}
	.orow.dim .lbl {
		text-decoration: line-through;
	}
	.dz {
		gap: 12px;
		justify-content: space-between;
		padding: 8px 0;
		border-bottom: 1px solid var(--line);
	}
	.dz:last-child {
		border-bottom: 0;
	}
	.x {
		background: none;
		border: 0;
		color: inherit;
		padding: 0 0 0 4px;
		cursor: pointer;
	}
	.err {
		color: var(--bad);
		font-size: 12.5px;
	}
</style>

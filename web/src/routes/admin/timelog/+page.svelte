<script lang="ts">
	// 工時設定（系統管理員，contract 05 §6.9）: the org's time rules and the
	// site's activities (order, rename, deactivate, default). An activity with
	// entries can only be deactivated.
	import type { Activity, TimelogSettings } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { parseHours, showHours } from '#lib/hours.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import { session } from '#lib/session.svelte.js';

	const admin = $derived(!!session.user?.is_admin);

	let rules = $state<TimelogSettings | null>(null);
	let form = $state({ max: '', accept_0_hours: false, accept_future_dates: false, accept_closed_issues: true, require_issue: false, require_comments: false });
	let ruleError = $state<unknown>(null);
	let ruleConflict = $state(false);
	let busy = $state(false);

	type Row = { id: string | null; name: string; active: boolean; is_default: boolean; in_use: boolean; uid: number };
	let acts = $state<Row[]>([]);
	let actError = $state<unknown>(null);
	let loadError = $state<unknown>(null);
	let uid = 0;

	function adoptRules(r: TimelogSettings) {
		rules = r;
		form = { max: showHours(r.max_hours_per_day), accept_0_hours: r.accept_0_hours, accept_future_dates: r.accept_future_dates, accept_closed_issues: r.accept_closed_issues, require_issue: r.require_issue, require_comments: r.require_comments };
		ruleConflict = false;
	}
	function adoptActs(list: Activity[]) {
		acts = [...list].sort((a, b) => a.position - b.position).map((a) => ({ id: a.id, name: a.name, active: a.active, is_default: a.is_default, in_use: a.in_use, uid: ++uid }));
	}
	async function load() {
		loadError = null;
		try {
			const [r, a] = await Promise.all([api('GET /org/timelog-settings'), api('GET /time-activities')]);
			adoptRules(r);
			// Without `project` the server answers the site list (Activity[]).
			adoptActs(a as unknown as Activity[]);
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		if (admin) load();
	});

	const maxParsed = $derived(form.max.trim() === '' ? '0.00' : parseHours(form.max));
	async function saveRules() {
		if (!rules || maxParsed === null) return;
		busy = true;
		ruleError = null;
		try {
			adoptRules(
				await api('PUT /org/timelog-settings', {
					operationId: newOperationId(),
					body: { lock_version: rules.lock_version, max_hours_per_day: maxParsed, accept_0_hours: form.accept_0_hours, accept_future_dates: form.accept_future_dates, accept_closed_issues: form.accept_closed_issues, require_issue: form.require_issue, require_comments: form.require_comments }
				})
			);
			toast('已儲存工時規則', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') ruleConflict = true;
			else ruleError = e;
		} finally {
			busy = false;
		}
	}

	function move(i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= acts.length) return;
		const r = [...acts];
		[r[i], r[j]] = [r[j], r[i]];
		acts = r;
	}
	function setDefault(i: number) {
		acts = acts.map((a, k) => ({ ...a, is_default: k === i ? !a.is_default : false }));
	}
	function add() {
		acts = [...acts, { id: null, name: '', active: true, is_default: false, in_use: false, uid: ++uid }];
	}
	function remove(i: number) {
		acts = acts.filter((_, k) => k !== i);
	}
	async function saveActs() {
		busy = true;
		actError = null;
		try {
			const items = acts.map((a, i) => ({ ...(a.id ? { id: a.id } : {}), name: a.name.trim(), position: i + 1, active: a.active, is_default: a.is_default && a.active }));
			adoptActs(await api('PUT /admin/time-activities', { body: { items } }));
			toast('已儲存活動', 'ok');
		} catch (e) {
			actError = e;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>工時設定・oxledger</title></svelte:head>
<div class="page">
	<h1>工時設定</h1>
	{#if !admin}
		<div class="empty" data-testid="tl-denied">這一頁只有系統管理員可以使用。</div>
	{/if}
	<ErrorBox error={loadError} onaction={load} />
	{#if rules}
		<div class="card" data-testid="timelog-rules">
			<h3 style="margin-top:0">規則</h3>
			<label class="fld">每天最多可記（小時，0＝不限）
				<input bind:value={form.max} style="width:100px" data-testid="tl-max" aria-invalid={maxParsed === null} />
			</label>
			<label><input type="checkbox" bind:checked={form.accept_0_hours} data-testid="tl-zero" /> 接受 0 小時</label>
			<label><input type="checkbox" bind:checked={form.accept_future_dates} data-testid="tl-future" /> 接受未來的日期</label>
			<label><input type="checkbox" bind:checked={form.accept_closed_issues} data-testid="tl-closed" /> 接受已結案的議題</label>
			<label><input type="checkbox" bind:checked={form.require_issue} data-testid="tl-req-issue" /> 必須指定議題</label>
			<label><input type="checkbox" bind:checked={form.require_comments} data-testid="tl-req-comments" /> 必須填說明</label>
			{#if ruleConflict}<div class="banner w">規則剛被別人改過，尚未儲存。<span class="sp"></span><button class="btn sm" onclick={load}>重新載入</button></div>{/if}
			<ErrorBox error={ruleError} />
			<button class="btn pri" onclick={saveRules} disabled={busy || ruleConflict || maxParsed === null} data-testid="tl-save">儲存規則</button>
		</div>
		<div class="card" data-testid="timelog-acts">
			<h3 style="margin-top:0">活動</h3>
			{#each acts as a, i (a.uid)}
				<div class="row arow" data-testid="tl-act">
					<button class="btn sm ghost" onclick={() => move(i, -1)} disabled={i === 0} aria-label="上移">↑</button>
					<button class="btn sm ghost" onclick={() => move(i, 1)} disabled={i === acts.length - 1} aria-label="下移">↓</button>
					<input bind:value={a.name} placeholder="活動名稱" style="width:180px" data-testid="tl-act-name" />
					<label><input type="checkbox" bind:checked={a.active} /> 啟用</label>
					<label><input type="radio" name="tl-default" checked={a.is_default} onclick={() => setDefault(i)} disabled={!a.active} /> 預設</label>
					<button class="btn sm" onclick={() => remove(i)} disabled={a.in_use} title={a.in_use ? '已有工時，只能停用' : ''} data-testid="tl-act-remove">刪除</button>
					{#if a.in_use}<span class="hint">已有工時，只能停用</span>{/if}
				</div>
			{/each}
			<div class="row" style="margin-top:8px">
				<button class="btn" onclick={add} data-testid="tl-act-add">＋ 新增活動</button>
				<button class="btn pri" onclick={saveActs} disabled={busy || acts.some((a) => !a.name.trim())} data-testid="tl-act-save">儲存活動</button>
			</div>
			<ErrorBox error={actError} />
		</div>
	{/if}
</div>

<style>
	.fld {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin-bottom: 6px;
	}
	.card label {
		display: block;
	}
	.arow label {
		display: inline;
	}
	.arow {
		gap: 6px;
		padding: 3px 0;
	}
</style>

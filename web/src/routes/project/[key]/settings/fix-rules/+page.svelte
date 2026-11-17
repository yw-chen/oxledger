<script lang="ts">
	// 修正規則 (design 06 §6.9, R-F1–R-F11; manage_project): "keyword →
	// target status" rules for pull requests merged into a linked
	// repository's default branch. A rule runs with the permissions of who
	// last changed or enabled it (the sponsor); whoever saves becomes that.
	import { page } from '$app/state';
	import type { FixRules } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { fixRuleProblem } from '#lib/errors.js';
	import { DISABLED_REASON } from '#lib/fixes.js';
	import { toast } from '#lib/toast.svelte.js';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	type Row = { id: string | null; enabled: boolean; keywords: string; target: string; trackers: string[]; sponsor: string | null; reason: string | null };

	const MAX_RULES = 5;
	const key = $derived((page.params.key ?? '').toUpperCase());
	let s = $state<FixRules | null>(null);
	let rows = $state<Row[]>([]);
	let error = $state<unknown>(null);
	let saveError = $state<unknown>(null);
	/** Index of the rule a `fix_rule_invalid` answer names. */
	let badIndex = $state<number | null>(null);
	let badText = $state('');
	let conflict = $state(false);
	let confirming = $state(false);
	let busy = $state(false);
	let opId = newOperationId();

	function adopt(v: FixRules) {
		s = v;
		rows = v.rules.map((r) => ({
			id: r.id,
			enabled: r.enabled,
			keywords: r.keywords.join(' '),
			target: r.target_status_id,
			trackers: r.tracker_ids ?? [],
			sponsor: r.sponsor.name,
			reason: r.enabled ? null : (r.disabled_reason ?? null)
		}));
	}
	async function load() {
		error = null;
		saveError = null;
		conflict = false;
		badIndex = null;
		try {
			adopt(await api('GET /projects/{id}/fix-rules', { path: { id: key } }));
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void key;
		load();
	});

	function add() {
		if (!s) return;
		badIndex = null;
		// Keywords are unique in a project: only the first rule gets GitHub's own.
		const used = new Set(rows.flatMap((r) => words(r.keywords).map((k) => k.toLowerCase())));
		const free = s.default_keywords.filter((k) => !used.has(k));
		rows.push({ id: null, enabled: false, keywords: free.join(' '), target: '', trackers: [], sponsor: null, reason: null });
	}
	function remove(i: number) {
		badIndex = null;
		rows.splice(i, 1);
	}
	const words = (t: string) => t.split(/[\s,，、]+/).filter(Boolean);
	const incomplete = $derived(rows.some((r) => !r.target || !words(r.keywords).length));

	async function save() {
		if (!s) return;
		confirming = false;
		busy = true;
		saveError = null;
		badIndex = null;
		try {
			const r = await api('PUT /projects/{id}/fix-rules', {
				path: { id: key },
				operationId: opId,
				body: {
					lock_version: s.lock_version,
					rules: rows.map((x) => ({ id: x.id, enabled: x.enabled, keywords: words(x.keywords), target_status_id: x.target, tracker_ids: x.trackers.length ? x.trackers : null }))
				}
			});
			opId = newOperationId();
			adopt(r);
			toast('已儲存修正規則', 'ok');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
			else if (e instanceof ApiError && e.code === 'fix_rule_invalid') {
				const i = Number(e.details?.index);
				badIndex = Number.isInteger(i) && i < rows.length ? i : null;
				badText = fixRuleProblem(e.details ?? {});
				if (badIndex === null) saveError = e;
			} else saveError = e;
			if (!(e instanceof ApiError && e.retryable)) opId = newOperationId();
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>{key}・修正規則・oxledger</title></svelte:head>
<ProjectFrame projectKey={key}>
	<p><a href="/project/{key}/settings">← 專案設定</a></p>
	<h2>修正規則</h2>
	<ErrorBox {error} onaction={load} />
	{#if s}
		<div class="banner w" data-testid="fix-warning">
			能合併到這個 repo 預設分支的人，都能讓本專案符合規則的單轉成目標狀態。PR 內文與 commit 訊息可能出自外部貢獻者，合併前請確認裡面的修正關鍵字。目標狀態建議用「已解決」這類還沒結案的狀態，不要直接用「已關閉」。
		</div>
		<p class="hint">
			PR 合併進預設分支時，標題、內文或 commit 訊息寫了「關鍵字 單號」（例如 <code>fixes {key}-12</code>），系統先向 GitHub 確認，再把單轉到目標狀態。直接推到預設分支的 commit 只會在單上提示，不會自動轉。
		</p>
		{#if !s.has_linked_repo}
			<div class="banner i" data-testid="fix-no-repo">先連動 GitHub repo 才能使用。到 <a href="/project/{key}/settings">專案設定</a> 的「開發（GitHub）」連動。</div>
		{/if}
		<div class="rules" data-testid="fix-rules">
			{#each rows as r, i (i)}
				<div class="card rule" data-testid="fix-rule">
					<div class="row">
						<label><input type="checkbox" bind:checked={r.enabled} disabled={!s.has_linked_repo && !r.enabled} data-testid="fix-enabled" /> 啟用</label>
						<span class="sp"></span>
						<button type="button" class="btn sm ghost" onclick={() => remove(i)} data-testid="fix-remove">移除</button>
					</div>
					<label class="tfield">關鍵字（空白分隔，最多 10 個，不分大小寫）
						<input type="text" bind:value={r.keywords} placeholder="fixes closes 修正" data-testid="fix-keywords" />
					</label>
					<label class="tfield">目標狀態
						<select bind:value={r.target} data-testid="fix-target">
							<option value="">（請選擇）</option>
							{#each s.statuses as st (st.id)}<option value={st.id}>{st.name}{st.closed ? '（結案）' : ''}</option>{/each}
						</select>
					</label>
					{#if s.trackers.length > 1}
						<fieldset class="fs">
							<legend>適用的追蹤類型（都不勾＝全部）</legend>
							{#each s.trackers as t (t.id)}
								<label><input type="checkbox" value={t.id} bind:group={r.trackers} /> {t.name}</label>
							{/each}
						</fieldset>
					{/if}
					{#if r.sponsor}<div class="hint" data-testid="fix-sponsor">規則以 {r.sponsor} 的權限執行</div>{:else}<div class="hint">儲存後以你的權限執行</div>{/if}
					{#if r.reason && r.reason !== 'manual'}<div class="bad" data-testid="fix-disabled-reason">{DISABLED_REASON[r.reason] ?? r.reason}</div>{/if}
					{#if badIndex === i}<div class="bad" data-testid="fix-invalid">{badText}</div>{/if}
				</div>
			{:else}
				<div class="empty">還沒有規則。</div>
			{/each}
		</div>
		{#if conflict}
			<div class="banner w" data-testid="fix-conflict">規則剛被別人改過，尚未儲存。<span class="sp"></span><button class="btn sm" onclick={load}>重新載入</button></div>
		{/if}
		<ErrorBox error={saveError} />
		{#if confirming}
			<div class="banner i" data-testid="fix-confirm">
				儲存後，你修改或啟用的規則會以你的權限執行。
				<span class="sp"></span>
				<button class="btn sm pri" onclick={save} disabled={busy} data-testid="fix-confirm-save">確定儲存</button>
				<button class="btn sm" onclick={() => (confirming = false)}>取消</button>
			</div>
		{/if}
		<div class="row">
			<button class="btn" onclick={add} disabled={rows.length >= MAX_RULES} data-testid="fix-add">新增規則</button>
			<button class="btn pri" onclick={() => (confirming = true)} disabled={busy || conflict || incomplete || confirming} data-testid="fix-save">{busy ? '儲存中…' : '儲存'}</button>
			{#if rows.length >= MAX_RULES}<span class="hint">最多 {MAX_RULES} 條</span>{/if}
		</div>
	{/if}
</ProjectFrame>

<style>
	.rules {
		display: flex;
		flex-direction: column;
		gap: 10px;
		margin: 10px 0;
		max-width: 560px;
	}
	.rule {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
	}
	.sp {
		flex: 1;
	}
	.tfield {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
	.fs {
		border: 1px solid var(--line2);
		border-radius: 6px;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
	}
	.bad {
		color: var(--bad, #b42318);
		font-size: 12.5px;
	}
</style>

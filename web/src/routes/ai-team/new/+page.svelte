<script lang="ts">
	// 建立 AI 角色 (contract 05 §6.18): ① template → ② name, duties, agent
	// software → ③ runtime (existing, or new with the isolation declaration;
	// one runtime per agent software, Q2) → ④ projects and policies (the
	// template filled in, the whole table visible) → ⑤ reading your time →
	// ⑥ confirm → the token, once. One step-up and one Idempotency-Key for
	// the whole wizard: pressing again after a failure replays, never doubles.
	import type { AgentClientView, CreatedRole, DelegationView, IssuedCredential, PolicyMode, RuntimeView, TemplateSpec } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { anyAllowed, byMode, changesFrom, commandLabel, ROLE_KEY, setAll, sharedRuntimeText, TEMPLATE_DUTY, TEMPLATE_LABEL, TIME_WARNING, WIZARD_TEMPLATES, type Policies } from '#lib/aiteam.js';
	import { refreshCounts } from '#lib/counts.svelte.js';
	import { fieldLabel } from '#lib/format.js';
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import { session, steppedUpRecently } from '#lib/session.svelte.js';
	import { askPassword, stepped, stepUp } from '#lib/stepup.svelte.js';
	import { customFieldNames } from '#lib/trackers.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import PolicyTable from '#lib/components/PolicyTable.svelte';
	import TokenOnce from '#lib/components/TokenOnce.svelte';

	const STEPS = ['範本', '名稱與軟體', '執行環境', '專案與政策', '讀取範圍', '確認'];
	const DEFAULT_KEY: Record<string, string> = { triage: 'triage', pm_assistant: 'pm', design_review: 'design_review', qa: 'qa', release: 'release' };

	let step = $state(1);
	let templates = $state<TemplateSpec[]>([]);
	let clients = $state<AgentClientView[]>([]);
	let runtimes = $state<RuntimeView[]>([]);
	let delegations = $state<DelegationView[]>([]);
	let loadError = $state<unknown>(null);

	let template = $state('');
	let roleKey = $state('');
	let duties = $state('');
	let clientId = $state('');
	let rtMode = $state<'existing' | 'new'>('new');
	let rtId = $state('');
	let rtLabel = $state('');
	let isolationAck = $state(false);
	let picked = $state<string[]>([]);
	let pol = $state<Record<string, Policies>>({});
	let days = $state(7);
	let readTime = $state(false);
	let timeAck = $state(false);
	let cfNames = $state<Record<string, Map<string, string>>>({});

	let busy = $state(false);
	let error = $state<unknown>(null);
	let taken = $state<{ runtime_id: string; label: string } | null>(null);
	let done = $state<CreatedRole | null>(null);
	let op = newOperationId();

	// §6.18: the wizard opens after a step-up (cancelling goes back to the
	// list). The final create still goes through stepped() in case the 15
	// minutes run out while the person fills it in.
	let unlocked = $state(steppedUpRecently());
	onMount(() => {
		if (unlocked) return;
		let gone = false;
		const asking = askPassword('建立 AI 角色需要確認身分');
		const mine = stepUp.resolve;
		asking.then((ok) => {
			if (gone) return;
			if (ok) unlocked = true;
			else goto('/ai-team', { replace: true });
		});
		// Leaving (back button) while the dialog is open closes it; it asked for this page.
		return () => {
			gone = true;
			if (stepUp.resolve === mine) mine?.(false);
		};
	});

	$effect(() => {
		Promise.all([api('GET /ai-team/templates'), api('GET /agent-clients'), api('GET /me/agent-runtimes'), api('GET /delegations')])
			.then(([t, c, r, d]) => {
				templates = t;
				clients = c;
				runtimes = r;
				delegations = d;
				if (c.length === 1) clientId = c[0].id;
			})
			.catch((e) => (loadError = e));
	});
	const spec = $derived(templates.find((t) => t.template === template) ?? null);
	const tplPolicies = (t: TemplateSpec): Policies => JSON.parse(JSON.stringify(t.policies));
	function chooseTemplate(t: string) {
		if (t !== template) {
			template = t;
			roleKey = DEFAULT_KEY[t] ?? '';
			const s = templates.find((x) => x.template === t);
			readTime = !!s?.read_own_time;
			timeAck = false;
			pol = {};
		}
	}

	// ③ runtimes: external, not retired. The software already in one (an active delegation says so) cannot get a second (Q2).
	const external = $derived(runtimes.filter((r) => r.kind !== 'builtin' && !r.retired_at && r.context_id));
	const live = (x: DelegationView) => !x.revoked_at && Date.parse(x.expires_at) > Date.now();
	const clientRuntime = $derived.by(() => {
		const rid = delegations.find((x) => x.agent_client_id === clientId && live(x))?.runtime_id;
		return external.find((r) => r.id === rid) ?? null;
	});
	$effect(() => {
		if (step === 3 && clientRuntime && rtMode === 'new') {
			rtMode = 'existing';
			rtId = clientRuntime.id;
		}
	});
	const chosenRt = $derived(rtMode === 'existing' ? (external.find((r) => r.id === rtId) ?? null) : null);
	const sharedText = $derived(chosenRt ? sharedRuntimeText({ roles: chosenRt.roles.filter((r) => !r.retired).map((r) => r.handle), sources: chosenRt.sources, time_readers: chosenRt.time_readers }) : '');
	const clientName = $derived(clients.find((c) => c.id === clientId)?.name ?? '');

	// ④ projects
	$effect(() => {
		for (const pid of picked) {
			if (!pol[pid] && spec) pol[pid] = tplPolicies(spec);
			if (!cfNames[pid]) customFieldNames(pid).then((m) => (cfNames[pid] = m)).catch(() => (cfNames[pid] = new Map()));
		}
	});
	const fname = (pid: string, f: string) => cfNames[pid]?.get(f) ?? fieldLabel(f);
	const projectKey = (pid: string) => session.projects.find((p) => p.id === pid)?.key ?? pid;
	function setMode(pid: string, kind: 'commands' | 'fields', key: string, m: PolicyMode) {
		pol[pid] = { ...pol[pid], [kind]: { ...pol[pid][kind], [key]: m } };
	}

	const can = $derived.by(() => {
		if (step === 1) return !!spec;
		if (step === 2) return ROLE_KEY.test(roleKey) && duties.length <= 500 && !!clientId;
		if (step === 3) return rtMode === 'existing' ? !!chosenRt : !clientRuntime && rtLabel.trim().length >= 1 && rtLabel.trim().length <= 60 && isolationAck;
		if (step === 4) return picked.length > 0 && picked.every((p) => pol[p] && anyAllowed(pol[p])) && days >= 1 && days <= 30;
		if (step === 5) return !readTime || timeAck;
		return true;
	});

	function body() {
		return {
			template,
			role_key: roleKey,
			duties,
			client_id: clientId,
			runtime: rtMode === 'existing' ? { id: rtId } : { new: { label: rtLabel.trim(), isolation_ack: true } },
			delegations: picked.map((pid) => ({ project_id: pid, expires_in_days: days, policies: spec ? changesFrom(tplPolicies(spec), pol[pid]) : pol[pid] })),
			read_own_time: readTime,
			read_own_time_ack: readTime ? timeAck : undefined
		};
	}
	async function create() {
		busy = true;
		error = null;
		taken = null;
		try {
			const r = await stepped(() => api('POST /ai-team', { body: body(), operationId: op }), '為了發出 agent 的 token，請再輸入一次密碼（整個精靈只問這一次）');
			if (!r) return;
			done = r;
			refreshCounts();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'runtime_client_taken') {
				const d = (e.body?.details ?? {}) as { runtime_id?: string; label?: string };
				taken = { runtime_id: d.runtime_id ?? '', label: d.label ?? '另一個執行環境' };
			}
			error = e;
			// A refused request changed nothing: the next try is a new request.
			if (e instanceof ApiError && !e.retryable) op = newOperationId();
		} finally {
			busy = false;
		}
	}
	const token = $derived<IssuedCredential | null>(done?.token ? { id: done.credential_id, token: done.token, expires_at: done.token_expires_at, agent_principal_id: done.principal.id, context_id: '' } : null);
</script>

<svelte:head><title>建立 AI 角色・oxledger</title></svelte:head>
<div class="page">
	<a href="/ai-team" class="hint">← AI 團隊</a>
	<h1>建立 AI 角色</h1>
	<ErrorBox error={loadError} />
	{#if done}
		<div class="card" data-testid="wizard-done">
			<b>已建立 <span class="agname">{done.principal.handle}</span>（執行環境：{done.runtime.label}）</b>
			{#each done.warnings as w, i (i)}<div class="banner w">{sharedRuntimeText(w)}</div>{/each}
			{#if token}
				<TokenOnce {token} onclose={() => (location.href = `/ai-team/${encodeURIComponent(done!.principal.handle)}`)} />
			{:else}
				<p class="hint">token 已在第一次建立時顯示過；遺失請在角色頁發新 token。</p>
			{/if}
			<a class="btn" href="/ai-team/{encodeURIComponent(done.principal.handle)}" data-testid="go-role">前往角色頁</a>
		</div>
	{:else if !unlocked}
		<p class="hint" data-testid="wizard-locked">確認身分後才能建立角色。</p>
	{:else}
		<div class="steps" data-testid="steps">
			{#each STEPS as s, i (s)}<span class:on={step === i + 1} class:done={step > i + 1}>{i + 1}. {s}</span>{/each}
		</div>

		{#if step === 1}
			<div class="tcards">
				{#each WIZARD_TEMPLATES as t (t)}
					{@const sp = templates.find((x) => x.template === t)}
					{@const m = sp ? byMode(sp.policies as Policies) : null}
					<label class="card tcard" class:on={template === t} data-testid="tpl-{t}">
						<input type="radio" name="tpl" checked={template === t} onchange={() => chooseTemplate(t)} />
						<b>{TEMPLATE_LABEL[t]}</b>
						<div class="hint">{TEMPLATE_DUTY[t]}</div>
						{#if m}
							<div style="font-size:12.5px">直接寫入：{m.direct.map(commandLabel).join('、') || '—'}</div>
							<div style="font-size:12.5px">需要核准：{m.review.map(commandLabel).join('、') || '—'}</div>
						{/if}
					</label>
				{/each}
			</div>
		{:else if step === 2}
			<div class="card">
				<label class="fl">名稱（role key）<input bind:value={roleKey} maxlength="30" data-testid="role-key" /></label>
				<div class="hint">小寫英文開頭，之後可用小寫英文、數字、底線，最多 30 字。handle 會是「名稱@你的帳號」。</div>
				{#if roleKey && !ROLE_KEY.test(roleKey)}<div class="hint" style="color:var(--bad)">格式不對。</div>{/if}
				<label class="fl" style="margin-top:8px">職責<textarea bind:value={duties} maxlength="500" rows="3" data-testid="role-duties"></textarea></label>
				<div class="hint">只有你和站台管理員看得到。{duties.length}/500</div>
				<div style="margin-top:8px"><b>agent 軟體</b></div>
				{#each clients as c (c.id)}
					<label class="row" style="gap:6px"><input type="radio" name="client" value={c.id} bind:group={clientId} data-testid="client" /> {c.name}</label>
				{:else}
					<div class="empty">管理員還沒有啟用任何 agent 軟體。</div>
				{/each}
			</div>
		{:else if step === 3}
			<div class="card" data-testid="runtime-step">
				<label class="row" style="gap:6px"><input type="radio" name="rt" value="existing" bind:group={rtMode} disabled={!external.length} data-testid="rt-existing" /> 放在既有的執行環境</label>
				{#if rtMode === 'existing'}
					<select bind:value={rtId} aria-label="執行環境" style="margin-left:22px" data-testid="rt-select">
						<option value="">選擇…</option>
						{#each external as r (r.id)}<option value={r.id}>{r.label}（{r.roles.filter((x) => !x.retired).map((x) => x.handle).join('、') || '沒有角色'}）</option>{/each}
					</select>
					{#if sharedText}<div class="banner w" data-testid="shared-warning">{sharedText}</div>{/if}
				{/if}
				<label class="row" style="gap:6px;margin-top:8px"><input type="radio" name="rt" value="new" bind:group={rtMode} disabled={!!clientRuntime} data-testid="rt-new" /> 新的執行環境</label>
				{#if clientRuntime}
					<div class="hint" style="margin-left:22px" data-testid="rt-taken">同一個 agent 軟體只能有一個執行環境。要從頭開始，請在〈{clientRuntime.label}〉建新工作脈絡，或先退役它。</div>
				{:else if rtMode === 'new'}
					<div style="margin-left:22px">
						<label class="fl">名稱<input bind:value={rtLabel} maxlength="60" placeholder="例如 Alice 的筆電" data-testid="rt-label" /></label>
						<label class="row" style="gap:6px;align-items:flex-start;margin-top:6px"><input type="checkbox" bind:checked={isolationAck} data-testid="rt-ack" /> 這個執行環境不和我其他的 agent 共用電腦、檔案、記憶或帳號</label>
						<div class="hint">oxledger 無法驗證這件事；宣告不實時，限制會失效。</div>
					</div>
				{/if}
			</div>
		{:else if step === 4}
			<div class="card">
				<b>委派哪些專案？</b>
				{#each session.projects as p (p.id)}
					<label class="row" style="gap:6px"><input type="checkbox" value={p.id} bind:group={picked} data-testid="project-{p.key}" /> {p.key}・{p.name}</label>
				{/each}
				<label class="row" style="gap:6px;margin-top:8px">委派期限 <input type="number" min="1" max="30" bind:value={days} style="width:70px" data-testid="days" /> 天（最多 30）</label>
			</div>
			{#each picked as pid (pid)}
				{#if pol[pid]}
					<div class="card" data-testid="wizard-policy" data-project={projectKey(pid)}>
						<div class="row" style="gap:6px">
							<b>{projectKey(pid)}</b><span style="flex:1"></span>
							<button class="btn sm" onclick={() => (pol[pid] = setAll(pol[pid], 'review'))} data-testid="allowed-review">允許的改成需要核准</button>
							<button class="btn sm" onclick={() => (pol[pid] = setAll(pol[pid], 'review', true))} data-testid="all-review">全部需要核准</button>
							<button class="btn sm ghost" onclick={() => spec && (pol[pid] = tplPolicies(spec))}>恢復範本</button>
						</div>
						<PolicyTable policies={pol[pid]} fieldName={(f) => fname(pid, f)} onpick={(k, key, m) => setMode(pid, k, key, m)} name="w-{pid}" />
						{#if !anyAllowed(pol[pid])}<div class="hint" style="color:var(--bad)">至少要允許一個操作。</div>{/if}
					</div>
				{/if}
			{/each}
		{:else if step === 5}
			<div class="card">
				<label class="row" style="gap:6px"><input type="checkbox" bind:checked={readTime} onchange={() => (timeAck = false)} data-testid="read-time" /> 讓 agent 讀我自己的工時</label>
				{#if readTime}
					<div class="warnbox" data-testid="time-warning">
						<b>⚠ {TIME_WARNING.title}</b>
						{#each TIME_WARNING.body as p (p)}<p>{p}</p>{/each}
						<label class="row" style="gap:6px"><input type="checkbox" bind:checked={timeAck} data-testid="time-ack" /> {TIME_WARNING.ack}</label>
					</div>
				{/if}
			</div>
		{:else}
			<div class="card" data-testid="confirm">
				<b>確認</b>
				<ul>
					<li>角色：{roleKey}@{session.user?.login ?? '你'}（{TEMPLATE_LABEL[template]}）・agent 軟體：{clientName}</li>
					<li>執行環境：{rtMode === 'existing' ? `${chosenRt?.label}（既有）` : `${rtLabel.trim()}（新）`}</li>
					<li>讀我的工時：{readTime ? '是' : '否'}・期限 {days} 天</li>
				</ul>
				{#if sharedText}<div class="banner w">{sharedText}</div>{/if}
				{#each picked as pid (pid)}
					{@const m = byMode(pol[pid])}
					<div style="margin-top:6px" data-testid="confirm-project"><b>{projectKey(pid)}</b>：直接寫入 {m.direct.map(commandLabel).join('、') || '—'}；需要核准 {m.review.map(commandLabel).join('、') || '—'}</div>
					<!-- §6.18 ⑥: the final whole table, read-only (commands, and 修改欄位's fields under 逐欄設定). -->
					<details open class="fulltable" data-testid="confirm-table" data-project={projectKey(pid)}>
						<summary>{projectKey(pid)} 的完整政策表</summary>
						<PolicyTable policies={pol[pid]} fieldName={(f) => fname(pid, f)} onpick={() => {}} disabled name="c-{pid}" />
					</details>
				{/each}
				<p class="hint">讀不到私有內容、不能擴大你的權限；直接寫入不保證內容正確，可在活動頁回復。</p>
				{#if taken}
					<div class="banner w" data-testid="client-taken">
						這個 agent 軟體已經在〈{taken.label}〉。
						<button class="btn sm" onclick={() => ((rtMode = 'existing'), (rtId = taken!.runtime_id), (step = 3), (taken = null), (error = null))}>放進那個執行環境</button>
					</div>
				{/if}
				<ErrorBox {error} />
			</div>
		{/if}

		<div class="row" style="gap:8px;margin-top:10px">
			{#if step > 1}<button class="btn" onclick={() => step--} disabled={busy}>上一步</button>{/if}
			{#if step < STEPS.length}
				<button class="btn pri" onclick={() => step++} disabled={!can} data-testid="next">下一步</button>
			{:else}
				<button class="btn pri" onclick={create} disabled={busy} data-testid="create-role">{busy ? '建立中…' : '建立角色'}</button>
			{/if}
		</div>
	{/if}
</div>

<style>
	.tcards { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 10px; }
	.tcard { cursor: pointer; display: flex; flex-direction: column; gap: 4px; }
	.tcard.on { border-color: var(--act); background: var(--act-a); }
	.tcard input { position: absolute; opacity: 0; pointer-events: none; }
	.tcard { position: relative; }
	.fl { display: flex; flex-direction: column; gap: 3px; }
	.warnbox { border: 2px solid var(--warn, #c77700); border-radius: 8px; padding: 8px 10px; margin-top: 8px; }
	.warnbox p { margin: 4px 0; font-size: 13px; }
	.fulltable { margin: 4px 0 8px; }
	.fulltable summary { cursor: pointer; font-size: 13px; color: var(--dim, inherit); }
</style>

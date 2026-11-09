<script lang="ts">
	// 委派精靈 (§15.2, §16.10): agent 軟體 → 專案 → 操作表 → 確認, with one
	// step-up for the whole wizard, and the token plus MCP settings on the
	// same page at the end. Adding a project to an agent that already has a
	// work context keeps its token (no new MCP setup).
	import { page } from '$app/state';
	import type { AgentClientView, ContextView, DelegationView, IssuedCredential, PolicyMode } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { COMMAND_LABELS, MODE_LABELS, fieldLabel } from '$lib/format';
	import { refreshCounts } from '$lib/counts.svelte';
	import { reauthenticate, session, steppedUpRecently } from '$lib/session.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import TokenOnce from '$lib/components/TokenOnce.svelte';
	import { customFieldNames } from '$lib/trackers';

	const COMMANDS = ['create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of'];
	const BUILTIN = ['subject', 'description', 'assigned_to_id', 'priority_id', 'start_date', 'due_date', 'fixed_version_id'];
	const MODES: PolicyMode[] = ['direct', 'review', 'off'];

	let step = $state(1);
	let clients = $state<AgentClientView[]>([]);
	let contexts = $state<ContextView[]>([]);
	let delegations = $state<DelegationView[]>([]);
	let clientId = $state(page.url.searchParams.get('agent') ?? '');
	let projectId = $state('');
	let commands = $state<Record<string, PolicyMode>>(Object.fromEntries(COMMANDS.map((c) => [c, 'direct'])));
	let fieldModes = $state<Record<string, PolicyMode>>({});
	let customFields = $state<{ key: string; name: string }[]>([]);
	let showFields = $state(false);
	let days = $state(7);
	let password = $state('');
	let askPassword = $state(false);
	let busy = $state(false);
	let error = $state<unknown>(null);
	let loadError = $state<unknown>(null);
	let done = $state<{ delegation: DelegationView; token: IssuedCredential | null } | null>(null);

	$effect(() => {
		Promise.all([api('GET /agent-clients'), api('GET /agent-contexts'), api('GET /delegations')])
			.then(([c, x, d]) => {
				clients = c;
				contexts = x;
				delegations = d;
				if (!clientId && c.length === 1) clientId = c[0].id;
			})
			.catch((e) => (loadError = e));
	});

	const activeContext = $derived(contexts.find((c) => c.agent_client_id === clientId && c.state === 'active') ?? null);
	const delegated = $derived(new Set(delegations.filter((d) => activeContext && d.context_id === activeContext.id && !d.revoked_at && Date.parse(d.expires_at) > Date.now()).map((d) => d.project_id)));
	const choosable = $derived(session.projects.filter((p) => !delegated.has(p.id)));
	$effect(() => {
		if (step === 2 && !projectId && choosable.length === 1) projectId = choosable[0].id;
	});
	const project = $derived(session.projects.find((p) => p.id === projectId) ?? null);

	// Custom fields of the project (every tracker's schema).
	$effect(() => {
		const pid = projectId;
		if (!pid) return;
		customFieldNames(pid)
			.then((m) => (customFields = [...m].map(([key, name]) => ({ key, name }))))
			.catch(() => (customFields = []));
	});
	const allFields = $derived([...BUILTIN.map((k) => ({ key: k, name: fieldLabel(k) })), ...customFields]);
	const fieldMode = (k: string) => fieldModes[k] ?? 'direct';
	function all(m: PolicyMode) {
		for (const c of COMMANDS) commands[c] = m;
		for (const f of allFields) fieldModes[f.key] = m;
	}

	function body() {
		const ops = COMMANDS.filter((c) => commands[c] !== 'off');
		const writable = commands.update_fields === 'off' ? [] : allFields.map((f) => f.key).filter((k) => fieldMode(k) !== 'off');
		const policies = {
			commands: Object.fromEntries(ops.filter((c) => commands[c] === 'review').map((c) => [c, 'review' as PolicyMode])),
			fields: Object.fromEntries(writable.filter((k) => fieldMode(k) === 'review').map((k) => [k, 'review' as PolicyMode]))
		};
		return { agent_client_id: clientId, project_id: projectId, proposal_ops: ops, writable_fields: writable, expires_in_days: days, policies };
	}
	const summary = $derived.by(() => {
		const by = (m: PolicyMode) => COMMANDS.filter((c) => commands[c] === m).map((c) => COMMAND_LABELS[c]);
		return { direct: by('direct'), review: by('review'), off: by('off') };
	});

	async function create() {
		error = null;
		if (!steppedUpRecently() && !askPassword) {
			askPassword = true;
			return;
		}
		busy = true;
		try {
			if (askPassword) {
				await reauthenticate(password);
				password = '';
				askPassword = false;
			}
			const hadContext = !!activeContext;
			const d = await api('POST /delegations', { body: body() });
			// A new work context needs its first token; an existing one keeps its token.
			const token = hadContext ? null : await api('POST /agent-contexts/{id}/credentials', { path: { id: d.context_id }, body: { label: 'spa' } });
			done = { delegation: d, token };
			refreshCounts();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'reauthentication_required') askPassword = true;
			error = e;
		} finally {
			busy = false;
		}
	}
	const clientName = $derived(clients.find((c) => c.id === clientId)?.name ?? '');
</script>

<svelte:head><title>委派給 agent・oxledger</title></svelte:head>
<div class="page">
	<a href="/agents" class="hint">← 我的 agent 委派</a>
	<h1>委派給 agent</h1>
	<ErrorBox error={loadError} />
	{#if done}
		<div class="card" data-testid="wizard-done">
			<b>已委派 {project?.key} 給 {clientName}</b>
			<div class="hint">到期 {new Date(done.delegation.expires_at).toLocaleString('zh-TW', { timeZone: 'Asia/Taipei' })}</div>
			{#if done.token}
				<TokenOnce token={done.token} onclose={() => (location.href = '/agents')} />
			{:else}
				<p class="hint">這個 agent 已經有 token；沿用原本的 MCP 設定即可，不用重設。</p>
				<a class="btn" href="/agents">回到委派頁</a>
			{/if}
		</div>
	{:else}
		<div class="steps" data-testid="steps">
			{#each ['agent 軟體', '專案', '可以做什麼', '確認'] as s, i (s)}<span class:on={step === i + 1} class:done={step > i + 1}>{i + 1}. {s}</span>{/each}
		</div>

		{#if step === 1}
			<div class="card">
				<b>要委派給哪個 agent 軟體？</b>
				{#each clients as c (c.id)}
					<label class="row" style="gap:6px"><input type="radio" name="client" value={c.id} bind:group={clientId} /> {c.name}{contexts.some((x) => x.agent_client_id === c.id && x.state === 'active') ? '（已委派過，會加入同一個工作脈絡）' : ''}</label>
				{:else}
					<div class="empty">管理員還沒有啟用任何 agent 軟體。</div>
				{/each}
				<button class="btn pri" style="margin-top:8px" disabled={!clientId} onclick={() => (step = 2)} data-testid="next">下一步</button>
			</div>
		{:else if step === 2}
			<div class="card">
				<b>委派哪個專案？</b>
				{#each choosable as p (p.id)}
					<label class="row" style="gap:6px"><input type="radio" name="project" value={p.id} bind:group={projectId} /> {p.key}・{p.name}</label>
				{:else}
					<div class="empty">你可以看到的專案都已經委派給這個 agent。</div>
				{/each}
				{#if activeContext && delegated.size}
					<p class="hint" data-testid="direction">加入後，agent 的新寫入只能寫到讀者不比它讀過的專案更多的地方（例如加入私有專案後，就不能再寫回公開專案）。伺服器會在寫入時檢查；第一版還不能在這裡逐專案預覽。</p>
				{/if}
				<div class="row" style="gap:8px;margin-top:8px">
					<button class="btn" onclick={() => (step = 1)}>上一步</button>
					<button class="btn pri" disabled={!projectId} onclick={() => (step = 3)} data-testid="next">下一步</button>
				</div>
			</div>
		{:else if step === 3}
			<div class="card">
				<div class="row" style="gap:8px">
					<b>{project?.key} 裡 agent 可以做什麼</b>
					<span style="flex:1"></span>
					<button class="btn sm" onclick={() => all('direct')}>全部直接寫入（預設）</button>
					<button class="btn sm" onclick={() => all('review')} data-testid="all-review">全部需要核准</button>
				</div>
				<table class="t" style="margin-top:6px" data-testid="ops-table">
					<thead><tr><th>操作</th>{#each MODES as m (m)}<th>{MODE_LABELS[m]}</th>{/each}</tr></thead>
					<tbody>
						{#each COMMANDS as c (c)}
							<tr>
								<td>{COMMAND_LABELS[c]}{#if c === 'update_fields'} <button class="btn sm ghost" onclick={() => (showFields = !showFields)}>{showFields ? '收合欄位' : '逐欄設定'}</button>{/if}</td>
								{#each MODES as m (m)}<td><input type="radio" name="cmd-{c}" checked={commands[c] === m} onchange={() => (commands[c] = m)} aria-label="{COMMAND_LABELS[c]}：{MODE_LABELS[m]}" /></td>{/each}
							</tr>
							{#if c === 'update_fields' && showFields && commands.update_fields !== 'off'}
								{#each allFields as f (f.key)}
									<tr class="hint">
										<td style="padding-left:18px">└ {f.name}</td>
										{#each MODES as m (m)}<td><input type="radio" name="f-{f.key}" checked={fieldMode(f.key) === m} onchange={() => (fieldModes[f.key] = m)} aria-label="{f.name}：{MODE_LABELS[m]}" /></td>{/each}
									</tr>
								{/each}
							{/if}
						{/each}
					</tbody>
				</table>
				<label class="row" style="gap:6px;margin-top:8px">委派期限 <input type="number" min="1" max="30" bind:value={days} style="width:70px" /> 天（最多 30）</label>
				<div class="row" style="gap:8px;margin-top:8px">
					<button class="btn" onclick={() => (step = 2)}>上一步</button>
					<button class="btn pri" disabled={!COMMANDS.some((c) => commands[c] !== 'off') || days < 1 || days > 30} onclick={() => (step = 4)} data-testid="next">下一步</button>
				</div>
			</div>
		{:else}
			<div class="card" data-testid="confirm">
				<b>確認</b>
				<ul>
					<li>agent：{clientName}{activeContext ? '（加入既有工作脈絡，沿用原本的 token）' : '（第一次委派，完成後會顯示 token）'}</li>
					<li>專案：{project?.key}・{project?.name}</li>
					{#if summary.direct.length}<li>直接寫入：{summary.direct.join('、')}</li>{/if}
					{#if summary.review.length}<li>需要你核准：{summary.review.join('、')}</li>{/if}
					{#if summary.off.length}<li>不允許：{summary.off.join('、')}</li>{/if}
					<li>期限：{days} 天</li>
				</ul>
				<p class="hint">{summary.direct.length ? 'agent 可以不經你逐筆核准就修改。' : '每個寫入都要你核准才會套用。'}讀不到私有內容、不能擴大權限；直接寫入不保證內容正確，可在活動頁回復。</p>
				{#if askPassword}
					<div class="inline-form" data-testid="step-up">
						<span>為了發出新的 agent 憑證，請再輸入一次密碼（整個精靈只問這一次）</span>
						<input type="password" bind:value={password} autocomplete="current-password" aria-label="密碼" data-testid="step-up-password" />
					</div>
				{/if}
				<ErrorBox {error} />
				<div class="row" style="gap:8px;margin-top:8px">
					<button class="btn" onclick={() => (step = 3)} disabled={busy}>上一步</button>
					<button class="btn pri" onclick={create} disabled={busy || (askPassword && !password)} data-testid="create-delegation">{busy ? '建立中…' : askPassword ? '確認密碼並建立' : '建立委派'}</button>
				</div>
			</div>
		{/if}
	{/if}
</div>

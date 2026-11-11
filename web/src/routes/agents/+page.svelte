<script lang="ts">
	// 我的 agent 委派 (§15.2, §16.10). Per agent: its work context, the
	// projects delegated, what it may do there (direct / needs approval / not
	// allowed), and three separate ways to stop it: 暫停寫入 (reversible),
	// 撤銷 token, 停用這個 agent (closes the context). Revoking one project's
	// delegation does not lift the write restriction; only a new work context
	// does (U14).
	import type { ContextView, DelegationView, IssuedCredential, PolicyMode, PolicyView } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { fmtShort } from '$lib/format';
	import { refreshCounts } from '$lib/counts.svelte';
	import { projectById, reauthenticate, steppedUpRecently } from '$lib/session.svelte';
	import { toast } from '$lib/toast.svelte';
	import { COMMAND_LABELS, MODE_LABELS, fieldLabel } from '$lib/format';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import TokenOnce from '$lib/components/TokenOnce.svelte';
	import { customFieldNames } from '$lib/trackers';

	let contexts = $state<ContextView[]>([]);
	let delegations = $state<DelegationView[]>([]);
	let policies = $state<Record<string, PolicyView>>({});
	let cfNames = $state<Record<string, Map<string, string>>>({});
	let loadError = $state<unknown>(null);
	let actionError = $state<unknown>(null);
	let busy = $state(false);
	let issued = $state<IssuedCredential | null>(null);
	/** Tokens issued on this page (the server has no token list, gap G14). */
	let issuedHere = $state<{ id: string; context: string; at: string; revoked?: boolean }[]>([]);
	let password = $state('');
	let needPassword = $state<null | (() => Promise<void>)>(null);
	let ack = $state(false);
	let freshFor = $state<string | null>(null);

	async function load() {
		loadError = null;
		try {
			const [c, d] = await Promise.all([api('GET /agent-contexts'), api('GET /delegations')]);
			contexts = c;
			delegations = d;
			const live = d.filter((x) => !x.revoked_at && Date.parse(x.expires_at) > Date.now());
			const p: Record<string, PolicyView> = {};
			await Promise.all(live.map(async (x) => (p[x.id] = await api('GET /delegations/{id}/policies', { path: { id: x.id } }).catch(() => null as never))));
			policies = p;
			const names: Record<string, Map<string, string>> = {};
			await Promise.all([...new Set(live.map((x) => x.project_id))].map(async (pid) => (names[pid] = await customFieldNames(pid).catch(() => new Map()))));
			cfNames = names;
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		load();
	});

	const byContext = (cid: string) => delegations.filter((d) => d.context_id === cid);
	const live = (d: DelegationView) => !d.revoked_at && Date.parse(d.expires_at) > Date.now();
	const projectKey = (pid: string) => projectById(pid)?.key ?? '（看不到的專案）';
	const fname = (pid: string, f: string) => cfNames[pid]?.get(f) ?? fieldLabel(f);

	async function act(fn: () => Promise<unknown>, done: string) {
		busy = true;
		actionError = null;
		try {
			await fn();
			toast(done, 'ok');
			await load();
			refreshCounts();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'reauthentication_required') {
				needPassword = () => act(fn, done);
			} else actionError = e;
		} finally {
			busy = false;
		}
	}
	/** Step-up once, then continue what the person asked for (§15.2). */
	async function withStepUp(fn: () => Promise<void>) {
		if (steppedUpRecently()) return fn();
		needPassword = fn;
	}
	async function confirmPassword() {
		const next = needPassword;
		busy = true;
		actionError = null;
		try {
			await reauthenticate(password);
			password = '';
			needPassword = null;
			await next?.();
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}

	const suspend = (c: ContextView) => act(() => api('POST /agent-contexts/{id}/suspend', { path: { id: c.id }, body: {} }), `已暫停 ${c.agent_label} 的寫入（讀取照常）`);
	const resume = (c: ContextView) => act(() => api('POST /agent-contexts/{id}/resume', { path: { id: c.id }, body: {} }), `已恢復 ${c.agent_label} 的寫入`);
	const close = (c: ContextView) => act(() => api('POST /agent-contexts/{id}/close', { path: { id: c.id }, body: {} }), `已停用 ${c.agent_label}（這個工作脈絡與它的委派都結束）`);
	const revokeDelegation = (d: DelegationView) => act(() => api('POST /delegations/{id}/revoke', { path: { id: d.id }, body: {} }), `已撤銷 ${projectKey(d.project_id)} 的委派`);
	function newToken(c: ContextView) {
		return withStepUp(() =>
			act(async () => {
				issued = await api('POST /agent-contexts/{id}/credentials', { path: { id: c.id }, body: { label: 'spa' } });
				issuedHere = [{ id: issued.id, context: c.id, at: new Date().toISOString() }, ...issuedHere];
			}, '已發新 token（舊 token 仍可用，記得撤銷）')
		);
	}
	const revokeToken = (t: { id: string }) =>
		act(async () => {
			await api('POST /agent-credentials/{id}/revoke', { path: { id: t.id }, body: {} });
			issuedHere = issuedHere.map((x) => (x.id === t.id ? { ...x, revoked: true } : x));
		}, '已撤銷 token（不影響現有提案）');
	function freshContext(c: ContextView) {
		freshFor = c.id;
		ack = false;
	}
	function confirmFresh(c: ContextView) {
		return withStepUp(() =>
			act(async () => {
				await api('POST /agent-contexts', { body: { agent_client_id: c.agent_client_id, fresh_context_ack: true } });
				freshFor = null;
			}, '已建立新工作脈絡；舊脈絡與它的委派已結束。請重新加入專案並發新 token。')
		);
	}

	async function setMode(d: DelegationView, kind: 'commands' | 'fields', key: string, mode: PolicyMode) {
		const p = policies[d.id];
		if (!p) return;
		const next = { commands: { ...p.commands }, fields: { ...p.fields } };
		next[kind][key] = mode;
		await act(async () => {
			policies[d.id] = await api('PUT /delegations/{id}/policies', { path: { id: d.id }, body: next });
		}, '已更新政策');
	}
	const SUSPEND: Record<string, string> = { manual: '你手動暫停', issue_write_burst: '短時間內改了太多張單，自動暫停', version_write_burst: '短時間內改了太多里程碑，自動暫停' };
	const MODES: PolicyMode[] = ['direct', 'review', 'off'];
</script>

<svelte:head><title>agent 委派・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px">
		<h1 style="margin:0">我的 agent 委派</h1>
		<span style="flex:1"></span>
		<a class="btn pri" href="/agents/new" data-testid="new-delegation">委派給 agent</a>
	</div>
	<p class="hint">agent 以你的名義、在你委派的專案裡工作。它讀不到私有內容、不能擴大權限；直接寫入不保證內容正確，可在活動頁回復。</p>
	<ErrorBox error={loadError} onaction={load} />
	<ErrorBox error={actionError} onaction={load} />

	{#if needPassword}
		<div class="card" data-testid="step-up">
			<b>為了發出新的 agent 憑證，請再輸入一次密碼</b>
			<form class="inline-form" onsubmit={(e) => (e.preventDefault(), confirmPassword())}>
				<input type="password" bind:value={password} autocomplete="current-password" aria-label="密碼" data-testid="step-up-password" />
				<button class="btn pri" disabled={busy || !password}>確認</button>
				<button type="button" class="btn" onclick={() => (needPassword = null)}>取消</button>
			</form>
		</div>
	{/if}
	{#if issued}<TokenOnce token={issued} onclose={() => (issued = null)} />{/if}

	{#if !contexts.length && !loadError}
		<div class="empty">還沒有委派給任何 agent。</div>
	{/if}
	{#each contexts as c (c.id)}
		<div class="card" data-testid="context" data-label={c.agent_label}>
			<div class="row" style="gap:8px">
				<b class="agname">{c.agent_label}</b><span class="agent-label">agent</span>
				<span class="chip {c.state === 'active' ? 'c-ok' : 'c-dim'}">{c.state === 'active' ? '使用中' : c.changed_projects.length ? '已停用（權限設定變更）' : '已停用'}</span>
				{#if c.suspended_at}<span class="chip c-warn" data-testid="suspended">寫入已暫停</span>{/if}
				<span style="flex:1"></span>
				<a class="btn sm" href="/agents/{c.agent_principal_id}/activity">活動</a>
			</div>
			{#if c.suspended_at}
				<div class="banner w">{SUSPEND[c.suspension_reason ?? ''] ?? c.suspension_reason}・{fmtShort(c.suspended_at)}。暫停只擋寫入，讀取照常。</div>
			{/if}
			{#if c.state !== 'active' && c.changed_projects.length}
				<div class="banner w" data-testid="context-changed">這個 agent 的工作脈絡因 {c.changed_projects.map(projectKey).join('、')} 的權限設定變更而停用，它的提案都不能套用。</div>
			{/if}

			{#if c.state === 'active'}
				<div class="row" style="gap:8px;margin:6px 0">
					{#if c.suspended_at}
						<button class="btn sm" onclick={() => resume(c)} disabled={busy} data-testid="resume">恢復寫入</button>
					{:else}
						<button class="btn sm" onclick={() => suspend(c)} disabled={busy} data-testid="suspend" title="可恢復；讀取照常">暫停寫入</button>
					{/if}
					<button class="btn sm" onclick={() => newToken(c)} disabled={busy} data-testid="new-token" title="不影響現有提案；不需要新工作脈絡宣告">發新 token</button>
					<a class="btn sm" href="/agents/new?agent={c.agent_client_id}">加入專案</a>
					<button class="btn sm" onclick={() => close(c)} disabled={busy} data-testid="close-context" title="這個工作脈絡與它的所有委派都結束；之後要重新委派">停用這個 agent</button>
				</div>
			{:else}
				<div class="row" style="gap:8px;margin:6px 0">
					<button class="btn sm" onclick={() => freshContext(c)} disabled={busy} data-testid="fresh-context">建立新工作脈絡</button>
				</div>
				{#if freshFor === c.id}
					<div class="card" data-testid="fresh-ack">
						<label class="row" style="gap:6px;align-items:flex-start"><input type="checkbox" bind:checked={ack} /> 我確認這個 agent 不會帶著舊工作脈絡讀到的內容繼續工作（例如已清掉對話與暫存檔）。伺服器無法驗證這件事。</label>
						<button class="btn pri sm" style="margin-top:6px" disabled={!ack || busy} onclick={() => confirmFresh(c)}>建立新工作脈絡</button>
					</div>
				{/if}
			{/if}

			{#each issuedHere.filter((t) => t.context === c.id) as t (t.id)}
				<div class="row hint" style="gap:6px">token（{fmtShort(t.at)} 在這頁發的）{#if t.revoked}<span class="chip c-dim">已撤銷</span>{:else}<button class="btn sm" onclick={() => revokeToken(t)} disabled={busy}>撤銷 token</button>{/if}</div>
			{/each}

			{#each byContext(c.id) as d (d.id)}
				{@const p = policies[d.id]}
				<div class="card" style="margin:6px 0" data-testid="delegation">
					<div class="row" style="gap:8px">
						<b>{projectKey(d.project_id)}</b>
						{#if live(d)}<span class="hint">到期 {fmtShort(d.expires_at)}・一份最多 {d.max_ops} 個操作</span>{:else}<span class="chip c-dim">{d.revoked_at ? '已撤銷' : '已到期'}</span>{/if}
						<span style="flex:1"></span>
						{#if live(d) && c.state === 'active'}<button class="btn sm" onclick={() => revokeDelegation(d)} disabled={busy} data-testid="revoke-delegation" title="撤銷委派不會解除寫入限制">撤銷這個專案的委派</button>{/if}
					</div>
					{#if live(d) && p}
						<table class="t" style="margin-top:4px">
							<thead><tr><th>操作</th>{#each MODES as m (m)}<th>{MODE_LABELS[m]}</th>{/each}</tr></thead>
							<tbody>
								{#each d.proposal_ops as op (op)}
									<tr>
										<td>{COMMAND_LABELS[op] ?? op}</td>
										{#each MODES as m (m)}<td><input type="radio" name="{d.id}-{op}" checked={(p.commands[op] ?? 'direct') === m} disabled={busy || c.state !== 'active'} onchange={() => setMode(d, 'commands', op, m)} aria-label="{COMMAND_LABELS[op] ?? op}：{MODE_LABELS[m]}" /></td>{/each}
									</tr>
								{/each}
								{#if d.proposal_ops.includes('update_fields')}
									{#each d.writable_fields as f (f)}
										<tr class="hint">
											<td style="padding-left:18px">└ {fname(d.project_id, f)}</td>
											{#each MODES as m (m)}<td><input type="radio" name="{d.id}-f-{f}" checked={(p.fields[f] ?? 'direct') === m} disabled={busy || c.state !== 'active'} onchange={() => setMode(d, 'fields', f, m)} aria-label="{fname(d.project_id, f)}：{MODE_LABELS[m]}" /></td>{/each}
										</tr>
									{/each}
								{/if}
							</tbody>
						</table>
						{#if p.available_commands.length || p.available_fields.length}
							<div class="hint" data-testid="available">伺服器支援、但這個委派還沒有的：{[...p.available_commands.map((x) => COMMAND_LABELS[x] ?? x), ...p.available_fields.map((f) => fname(d.project_id, f))].join('、')}。不會自動加入；要加請重新委派這個專案。</div>
						{/if}
					{/if}
				</div>
			{/each}
			<div class="hint">撤銷委派不會解除寫入限制；要解除只能「建立新工作脈絡」。</div>
		</div>
	{/each}
</div>

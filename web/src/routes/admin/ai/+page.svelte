<script lang="ts">
	// AI 設定 (contract 05 §6.20, §3.13.2): the master switch (off by
	// default, with the reasons it cannot be turned on), the endpoints the
	// server registered (read-only), providers (D17 and, for public services,
	// the company-account confirmation), budgets, usage (counts and costs only)
	// and the audit trail. Changes to providers need a step-up.
	import type { EndpointList, ProviderEvent, ProviderView, SettingsView, Usage } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { describeError } from '$lib/errors';
	import { fmtShort, fmtTime } from '$lib/format';
	import { stepped } from '$lib/stepup.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	let settings = $state<SettingsView | null>(null);
	let endpoints = $state<EndpointList | null>(null);
	let providers = $state<ProviderView[]>([]);
	let events = $state<ProviderEvent[]>([]);
	let usage = $state<Usage | null>(null);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		loadError = null;
		try {
			const [s, e, p, ev, u] = await Promise.all([api('GET /admin/ai/settings'), api('GET /admin/ai/endpoints'), api('GET /admin/ai/providers'), api('GET /admin/ai/events'), api('GET /admin/ai/usage')]);
			settings = s;
			endpoints = e;
			providers = p.items;
			events = ev.items.slice(0, 50);
			usage = u;
			form = { budget: s.monthly_budget_usd ?? '', daily: s.user_daily_budget_usd, perHour: s.user_runs_per_hour, retention: s.conversation_retention_days };
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		load();
	});

	async function act<T>(fn: () => Promise<T>, done: string, why?: string) {
		busy = true;
		error = null;
		try {
			const r = why ? await stepped(fn, why) : await fn();
			if (r === null) return;
			toast(done, 'ok');
			await load();
			return r;
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}

	// ---- switch and budgets ----
	let form = $state({ budget: '', daily: '2.00', perHour: 30, retention: 30 });
	const BLOCKER: Record<string, string> = {
		no_provider: '還沒有使用中的服務',
		d17_not_attested: '還沒確認 D17',
		not_tested: '使用中的服務還沒測試成功',
		test_failed: '上次測試失敗',
		no_budget: '沒有月預算',
		org_account_not_attested: '外部服務還沒確認用公司帳號',
		endpoint_unregistered: '端點已不在伺服器設定中'
	};
	const active = $derived(providers.find((p) => p.active) ?? null);
	function blocker(b: string) {
		if (b === 'test_failed' && active) {
			const r = (active.last_test_result ?? {}) as { kind?: string; status?: number };
			return `上次測試失敗：${r.kind === 'http_status' ? `HTTP ${r.status}` : (TEST_KIND[r.kind ?? ''] ?? r.kind ?? '')}`;
		}
		return BLOCKER[b] ?? b;
	}
	function saveSettings(enabled: boolean) {
		if (!settings) return;
		const s = settings;
		return act(
			() =>
				api('PUT /admin/ai/settings', {
					operationId: newOperationId(),
					body: {
						lock_version: s.lock_version,
						enabled,
						active_provider_id: s.active_provider_id,
						monthly_budget_usd: form.budget.trim() || null,
						user_daily_budget_usd: form.daily,
						user_runs_per_hour: Number(form.perHour),
						conversation_retention_days: Number(form.retention)
					}
				}),
			enabled === s.enabled ? '已儲存' : enabled ? '已啟用 AI 助理' : '已關閉 AI 助理',
			enabled !== s.enabled ? '切換 AI 助理需要確認身分' : undefined
		);
	}
	const activate = (p: ProviderView) => settings && act(() => api('PUT /admin/ai/settings', { operationId: newOperationId(), body: { lock_version: settings!.lock_version, enabled: settings!.enabled, active_provider_id: p.id, monthly_budget_usd: settings!.monthly_budget_usd, user_daily_budget_usd: settings!.user_daily_budget_usd, user_runs_per_hour: settings!.user_runs_per_hour, conversation_retention_days: settings!.conversation_retention_days } }), `已把 ${p.name} 設為使用中`, '換使用中的服務需要確認身分');
	const testIt = (p: ProviderView) => act(() => api('POST /admin/ai/providers/{id}/test', { path: { id: p.id }, body: {} }), `已測試 ${p.name}`, '測試連線需要確認身分');
	const retire = (p: ProviderView) => act(() => api('POST /admin/ai/providers/{id}/retire', { path: { id: p.id }, body: {} }), `已退役 ${p.name}`, '退役服務需要確認身分');

	// ---- provider form ----
	type Form = { id: string | null; lock: number; kind: string; name: string; endpoint: string; model: string; maxOut: number; inUsd: string; outUsd: string; d17: boolean; org: boolean };
	let pf = $state<Form | null>(null);
	const blank = (): Form => ({ id: null, lock: 0, kind: 'anthropic', name: '', endpoint: endpoints?.items[0]?.name ?? '', model: 'claude-sonnet-5-5', maxOut: 4096, inUsd: '3.00', outUsd: '15.00', d17: false, org: false });
	const edit = (p: ProviderView): Form => ({ id: p.id, lock: p.lock_version, kind: p.kind, name: p.name, endpoint: p.endpoint_name, model: p.model, maxOut: p.max_output_tokens, inUsd: p.input_usd_per_mtok, outUsd: p.output_usd_per_mtok, d17: false, org: !!p.org_account_attested_at });
	const ep = $derived(endpoints?.items.find((e) => e.name === pf?.endpoint) ?? null);
	const isPublic = $derived(ep?.address_class === 'public');
	const pfOk = $derived(!!pf && pf.name.trim().length >= 1 && pf.name.trim().length <= 60 && !!pf.endpoint && (pf.kind === 'anthropic' || !!pf.model.trim()) && pf.d17 && (!isPublic || pf.org) && pf.maxOut >= 256 && pf.maxOut <= 32000);
	function setKind(k: string) {
		if (!pf) return;
		pf.kind = k;
		if (k === 'anthropic' && !pf.model) pf.model = 'claude-sonnet-5-5';
	}
	function saveProvider() {
		if (!pf) return;
		const f = pf;
		const common = { name: f.name.trim(), endpoint_name: f.endpoint, model: f.model.trim() || null, max_output_tokens: f.maxOut, input_usd_per_mtok: f.inUsd, output_usd_per_mtok: f.outUsd, d17_attested: f.d17, org_account_attested: isPublic ? f.org : undefined };
		return act(
			async () => {
				if (f.id) await api('PATCH /admin/ai/providers/{id}', { path: { id: f.id }, body: { ...common, lock_version: f.lock } });
				else await api('POST /admin/ai/providers', { operationId: newOperationId(), body: { ...common, kind: f.kind, d17_attested: true } });
				pf = null;
			},
			f.id ? '已更新服務' : '已新增服務（請測試連線）',
			'變更 AI 服務設定需要確認身分'
		);
	}
	/** §6.20 errors in words; never the provider's own text. */
	function errText(e: unknown): string {
		if (e instanceof ApiError) {
			const d = (e.body?.details ?? {}) as { kind?: string; model?: string };
			if (e.code === 'ai_model_disallowed') return `〈${pf?.model || d.model || '這個模型'}〉在 D17 禁用名單內。`;
			if (e.code === 'ai_endpoint_invalid') {
				if (d.kind === 'unregistered') return '這個端點已不在伺服器設定中。';
				if (d.kind === 'no_key') return `Anthropic 需要 key：請在伺服器設定 OXLEDGER_AI_KEY_${pf?.endpoint ?? '〈名稱〉'}。`;
				if (d.kind === 'endpoint_blocked') return '端點解析到不允許的位址（私有網段或本機需要伺服器設定 OXLEDGER_AI_ALLOW_HTTP=1；雲端 metadata 與 link-local 位址一律不允許）。';
			}
			if (e.code === 'ai_provider_error') return `測試失敗：${TEST_KIND[d.kind ?? ''] ?? d.kind ?? ''}`;
		}
		return describeError(e).title;
	}
	const TEST_KIND: Record<string, string> = { timeout: '逾時', http_status: 'HTTP 錯誤', malformed: '回應格式不對', endpoint_blocked: '端點位址不允許', not_configured: '這個版本沒有 AI 連線功能', connect: '連不上' };
	function lastTest(p: ProviderView): string {
		if (!p.last_test_at) return '還沒測試';
		const r = (p.last_test_result ?? {}) as { ok?: boolean; kind?: string; status?: number };
		return `${fmtShort(p.last_test_at)} ${r.ok ? '成功' : `失敗：${r.kind === 'http_status' ? `HTTP ${r.status}` : (TEST_KIND[r.kind ?? ''] ?? r.kind ?? '')}`}`;
	}
	const ADDR: Record<string, string> = { public: '公網', private: '私有網段', loopback: '本機', blocked: '不允許' };
	const EVENT: Record<string, string> = { created: '新增服務', updated: '修改服務', retired: '退役服務', activated: '設為使用中', settings_updated: '修改設定', test_run: '測試連線' };

	const pct = (a: string, b: string | null) => (b && Number(b) > 0 ? Math.min(100, (Number(a) / Number(b)) * 100) : 0);
	const maxDay = $derived(Math.max(0.000001, ...(usage?.days ?? []).map((d) => Number(d.spent_usd))));
</script>

<svelte:head><title>AI 設定・oxledger</title></svelte:head>
<div class="page">
	<h1>AI 設定</h1>
	<ErrorBox error={loadError} onaction={load} />
	{#if error}<div class="banner b" data-testid="ai-admin-error">{errText(error)}</div>{/if}
	{#if settings && endpoints}
		<h2>總開關</h2>
		<div class="card" data-testid="ai-switch">
			<label class="row" style="gap:8px">
				<input type="checkbox" checked={settings.enabled} disabled={busy || (!settings.enabled && settings.enable_blockers.length > 0)} onchange={(e) => { const el = e.currentTarget; Promise.resolve(saveSettings(el.checked)).then(() => { if (settings) el.checked = settings.enabled; }); }} data-testid="ai-enabled" />
				<b>啟用 AI 助理</b>
				<span class="chip {settings.enabled ? 'c-ok' : 'c-dim'}">{settings.enabled ? '已啟用' : '已關閉'}</span>
			</label>
			{#if !settings.enabled && settings.enable_blockers.length}
				<ul class="hint" data-testid="ai-blockers">{#each settings.enable_blockers as b (b)}<li>{blocker(b)}</li>{/each}</ul>
			{/if}
		</div>

		<h2>已登記的端點</h2>
		<div class="card" data-testid="ai-endpoints">
			{#if !endpoints.transport}<div class="banner w">這個伺服器版本沒有 AI 連線功能（未啟用 ai feature），測試與對話都會失敗。</div>{/if}
			{#if endpoints.items.length}
				<table class="t">
					<thead><tr><th>名稱</th><th>網址</th><th>位址類型</th><th>key</th></tr></thead>
					<tbody>
						{#each endpoints.items as e (e.name)}
							<tr><td class="mono">{e.name}</td><td class="mono">{e.origin}</td><td>{e.address_class ? (ADDR[e.address_class] ?? e.address_class) : '無法解析'}</td><td>{e.key_configured ? '已設定' : '未設定'}</td></tr>
						{/each}
					</tbody>
				</table>
			{/if}
			<p class="hint">端點與 key 由伺服器設定登記（OXLEDGER_AI_ENDPOINT_〈名稱〉、OXLEDGER_AI_KEY_〈名稱〉），在這裡無法新增或修改；改完要重新啟動服務。私有網段與本機位址用 http 需要 OXLEDGER_AI_ALLOW_HTTP=1{endpoints.allow_http ? '（目前已開啟）' : ''}。</p>
		</div>

		<h2>服務</h2>
		<div class="card" data-testid="ai-providers">
			{#each providers.filter((p) => !p.retired_at) as p (p.id)}
				<div class="prow" data-testid="ai-provider">
					<div class="row" style="gap:6px;flex-wrap:wrap">
						<b>{p.name}</b>
						{#if p.active}<span class="chip c-ok">使用中</span>{/if}
						<span class="hint">{p.kind === 'anthropic' ? 'Anthropic' : 'OpenAI 相容'}・{p.endpoint_name}{p.endpoint_registered ? '' : '（已不在伺服器設定）'}・{p.model}・工具呼叫{p.supports_tools ? '支援' : '不支援'}</span>
					</div>
					<div class="row" style="gap:6px;flex-wrap:wrap">
						<span class="hint" data-testid="ai-provider-test">最近測試：{lastTest(p)}</span>
						<span style="flex:1"></span>
						<button class="btn sm" onclick={() => testIt(p)} disabled={busy} data-testid="ai-test">測試連線</button>
						{#if !p.active}<button class="btn sm" onclick={() => activate(p)} disabled={busy} data-testid="ai-activate">設為使用中</button>{/if}
						<button class="btn sm ghost" onclick={() => (pf = edit(p))} disabled={busy}>編輯</button>
						<button class="btn sm ghost" onclick={() => confirm(`退役「${p.name}」？退役後無法復原，要再用必須重新新增。`) && retire(p)} disabled={busy} data-testid="ai-retire">退役</button>
					</div>
				</div>
			{:else}
				<div class="empty">還沒有服務。</div>
			{/each}
			{#if !pf}<button class="btn sm" onclick={() => (pf = blank())} disabled={!endpoints.items.length} data-testid="ai-provider-new">＋ 新增</button>{/if}
		</div>

		{#if pf}
			<div class="card" data-testid="ai-provider-form">
				<b>{pf.id ? '編輯服務' : '新增服務'}</b>
				{#if !endpoints.items.length}
					<p class="hint">伺服器沒有登記任何端點，無法新增。</p>
				{:else}
					<div class="fgrid">
						<label>類型
							<select value={pf.kind} onchange={(e) => setKind(e.currentTarget.value)} disabled={!!pf.id} data-testid="pf-kind">
								<option value="anthropic">Anthropic</option>
								<option value="openai_compat">OpenAI 相容</option>
							</select>
						</label>
						<label>名稱<input bind:value={pf.name} maxlength="60" data-testid="pf-name" /></label>
						<label>端點
							<select bind:value={pf.endpoint} data-testid="pf-endpoint">
								{#each endpoints.items as e (e.name)}<option value={e.name}>{e.name}（{e.origin}）</option>{/each}
							</select>
						</label>
						<label>模型<input bind:value={pf.model} data-testid="pf-model" /></label>
						<label>最大輸出 token<input type="number" min="256" max="32000" bind:value={pf.maxOut} data-testid="pf-maxout" /><span class="hint">一次回覆的累計上限</span></label>
						<label>輸入單價（USD／百萬 token）<input bind:value={pf.inUsd} data-testid="pf-in" /></label>
						<label>輸出單價（USD／百萬 token）<input bind:value={pf.outUsd} data-testid="pf-out" /><span class="hint">依你們的合約填，用來算成本上限</span></label>
					</div>
					<div class="banner w" style="margin-top:8px">依公司政策（D17），不得使用中國公司開發的模型或中國公司提供的服務（判定看開發者或公司所在地）。自架的開放權重模型也適用。</div>
					<label class="row" style="gap:6px"><input type="checkbox" bind:checked={pf.d17} data-testid="pf-d17" /> 我確認這個模型與服務符合 D17</label>
					{#if isPublic}
						<div class="banner i" style="margin-top:6px">對話內容（只含 AI 助理讀得到的議題內容）會送到〈{pf.name || '這個服務'}〉；請使用公司帳號的 key，並確認該服務的資料保存條款。</div>
						<label class="row" style="gap:6px"><input type="checkbox" bind:checked={pf.org} data-testid="pf-org" /> 我確認這個服務用的是公司（組織）帳號的 key</label>
					{/if}
					<div class="row" style="gap:6px;margin-top:8px">
						<button class="btn pri sm" onclick={saveProvider} disabled={busy || !pfOk} data-testid="pf-save">儲存</button>
						<button class="btn sm" onclick={() => (pf = null)}>取消</button>
					</div>
				{/if}
			</div>
		{/if}

		<h2>預算與保存</h2>
		<div class="card" data-testid="ai-budget">
			<div class="fgrid">
				<label>全站月預算（USD）<input bind:value={form.budget} placeholder="必填才可啟用" data-testid="ai-budget-month" /></label>
				<label>每人每日成本（USD）<input bind:value={form.daily} data-testid="ai-budget-day" /></label>
				<label>每人每小時次數<input type="number" min="1" max="600" bind:value={form.perHour} /></label>
				<label>對話保存天數<input type="number" min="1" max="365" bind:value={form.retention} /></label>
			</div>
			<button class="btn sm pri" style="margin-top:6px" onclick={() => saveSettings(settings!.enabled)} disabled={busy} data-testid="ai-budget-save">儲存</button>
			{#if settings.monthly_budget_usd}
				{@const m = settings.month}
				{@const used = pct(m.spent_usd, settings.monthly_budget_usd)}
				{@const held = pct(m.reserved_usd, settings.monthly_budget_usd)}
				<div class="bar" title="已用 {m.spent_usd}、保留中 {m.reserved_usd}、上限 {settings.monthly_budget_usd}">
					<i class:warn={used >= 80 && used < 100} class:bad={used >= 100} style="width:{used}%"></i><i class="held" style="width:{Math.min(held, 100 - used)}%"></i>
				</div>
				<div class="hint">本月已用 {m.spent_usd}・保留中 {m.reserved_usd}・上限 {settings.monthly_budget_usd} USD{used >= 100 ? '（已用完）' : used >= 80 ? '（已超過 80%）' : ''}</div>
			{/if}
		</div>

		<h2>用量</h2>
		<div class="card" data-testid="ai-usage">
			<p class="hint">管理員看不到使用者的對話內容，只有次數、token 與成本。</p>
			{#if usage?.days.length}
				<div class="days">
					{#each usage.days as d (d.day)}
						<div class="day" title="{d.day}：{d.runs} 次、{d.spent_usd} USD"><i style="height:{(Number(d.spent_usd) / maxDay) * 100}%"></i><span>{d.day.slice(8)}</span></div>
					{/each}
				</div>
			{/if}
			{#if usage?.users.length}
				<table class="t">
					<thead><tr><th>使用者</th><th>次數</th><th>輸入 token</th><th>輸出 token</th><th>成本（USD）</th></tr></thead>
					<tbody>{#each usage.users as u (u.user_id)}<tr><td>{u.name}</td><td>{u.runs}</td><td>{u.input_tokens}</td><td>{u.output_tokens}</td><td>{u.spent_usd}</td></tr>{/each}</tbody>
				</table>
			{:else}
				<div class="empty">本月還沒有用量。</div>
			{/if}
		</div>

		<h2>稽核</h2>
		<div class="card" data-testid="ai-events">
			{#each events as ev (ev.id)}
				<div class="row hint" style="gap:6px"><span class="mono">{fmtTime(ev.at)}</span><span>{ev.actor_name}</span><span>{EVENT[ev.kind] ?? ev.kind}</span>{#if ev.provider_id}<span>{providers.find((p) => p.id === ev.provider_id)?.name ?? ''}</span>{/if}</div>
			{:else}
				<div class="empty">沒有紀錄。</div>
			{/each}
		</div>
	{:else if !loadError}
		<div class="sync"><span class="spin"></span>載入中</div>
	{/if}
</div>

<style>
	.prow { border-bottom: 1px solid var(--line); padding: 6px 0; }
	.fgrid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 8px; margin-top: 6px; }
	.fgrid label { display: flex; flex-direction: column; gap: 3px; font-size: 13px; }
	.bar { display: flex; height: 10px; background: var(--card2); border-radius: 5px; overflow: hidden; margin-top: 8px; }
	.bar i { background: var(--ok, #2a7); }
	.bar i.warn { background: var(--warn, #c77700); }
	.bar i.bad { background: var(--bad, #c33); }
	.bar i.held { background: var(--dim); opacity: 0.5; }
	.days { display: flex; align-items: flex-end; gap: 3px; height: 80px; margin-bottom: 8px; }
	.day { flex: 1; display: flex; flex-direction: column; justify-content: flex-end; align-items: center; height: 100%; font-size: 10px; color: var(--dim); }
	.day i { width: 100%; background: var(--act); border-radius: 2px 2px 0 0; min-height: 1px; }
</style>

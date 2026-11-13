<script lang="ts">
	// 一個 AI 角色 (contract 05 §6.18): the URL carries the handle, the API the
	// principal id (`GET /ai-team?handle=` resolves it). What shows depends on
	// the viewer (§3.12.5): everyone who sees the roster gets the safety
	// summary; the sponsor and site admins the duties, every policy and the
	// runtime; a project manager the policy of the projects they manage
	// (stricter only).
	import { page } from '$app/state';
	import type { ActivityItem, CredentialSummary, IssuedCredential, PolicyMode, ProjectPolicy, SafetySummary, TeamDetail, TemplateSpec } from '$api/types';
	import { api, ApiError } from '$lib/api';
	import { commandLabel, isStricter, STATUS_LABEL, TEMPLATE_LABEL, type Policies } from '$lib/aiteam';
	import { refreshCounts } from '$lib/counts.svelte';
	import { defaultModeFor, newOperations, withNewOperations } from '$lib/review';
	import { fieldLabel, fmtShort, fmtTime, MODE_LABELS } from '$lib/format';
	import { labelOf, show, type Lookups } from '$lib/render';
	import { session } from '$lib/session.svelte';
	import { stepped } from '$lib/stepup.svelte';
	import { toast } from '$lib/toast.svelte';
	import { customFieldNames, projectLookups } from '$lib/trackers';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import PolicyTable from '$lib/components/PolicyTable.svelte';
	import TokenOnce from '$lib/components/TokenOnce.svelte';

	const handle = $derived(decodeURIComponent(page.params.handle ?? ''));
	let d = $state<TeamDetail | null>(null);
	let notFound = $state(false);
	let loadError = $state<unknown>(null);
	let actionError = $state<unknown>(null);
	let busy = $state(false);
	let creds = $state<CredentialSummary[] | null>(null);
	let issued = $state<IssuedCredential | null>(null);
	let activity = $state<ActivityItem[]>([]);
	let look = $state<Lookups>({ fields: [], assignees: [], versions: [] });
	let cfNames = $state<Record<string, Map<string, string>>>({});

	async function load() {
		loadError = null;
		notFound = false;
		// Moving to another role while this loads: the older answers are dropped.
		const h = handle;
		const stale = () => h !== handle;
		try {
			const l = await api('GET /ai-team', { query: { handle: h } });
			if (stale()) return;
			const m = l.items.find((x) => x.handle === h);
			if (!m) {
				notFound = true;
				d = null;
				return;
			}
			const detail = await api('GET /ai-team/{id}', { path: { id: m.id } });
			if (stale()) return;
			d = detail;
			const names: Record<string, Map<string, string>> = {};
			await Promise.all(d.policies.map(async (p) => (names[p.project_id] = await customFieldNames(p.project_id).catch(() => new Map()))));
			if (stale()) return;
			cfNames = names;
			await loadNewOps(detail, stale);
			if (stale()) return;
			creds = d.actions.includes('credentials') ? (await api('GET /ai-team/{id}/credentials', { path: { id: m.id } }).catch(() => ({ items: null }))).items : null;
			const a = await api('GET /ai-team/{id}/activity', { path: { id: m.id }, query: { limit: 20 } }).catch(() => null);
			if (stale()) return;
			activity = a?.items ?? [];
			const prefixes = new Set(activity.flatMap((i) => i.keys.map((k) => k.split('-')[0])));
			look = await projectLookups(session.projects.filter((p) => prefixes.has(p.key)).map((p) => p.id)).catch(() => look);
		} catch (e) {
			if (e instanceof ApiError && e.code === 'not_found') notFound = true;
			else loadError = e;
		}
	}
	$effect(() => {
		void handle;
		issued = null;
		load();
	});

	const me = $derived(session.user?.user_id);
	const isSponsor = $derived(!!d && d.sponsor.id === me);
	/** The runtime block is for the sponsor and site administrators only (§3.12.5). */
	const fullView = $derived(!!d?.runtime);
	const can = (a: string) => !!d?.actions.includes(a);
	const fname = (pid: string, f: string) => cfNames[pid]?.get(f) ?? fieldLabel(f);

	/** Runs an action; with `why` it needs a fresh step-up first (the layout's password dialog). */
	async function act(fn: () => Promise<unknown>, done: string, opts: { reload?: boolean; why?: string } = {}) {
		busy = true;
		actionError = null;
		try {
			const ok = opts.why ? await stepped(async () => (await fn(), true), opts.why) : (await fn(), true);
			if (!ok) return;
			toast(done, 'ok');
			if (opts.reload !== false) await load();
			refreshCounts();
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}

	// ---- pause / resume / retire ----
	let pauseFor = $state<{ scope: 'principal' | 'project'; project?: ProjectPolicy } | null>(null);
	let pauseReason = $state('');
	function askPause(scope: 'principal' | 'project', project?: ProjectPolicy) {
		pauseFor = { scope, project };
		pauseReason = '';
	}
	const pauseOk = $derived(!!pauseFor && (pauseFor.scope === 'principal' ? pauseReason.trim().length <= 500 : pauseReason.trim().length >= 1 && pauseReason.trim().length <= 500));
	function doPause() {
		if (!d || !pauseFor) return;
		const id = d.id;
		const body = { scope: pauseFor.scope, project_id: pauseFor.project?.project_id ?? null, reason: pauseReason.trim() || null };
		const where = pauseFor.project ? `在 ${pauseFor.project.project} ` : '';
		pauseFor = null;
		return act(() => api('POST /ai-team/{id}/pause', { path: { id }, body }), `已${where}暫停 ${d.handle} 的寫入（讀取照常）`);
	}
	const resume = (project?: ProjectPolicy) =>
		d && act(() => api('POST /ai-team/{id}/resume', { path: { id: d!.id }, body: { scope: project ? 'project' : 'principal', project_id: project?.project_id ?? null } }), project ? `已恢復 ${d.handle} 在 ${project.project} 的寫入` : `已恢復 ${d.handle} 的寫入`);
	let confirmRetire = $state(false);
	const retire = () => d && act(() => api('POST /ai-team/{id}/retire', { path: { id: d!.id }, body: {} }), `已退役 ${d.handle}（委派與 token 都已撤銷）`).then(() => (confirmRetire = false));

	// ---- tokens (G14) ----
	const newToken = () =>
		act(
			async () => {
				issued = await api('POST /ai-team/{id}/credentials', { path: { id: d!.id }, body: { label: 'spa' } });
				creds = (await api('GET /ai-team/{id}/credentials', { path: { id: d!.id } })).items;
			},
			'已發新 token（舊 token 仍可用，記得撤銷）',
			{ reload: false, why: '發出新的 agent token 需要確認身分' }
		);
	const revokeToken = (c: CredentialSummary) =>
		act(
			async () => {
				await api('POST /agent-credentials/{id}/revoke', { path: { id: c.id }, body: {} });
				creds = (await api('GET /ai-team/{id}/credentials', { path: { id: d!.id } })).items;
			},
			'已撤銷 token（不影響現有提案）',
			{ reload: false }
		);
	const CRED_STATE: Record<string, { t: string; c: string }> = {
		active: { t: '有效', c: 'c-ok' },
		revoked: { t: '已撤銷', c: 'c-dim' },
		expired: { t: '已到期', c: 'c-dim' },
		context_closed: { t: '工作脈絡已結束', c: 'c-dim' }
	};

	// ---- policies ----
	/** §6.18 ②: the allowed commands split into direct / review, from the policy table when the viewer has it. */
	function safetySplit(x: SafetySummary): { direct: string[]; review: string[] } | null {
		const p = d?.policies.find((q) => q.project_id === x.project_id);
		if (!p) return null;
		const mode = (c: string) => p.commands[c] ?? 'off';
		return { direct: x.allowed_commands.filter((c) => mode(c) === 'direct'), review: x.allowed_commands.filter((c) => mode(c) === 'review') };
	}

	// A failed change leaves the radio where it was clicked: draw the table again from the saved policies.
	let policyRev = $state(0);
	async function pick(p: ProjectPolicy, kind: 'commands' | 'fields', key: string, mode: PolicyMode) {
		const next: Policies = { commands: { ...p.commands }, fields: { ...p.fields } };
		next[kind][key] = mode;
		let ok = false;
		await act(async () => {
			await api('PUT /delegations/{id}/policies', { path: { id: p.delegation_id }, body: next });
			ok = true;
		}, `已更新 ${p.project} 的政策`, { why: '修改委派政策需要確認身分' });
		if (!ok) policyRev++;
	}
	/** Sponsors may loosen too; a project manager only tightens (§3.12.5). */
	const canPick = (from: PolicyMode, to: PolicyMode) => isSponsor || isStricter(from, to);
	// ---- new operations an existing delegation could add (contract 05 §4.3, §6.14; 02 §17.9a) ----
	/** delegation id → what it lacks; only the delegator may read `GET /delegations/{id}/policies`. */
	let newOps = $state<Record<string, { commands: string[]; fields: string[] }>>({});
	/** delegation id → `c:<op>` / `f:<field>` → ticked (default all ticked). */
	let newOpsSkip = $state<Record<string, Record<string, boolean>>>({});
	async function loadNewOps(detail: TeamDetail, stale: () => boolean) {
		if (detail.sponsor.id !== session.user?.user_id || detail.status === 'retired') {
			newOps = {};
			return;
		}
		const out: typeof newOps = {};
		await Promise.all(
			detail.policies.map(async (p) => {
				const v = await api('GET /delegations/{id}/policies', { path: { id: p.delegation_id } }).catch(() => null);
				if (v) out[p.delegation_id] = newOperations(v);
			})
		);
		if (stale()) return;
		newOps = out;
		newOpsSkip = {};
	}
	const newOpCount = (n: { commands: string[]; fields: string[] }) => n.commands.length + n.fields.length;
	function toggleNewOp(id: string, k: string) {
		newOpsSkip[id] = { ...(newOpsSkip[id] ?? {}), [k]: !newOpsSkip[id]?.[k] };
	}
	function addNewOps(p: ProjectPolicy) {
		const n = newOps[p.delegation_id];
		if (!n) return;
		const skip = newOpsSkip[p.delegation_id] ?? {};
		const add = { commands: n.commands.filter((c) => !skip[`c:${c}`]), fields: n.fields.filter((f) => !skip[`f:${f}`]) };
		const count = newOpCount(add);
		if (!count) return;
		const body = withNewOperations(p, add);
		return act(() => api('PUT /delegations/{id}/policies', { path: { id: p.delegation_id }, body }), `已把 ${count} 個新操作加入 ${p.project} 的政策（可在表中調整）`, {
			why: '放寬委派政策需要確認身分'
		});
	}

	const revokeDelegation = (p: ProjectPolicy) => act(() => api('POST /delegations/{id}/revoke', { path: { id: p.delegation_id }, body: {} }), `已撤銷 ${p.project} 的委派`);

	// ---- add a project (sponsor) ----
	let addProject = $state('');
	let tpl = $state<TemplateSpec | null>(null);
	const delegated = $derived(new Set(d?.policies.map((p) => p.project_id) ?? []));
	const addable = $derived(session.projects.filter((p) => !delegated.has(p.id)));
	async function doAdd() {
		if (!d || !addProject) return;
		const id = d.id;
		const pid = addProject;
		const template = d.role.template;
		return act(
			async () => {
				if (!tpl || tpl.template !== template) tpl = (await api('GET /ai-team/templates')).find((t) => t.template === template) ?? null;
				const t = tpl;
				if (!t) throw new Error('找不到這個角色的範本');
				const ops = t.proposal_ops.filter((o) => (t.policies.commands[o] ?? 'off') !== 'off');
				const fields = t.writable_fields.filter((f) => (t.policies.fields[f] ?? 'off') !== 'off');
				await api('POST /delegations', { body: { agent_principal_id: id, project_id: pid, proposal_ops: ops, writable_fields: fields, expires_in_days: 7, policies: t.policies } });
				addProject = '';
			},
			'已加入專案（套用範本的政策，可在下方調整）',
			{ why: '委派新專案需要確認身分' }
		);
	}

	// ---- runtime (sponsor) ----
	let renaming = $state(false);
	let newLabel = $state('');
	const rename = () =>
		d?.runtime &&
		act(async () => {
			await api('PATCH /me/agent-runtimes/{id}', { path: { id: d!.runtime!.id }, body: { label: newLabel.trim(), lock_version: d!.runtime!.lock_version } });
			renaming = false;
		}, '已改名稱');
	let askFresh = $state(false);
	let freshAck = $state(false);
	const freshContext = () =>
		d?.runtime &&
		act(
			async () => {
				await api('POST /me/agent-runtimes/{id}/new-context', { path: { id: d!.runtime!.id }, body: { fresh_context_ack: true } });
				askFresh = false;
				freshAck = false;
			},
			'已建立新工作脈絡；同一個執行環境的角色都要重新加入專案並發新 token。',
			{ why: '建新工作脈絡需要確認身分' }
		);
	let askRetireRt = $state(false);
	const retireRuntime = () =>
		d?.runtime &&
		act(
			async () => {
				await api('POST /me/agent-runtimes/{id}/retire', { path: { id: d!.runtime!.id }, body: {} });
				askRetireRt = false;
			},
			'已退役執行環境；這個 agent 軟體之後可以放進新的執行環境。',
			{ why: '退役執行環境需要確認身分' }
		);
	const rtRoles = $derived(d?.runtime ? [d.handle, ...d.runtime.shared_with] : []);

	const KIND: Record<string, { t: string; c: string }> = {
		direct: { t: '直接寫入', c: 'c-act' },
		proposal: { t: '送審後套用', c: 'c-prop' },
		blocked: { t: '被擋下', c: 'c-bad' },
		undo: { t: '已回復', c: 'c-dim' }
	};
</script>

<svelte:head><title>{handle}・AI 團隊・oxledger</title></svelte:head>
<div class="page">
	<a href="/ai-team" class="hint">← AI 團隊</a>
	<ErrorBox error={loadError} onaction={load} />
	{#if notFound}
		<div class="empty" data-testid="role-missing">找不到這個 AI 角色，或你沒有權限查看。</div>
	{:else if d}
		{@const s = STATUS_LABEL[d.status] ?? { t: d.status, c: 'c-dim' }}
		<div class="row" style="gap:8px;margin:6px 0;flex-wrap:wrap">
			<h1 style="margin:0" class="agname" data-testid="role-handle">{d.handle}</h1>
			<span class="agent-label">agent</span>
			<span class="chip c-dim">{TEMPLATE_LABEL[d.role.template] ?? d.role.template}</span>
			<span class="chip {s.c}" title={s.tip} data-testid="role-status">{s.t}</span>
			<span class="hint">sponsor：{d.sponsor.name}</span>
		</div>
		{#if d.duties != null}
			<div class="card"><span class="k">職責</span> {d.duties || '（沒有寫）'} <span class="hint">（只有 sponsor 和站台管理員看得到）</span></div>
		{/if}
		{#if d.template_update?.outdated}
			<div class="banner i">範本有新版（第 {d.template_update.latest_version} 版；這個角色是第 {d.template_update.current_version} 版）。範本改版不會改既有政策；要套用請在下方調整。</div>
		{/if}
		{#if d.pause}
			<div class="banner w" data-testid="role-paused">寫入已暫停・{fmtShort(d.pause.paused_at)}{d.pause.reason ? `：${d.pause.reason}` : ''}{d.pause.paused_by === 'site_admin' ? '（站台管理員暫停，只能由站台管理員恢復）' : ''}。讀取照常。</div>
		{/if}
		{#if d.status === 'quarantined'}
			<div class="banner b" data-testid="role-quarantined">這個角色所在的執行環境有專案權限改變，同一個執行環境的角色都已停用。{isSponsor ? '請在下方「執行環境」建新工作脈絡。' : '要由 sponsor 建新工作脈絡。'}</div>
		{/if}

		<ErrorBox error={actionError} onaction={load} />
		{#if issued}<TokenOnce token={issued} onclose={() => (issued = null)} />{/if}

		<div class="row" style="gap:8px;margin:8px 0;flex-wrap:wrap">
			{#if can('pause') && !d.pause && d.status !== 'retired' && fullView}<button class="btn sm" onclick={() => askPause('principal')} disabled={busy} data-testid="pause">暫停</button>{/if}
			{#if can('resume') && d.pause}<button class="btn sm" onclick={() => resume()} disabled={busy} data-testid="resume">恢復</button>{/if}
			{#if can('credentials') && d.status !== 'retired'}<button class="btn sm" onclick={newToken} disabled={busy} data-testid="new-token" title="不影響現有提案">新 token</button>{/if}
			<a class="btn sm" href="/agents/{d.id}/activity" data-testid="role-activity">活動與回復</a>
			{#if can('retire') && d.status !== 'retired'}<button class="btn sm" onclick={() => (confirmRetire = true)} disabled={busy} data-testid="retire">退役…</button>{/if}
		</div>
		{#if pauseFor}
			<div class="card" data-testid="pause-dialog">
				<b>{pauseFor.scope === 'principal' ? `暫停 ${d.handle}？它在所有專案的寫入會被拒絕，直到恢復。` : `在 ${pauseFor.project?.project} 暫停 ${d.handle}？只影響 ${pauseFor.project?.project}。`}</b>
				<label class="row" style="gap:6px;margin-top:6px">原因{pauseFor.scope === 'project' ? '（必填）' : '（選填）'}<input bind:value={pauseReason} maxlength="500" style="flex:1" data-testid="pause-reason" /></label>
				<div class="row" style="gap:6px;margin-top:6px">
					<button class="btn pri sm" onclick={doPause} disabled={busy || !pauseOk} data-testid="pause-confirm">暫停</button>
					<button class="btn sm" onclick={() => (pauseFor = null)}>取消</button>
				</div>
			</div>
		{/if}
		{#if confirmRetire}
			<div class="card" data-testid="retire-dialog">
				<b>退役 {d.handle}？</b>
				<p class="hint">它的委派與 token 都會撤銷，不能再寫入；已做的變更仍可在活動頁回復。不可復原。</p>
				<div class="row" style="gap:6px">
					<button class="btn pri sm" onclick={retire} disabled={busy} data-testid="retire-confirm">退役</button>
					<button class="btn sm" onclick={() => (confirmRetire = false)}>取消</button>
				</div>
			</div>
		{/if}

		<h2>安全摘要</h2>
		<div class="card" data-testid="safety">
			<div class="hint">範本：{TEMPLATE_LABEL[d.role.template] ?? d.role.template}。只列你看得到的專案。</div>
			{#if d.safety_summary.length}
				<table class="t">
					<thead><tr><th>專案</th><th>直接寫入</th><th>需要核准</th></tr></thead>
					<tbody>
						{#each d.safety_summary as x (x.project_id)}
							{@const split = safetySplit(x)}
							<tr data-testid="safety-row">
								<td class="mono">{x.project}</td>
								{#if split}
									<td data-testid="safety-direct">{split.direct.map(commandLabel).join('、') || '—'}</td>
									<td data-testid="safety-review">{split.review.map(commandLabel).join('、') || '—'}</td>
								{:else}
									<!-- The summary has no modes for viewers without the full settings (backend gap G39). -->
									<td colspan="2">{x.allowed_commands.map(commandLabel).join('、') || '—'}<span class="hint">（直接寫入或需要核准：伺服器沒有提供）</span></td>
								{/if}
							</tr>
						{/each}
					</tbody>
				</table>
			{:else}
				<div class="empty">沒有你看得到的專案。</div>
			{/if}
		</div>

		{#if d.policies.length || (isSponsor && d.status !== 'retired')}
			<h2>完整設定</h2>
			{#each d.policies as p (p.delegation_id)}
				<div class="card" data-testid="policy" data-project={p.project}>
					<div class="row" style="gap:8px;flex-wrap:wrap">
						<b>{p.project}</b>
						<span class="hint">到期 {fmtShort(p.expires_at)}</span>
						{#if p.read_own_time}<span class="chip c-warn" title="這個 agent 可讀你的工時（含說明）。說明可能被抄到專案成員看得到的地方。">可讀我的工時</span>{/if}
						{#if p.paused_at}<span class="chip c-warn" data-testid="project-paused">本專案已暫停{p.paused_reason ? `：${p.paused_reason}` : ''}</span>{/if}
						<span style="flex:1"></span>
						{#if d.status !== 'retired'}
							{#if p.paused_at}
								{#if can('resume')}<button class="btn sm" onclick={() => resume(p)} disabled={busy} data-testid="project-resume">恢復本專案</button>{/if}
							{:else if can('pause')}
								<button class="btn sm" onclick={() => askPause('project', p)} disabled={busy} data-testid="project-pause">在 {p.project} 暫停</button>
							{/if}
							{#if isSponsor}<button class="btn sm ghost" onclick={() => revokeDelegation(p)} disabled={busy} data-testid="revoke-delegation" title="撤銷委派不會解除共用來源的寫入限制">撤銷委派</button>{/if}
						{/if}
					</div>
					{#if isSponsor && newOps[p.delegation_id] && newOpCount(newOps[p.delegation_id]) && d.status !== 'retired'}
						{@const nw = newOps[p.delegation_id]}
						{@const skip = newOpsSkip[p.delegation_id] ?? {}}
						<div class="banner i" data-testid="new-ops" data-count={newOpCount(nw)}>
							<div>有 {newOpCount(nw)} 個新操作可以加入。現有委派不會自動加入；勾選要加入的，之後可以在下表調整。</div>
							<div class="row" style="gap:10px;flex-wrap:wrap;margin-top:4px">
								{#each nw.commands as op (op)}
									<label class="row" style="gap:4px" data-testid="new-op" data-op={op}><input type="checkbox" checked={!skip[`c:${op}`]} onchange={() => toggleNewOp(p.delegation_id, `c:${op}`)} disabled={busy} />{commandLabel(op)}<span class="hint">（預設{MODE_LABELS[defaultModeFor(op)]}）</span></label>
								{/each}
								{#each nw.fields as f (f)}
									<label class="row" style="gap:4px" data-testid="new-op" data-field={f}><input type="checkbox" checked={!skip[`f:${f}`]} onchange={() => toggleNewOp(p.delegation_id, `f:${f}`)} disabled={busy} />欄位〈{fname(p.project_id, f)}〉<span class="hint">（預設{MODE_LABELS.direct}）</span></label>
								{/each}
								<button class="btn sm pri" onclick={() => addNewOps(p)} disabled={busy || (Object.values(skip).filter(Boolean).length >= newOpCount(nw))} data-testid="new-ops-add">加入政策表</button>
							</div>
						</div>
					{/if}
					{#key policyRev}<PolicyTable policies={p} fieldName={(f) => fname(p.project_id, f)} onpick={(k, key, m) => pick(p, k, key, m)} {canPick} disabled={busy || d.status === 'retired'} name={p.delegation_id} />{/key}
					{#if !isSponsor}<div class="hint">你是這個專案的管理者：只能把政策改嚴（直接寫入 → 需要核准 → 不允許）。</div>{/if}
				</div>
			{/each}
			{#if isSponsor && d.status !== 'retired' && d.status !== 'quarantined'}
				<div class="row" style="gap:6px">
					<select bind:value={addProject} aria-label="加入專案" data-testid="add-project">
						<option value="">＋ 加入專案…</option>
						{#each addable as p (p.id)}<option value={p.id}>{p.key}・{p.name}</option>{/each}
					</select>
					<button class="btn sm" onclick={doAdd} disabled={busy || !addProject} data-testid="add-project-go">加入（套用範本政策，7 天）</button>
				</div>
			{/if}
		{/if}

		{#if d.runtime}
			{@const rt = d.runtime}
			<h2>執行環境</h2>
			<div class="card" data-testid="runtime">
				<div>
					執行環境：<b>{rt.label}</b>（{rt.kind === 'builtin' ? '內建' : '外部'}）。
					{#if rt.shared_with.length}和 {rt.shared_with.join('、')} 共用來源限制；{/if}來源專案：{rt.sources.join('、') || '（還沒有）'}。
				</div>
				{#if rt.time_readers.length}
					<div data-testid="runtime-time">可讀你的工時：是（來自 {rt.time_readers.join('、')} 的委派）。同一個執行環境的其他角色也可能從共用記憶取得。</div>
				{/if}
				{#if isSponsor}
					<div class="row" style="gap:6px;margin-top:6px;flex-wrap:wrap">
						{#if renaming}
							<input bind:value={newLabel} maxlength="60" aria-label="執行環境名稱" data-testid="runtime-label" />
							<button class="btn sm pri" onclick={rename} disabled={busy || !newLabel.trim()} data-testid="runtime-rename-save">儲存</button>
							<button class="btn sm" onclick={() => (renaming = false)}>取消</button>
						{:else}
							<button class="btn sm" onclick={() => ((renaming = true), (newLabel = rt.label))} data-testid="runtime-rename">改名稱</button>
						{/if}
						{#if rt.kind !== 'builtin'}
							<button class="btn sm" onclick={() => ((askFresh = true), (freshAck = false))} disabled={busy} data-testid="runtime-fresh">建新工作脈絡…</button>
							<button class="btn sm" onclick={() => (askRetireRt = true)} disabled={busy} data-testid="runtime-retire">退役執行環境…</button>
						{/if}
					</div>
					{#if askFresh}
						<div class="card" data-testid="fresh-dialog">
							<b>建新工作脈絡會停掉這個執行環境的所有角色：{rtRoles.join('、')}</b>
							<p class="hint">舊脈絡的委派與 token 都會撤銷；之後要重新加入專案、發新 token。新脈絡從沒有任何來源開始。</p>
							<label class="row" style="gap:6px;align-items:flex-start"><input type="checkbox" bind:checked={freshAck} data-testid="fresh-ack" /> 我確認這個執行環境不會帶著舊工作脈絡讀到的內容繼續工作（例如已清掉對話、記憶與暫存檔）。oxledger 無法驗證這件事。</label>
							<div class="row" style="gap:6px;margin-top:6px">
								<button class="btn pri sm" onclick={freshContext} disabled={busy || !freshAck} data-testid="fresh-confirm">建新工作脈絡</button>
								<button class="btn sm" onclick={() => (askFresh = false)}>取消</button>
							</div>
						</div>
					{/if}
					{#if askRetireRt}
						<div class="card" data-testid="retire-runtime-dialog">
							<b>退役「{rt.label}」？</b>
							<p class="hint">會撤銷 {rtRoles.length} 個角色的委派與 token，角色一起退役；之後這個 agent 軟體可以放進新的執行環境。不可復原。</p>
							<div class="row" style="gap:6px">
								<button class="btn pri sm" onclick={retireRuntime} disabled={busy} data-testid="retire-runtime-confirm">退役執行環境</button>
								<button class="btn sm" onclick={() => (askRetireRt = false)}>取消</button>
							</div>
						</div>
					{/if}
				{/if}
			</div>
		{/if}

		{#if creds}
			<h2>token</h2>
			<div class="card" data-testid="creds">
				{#each creds as c (c.id)}
					{@const st = CRED_STATE[c.state] ?? { t: c.state, c: 'c-dim' }}
					<div class="row" style="gap:8px;padding:2px 0" data-testid="cred">
						<span>{c.label}</span>
						<span class="chip {st.c}">{st.t}</span>
						<span class="hint">建立 {fmtShort(c.created_at)}・到期 {fmtShort(c.expires_at)}・最後使用 {c.last_used_at ? fmtTime(c.last_used_at) : '沒有紀錄'}</span>
						<span style="flex:1"></span>
						{#if c.state === 'active'}<button class="btn sm" onclick={() => revokeToken(c)} disabled={busy} data-testid="cred-revoke">撤銷</button>{/if}
					</div>
				{:else}
					<div class="empty">沒有 token。</div>
				{/each}
			</div>
		{/if}

		<h2>活動</h2>
		<div class="card" data-testid="role-activity-list">
			{#each activity as it, i (it.execution_id ?? `${it.at}-${i}`)}
				<div class="row" style="gap:6px;padding:2px 0;flex-wrap:wrap" data-testid="activity">
					<span class="dim mono" style="font-size:12px">{fmtShort(it.at)}</span>
					<span class="chip {KIND[it.kind]?.c ?? 'c-dim'}">{KIND[it.kind]?.t ?? it.kind}</span>
					{#each it.keys as k (k)}<a class="key" href="/issue/{k}">{k}</a>{/each}
					{#each it.diff.slice(0, 2) as x, j (j)}<span class="hint">{labelOf(x.field, look)}：{show(x.field, x.before, look)} → {show(x.field, x.after, look)}</span>{/each}
				</div>
			{:else}
				<div class="empty">沒有你看得到的活動。</div>
			{/each}
			<a class="hint" href="/agents/{d.id}/activity">全部活動、回復與事故回復 →</a>
		</div>
	{:else if !loadError}
		<div class="sync"><span class="spin"></span>載入中</div>
	{/if}
</div>

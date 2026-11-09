<script lang="ts">
	// Home (layout B, U22): widgets each person can add, remove, order and
	// widen; "需要你決定" is always there (only movable).
	import type { AgentSummary, SearchItem, VersionView } from '$api/types';
	import { api } from '$lib/api';
	import { counts, refreshCounts } from '$lib/counts.svelte';
	import { prefs, savePrefs, WIDGET_TITLES, type Widget, type WidgetKind } from '$lib/prefs.svelte';
	import { session, projectById } from '$lib/session.svelte';
	import { parse, toParams } from '$lib/query';
	import { HEALTH, HEALTH_REASON } from '$lib/health';
	import IssueList from '$lib/components/IssueList.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	let editing = $state(false);
	let summary = $state<AgentSummary | null>(null);
	let mine = $state<SearchItem[] | null>(null);
	let recent = $state<SearchItem[] | null>(null);
	let milestones = $state<{ project: string; v: VersionView }[] | null>(null);
	let queries = $state<Record<string, { items?: SearchItem[]; error?: unknown }>>({});
	let errors = $state<Record<string, unknown>>({});
	let newQuery = $state({ title: '', query: '' });

	const kinds = $derived(new Set(prefs.widgets.map((w) => w.kind)));

	async function load(kind: WidgetKind, w?: Widget) {
		try {
			if (kind === 'agents') summary = await api('GET /home/agent-summary');
			if (kind === 'mine') mine = (await api('POST /issues/search', { body: { assignee: 'me', status_category: 'open', limit: 10 } })).items;
			if (kind === 'recent') recent = (await api('POST /issues/search', { body: { limit: 10 } })).items;
			if (kind === 'milestones') {
				const out: { project: string; v: VersionView }[] = [];
				for (const p of session.projects.slice(0, 8)) {
					const l = await api('GET /projects/{id}/versions', { path: { id: p.key }, query: { include: 'stats' } });
					for (const v of l.versions) if (v.status !== 'closed') out.push({ project: p.key, v });
				}
				milestones = out;
			}
			if (kind === 'query' && w?.query) {
				try {
					const r = await api('POST /issues/search', { body: { ...toParams(parse(w.query)), limit: 10 } });
					queries[w.id] = { items: r.items };
				} catch (e) {
					queries[w.id] = { error: e };
				}
			}
		} catch (e) {
			errors[kind] = e;
		}
	}

	$effect(() => {
		for (const w of prefs.widgets) if (w.kind !== 'decide') load(w.kind, w);
	});

	function move(i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= prefs.widgets.length) return;
		const ws = [...prefs.widgets];
		[ws[i], ws[j]] = [ws[j], ws[i]];
		prefs.widgets = ws;
		savePrefs();
	}
	function remove(i: number) {
		if (prefs.widgets[i].kind === 'decide') return;
		prefs.widgets = prefs.widgets.filter((_, k) => k !== i);
		savePrefs();
	}
	function widen(i: number) {
		prefs.widgets[i].wide = !prefs.widgets[i].wide;
		savePrefs();
	}
	function add(kind: WidgetKind) {
		const w: Widget = { id: kind === 'query' ? `q${Date.now()}` : kind, kind, wide: false };
		if (kind === 'query') {
			w.title = newQuery.title.trim() || newQuery.query.trim();
			w.query = newQuery.query.trim();
			try {
				parse(w.query);
			} catch {
				errors.query = new Error('查詢語法有誤');
				return;
			}
			newQuery = { title: '', query: '' };
		}
		prefs.widgets = [...prefs.widgets, w];
		savePrefs();
	}

	const failed = $derived(counts.awaiting.filter((c) => c.last_execution_status === 'failed'));
	const paused = $derived(counts.contexts.filter((c) => c.state === 'active' && c.suspended_at));
	const broken = $derived(counts.contexts.filter((c) => c.state === 'active' && c.changed_projects.length > 0));
	const projName = (id: string) => projectById(id)?.key ?? '某個專案';
	const SUSPEND: Record<string, string> = {
		manual: '你手動暫停',
		audience_blocked: '1 小時內多次被受眾規則擋下，已自動暫停',
		issue_write_burst: '同一張單短時間內寫入太多次，已自動暫停',
		version_write_burst: '里程碑短時間內寫入太多次，已自動暫停'
	};
</script>

<svelte:head><title>首頁・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="margin-bottom:12px">
		<h1>{session.user?.name}，你好</h1>
		<span class="sp" style="flex:1"></span>
		<button class="btn sm" class:on={editing} onclick={() => (editing = !editing)} data-testid="edit-home">{editing ? '完成' : '自訂首頁'}</button>
	</div>
	{#if editing}
		<div class="card" data-testid="home-editor">
			<div class="k">新增小工具</div>
			<div class="row" style="margin:6px 0">
				{#each Object.entries(WIDGET_TITLES) as [k, t] (k)}
					{#if k !== 'decide' && k !== 'query' && !kinds.has(k as WidgetKind)}
						<button class="btn sm" onclick={() => add(k as WidgetKind)}>＋ {t}</button>
					{/if}
				{/each}
			</div>
			<div class="inline-form">
				<input type="text" placeholder="名稱（例如：AUTH 處理中）" bind:value={newQuery.title} style="max-width:200px" />
				<input type="text" placeholder="project:AUTH status:open 登入" bind:value={newQuery.query} style="max-width:320px" />
				<button class="btn sm" disabled={!newQuery.query.trim()} onclick={() => add('query')}>＋ 存起來的篩選</button>
			</div>
			<ErrorBox error={errors.query} />
			<p class="hint">「需要你決定」不能移除，只能移動。設定存在這個瀏覽器（伺服器還沒有個人設定的端點）。</p>
		</div>
	{/if}
	<div class="wgrid">
		{#each prefs.widgets as w, i (w.id)}
			<section class="widget" class:wide={w.wide} class:editing data-testid="widget-{w.kind}">
				<div class="wh">
					<b>{w.title ?? WIDGET_TITLES[w.kind]}</b>
					<span style="flex:1"></span>
					{#if editing}
						<button class="btn sm" title="往前" onclick={() => move(i, -1)} disabled={i === 0}>↑</button>
						<button class="btn sm" title="往後" onclick={() => move(i, 1)} disabled={i === prefs.widgets.length - 1}>↓</button>
						<button class="btn sm" onclick={() => widen(i)}>{w.wide ? '變窄' : '變寬'}</button>
						{#if w.kind !== 'decide'}<button class="btn sm bad" onclick={() => remove(i)} data-testid="remove-{w.kind}">移除</button>{/if}
					{/if}
				</div>
				{#if w.kind === 'decide'}
					{@const n = counts.awaiting.length}
					{#each paused as c (c.id)}
						<div class="banner w"><span class="agname">{c.agent_label}</span> 寫入已暫停：{SUSPEND[c.suspension_reason ?? ''] ?? c.suspension_reason}<span class="sp"></span><a class="btn sm" href="/agents">處理</a></div>
					{/each}
					{#each broken as c (c.id)}
						<div class="banner b"><span class="agname">{c.agent_label}</span> 的工作脈絡因 {c.changed_projects.map(projName).join('、')} 的權限設定變更而停用<span class="sp"></span><a class="btn sm" href="/agents">建立新工作脈絡</a></div>
					{/each}
					{#each failed as c (c.changeset_id)}
						<div class="banner b">提案「{c.title ?? c.targets.map((t) => t.key).join('、')}」已核准但套用失敗<span class="sp"></span><a class="btn sm" href="/cs/{c.changeset_id}">查看</a></div>
					{/each}
					{#if n}
						<div class="row"><span class="badge">{counts.awaitingMore ? `${n}+` : n}</span> 份提案等你審<span style="flex:1"></span><a class="btn sm pri" href="/review">開啟待審</a></div>
						<ul style="margin:6px 0 0;padding-left:18px;font-size:13px">
							{#each counts.awaiting.slice(0, 10) as c (c.changeset_id)}
								<li><a href="/cs/{c.changeset_id}">{c.title ?? '提案'}</a> <span class="dim">{c.targets.map((t) => t.key).join('、')}</span></li>
							{/each}
						</ul>
					{:else if !paused.length && !broken.length && !failed.length}
						<div class="empty">目前沒有要你決定的事。</div>
					{/if}
					<button class="btn sm ghost" onclick={() => refreshCounts()}>重新整理</button>
				{:else if w.kind === 'agents'}
					<ErrorBox error={errors.agents} />
					{#if summary}
						{#each summary.today as t (t.agent_principal_id)}
							<div class="row"><span class="agname">{t.label}</span><span class="agent-label">agent</span> 今天改了 <b>{t.issues_changed}</b> 張<span style="flex:1"></span><a class="btn sm" href="/agents/{t.agent_principal_id}/activity">活動</a></div>
						{:else}
							<div class="empty">今天沒有 agent 寫入你看得到的單。</div>
						{/each}
					{/if}
				{:else if w.kind === 'mine'}
					<ErrorBox error={errors.mine} />
					{#if mine}<IssueList items={mine} compact />{/if}
				{:else if w.kind === 'recent'}
					<ErrorBox error={errors.recent} />
					{#if recent}<IssueList items={recent} compact />{/if}
				{:else if w.kind === 'milestones'}
					<ErrorBox error={errors.milestones} />
					{#if milestones}
						{#each milestones as m (m.v.id)}
							{@const h = HEALTH[m.v.health ?? ''] ?? { label: m.v.health, cls: 'c-dim' }}
							<div class="msw">
								<div class="row" style="gap:6px">
									<a href="/project/{m.project}/milestones"><span class="mono">{m.project}</span> {m.v.name}</a>
									<span class="chip {h.cls}" title={(m.v.health_reasons ?? []).map((r) => HEALTH_REASON[r] ?? r).join('；')}>{h.label}</span>
									<span class="dim mono" style="font-size:12px">{m.v.effective_date ?? '未定'}</span>
								</div>
								{#if m.v.stats && m.v.stats.total}
									<div class="prog"><i style="width:{Math.round((m.v.stats.closed / m.v.stats.total) * 100)}%"></i></div>
									<div class="hint">完成 {m.v.stats.closed}／{m.v.stats.total}{m.v.stats.overdue ? `・逾期 ${m.v.stats.overdue}` : ''}</div>
								{/if}
							</div>
						{:else}
							<div class="empty">沒有進行中的里程碑。</div>
						{/each}
					{/if}
				{:else if w.kind === 'query'}
					<div class="hint mono">{w.query}</div>
					<ErrorBox error={queries[w.id]?.error} />
					{#if queries[w.id]?.items}<IssueList items={queries[w.id].items!} compact />{/if}
					<a class="hint" href="/search?q={encodeURIComponent(w.query ?? '')}">全部結果 →</a>
				{/if}
			</section>
		{/each}
	</div>
</div>

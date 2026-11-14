<script lang="ts">
	// Triage inbox (contract 05 §6.4): new issues wait here until someone
	// accepts, declines, marks as duplicate or snoozes them. One project
	// (`projectKey`) or every project the person may triage in (`/triage`).
	import type { SpaIssue, TriageDecision, TriageItem, TriageList } from '$api/types';
	import { api, ApiError } from '#lib/api.js';
	import { fmtTime } from '#lib/format.js';
	import { ago } from '#lib/triage.js';
	import { refreshTriageCounts } from '#lib/counts.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from './ErrorBox.svelte';
	import Actor from './Actor.svelte';
	import TriageDecide from './TriageDecide.svelte';

	let { projectKey = null, projectId = null }: { projectKey?: string | null; projectId?: string | null } = $props();

	const HELP_KEY = 'oxledger.triage.help';
	let help = $state(true);
	try {
		help = localStorage.getItem(HELP_KEY) !== 'hidden';
	} catch {
		/* storage off: show it */
	}
	function toggleHelp() {
		help = !help;
		try {
			localStorage.setItem(HELP_KEY, help ? 'shown' : 'hidden');
		} catch {
			/* nothing to remember with */
		}
	}

	let view = $state<'inbox' | 'decided'>('inbox');
	let snoozed = $state(false);
	let byKind = $state<'all' | 'user' | 'agent'>('all');
	let selfOnly = $state(false);
	let list = $state<TriageList | null>(null);
	let items = $state<TriageItem[]>([]);
	let error = $state<unknown>(null);
	let loadingMore = $state(false);
	let canManage = $state(false);
	let sel = $state(0);
	let panel = $state<{ key: string; decision: TriageDecision } | null>(null);
	let fading = $state<Set<string>>(new Set());

	let seq = 0;
	async function load(more = false) {
		const s = ++seq;
		if (!more) error = null;
		else loadingMore = true;
		const query = {
			include_snoozed: view === 'inbox' && snoozed ? 'true' : undefined,
			view: view === 'decided' ? 'decided' : undefined,
			decided_by_kind: view === 'decided' && byKind !== 'all' ? byKind : undefined,
			self_triage: view === 'decided' && selfOnly ? 'only' : undefined,
			cursor: more ? (list?.next_cursor ?? undefined) : undefined
		};
		try {
			const r = projectKey ? await api('GET /projects/{id}/triage', { path: { id: projectKey }, query }) : await api('GET /triage', { query });
			if (s !== seq) return;
			list = r;
			items = more ? [...items, ...r.items.filter((i) => !items.some((x) => x.id === i.id))] : r.items;
			if (!more) sel = 0;
		} catch (e) {
			if (s !== seq) return;
			if (more && e instanceof ApiError && e.code === 'invalid_cursor') return load(false);
			if (!more) {
				list = null;
				items = [];
			}
			error = e;
		} finally {
			if (s === seq) loadingMore = false;
		}
	}
	$effect(() => {
		void projectKey;
		void view;
		void snoozed;
		void byKind;
		void selfOnly;
		panel = null;
		load();
	});
	$effect(() => {
		if (!projectKey) return;
		api('GET /projects/{id}/triage-settings', { path: { id: projectKey } })
			.then((s) => (canManage = s.can_manage))
			.catch(() => (canManage = false));
	});

	const enabled = $derived(projectKey ? (list?.settings?.enabled ?? true) : true);
	// The cross-project inbox lists only projects the person may triage in.
	const canDecide = $derived(projectKey ? !!list?.can_decide : true);
	const current = $derived(items[sel] ?? null);

	function pid(it: TriageItem) {
		return it.project?.id ?? projectId ?? projectKey ?? '';
	}
	function pkey(it: TriageItem) {
		return it.project?.key ?? projectKey ?? it.key.split('-')[0];
	}

	function open(it: TriageItem, d: TriageDecision) {
		if (!canDecide || view !== 'inbox') return;
		sel = items.indexOf(it);
		panel = { key: it.key, decision: d };
	}
	function done(it: TriageItem, r: 'done' | 'gone' | 'conflict', text?: string) {
		panel = null;
		if (r === 'conflict') {
			toast(text ?? '這張單剛被別人更新，已重新載入', 'w');
			reloadRow(it);
			return;
		}
		toast(text ?? '', r === 'done' ? 'ok' : 'w');
		// The row fades out, then leaves (§6.4).
		fading = new Set([...fading, it.id]);
		setTimeout(() => {
			items = items.filter((x) => x.id !== it.id);
			fading = new Set([...fading].filter((x) => x !== it.id));
			if (sel >= items.length) sel = Math.max(0, items.length - 1);
		}, 300);
		refreshTriageCounts();
	}
	/** A conflict: read the row again (it may have left the inbox meanwhile). */
	async function reloadRow(it: TriageItem) {
		try {
			const r = projectKey ? await api('GET /projects/{id}/triage', { path: { id: projectKey }, query: { include_snoozed: snoozed ? 'true' : undefined, limit: 100 } }) : await api('GET /triage', { query: { include_snoozed: snoozed ? 'true' : undefined, limit: 100 } });
			const fresh = r.items.find((x) => x.id === it.id);
			items = fresh ? items.map((x) => (x.id === it.id ? fresh : x)) : items.filter((x) => x.id !== it.id);
		} catch (e) {
			error = e;
		}
	}

	function typing(e: KeyboardEvent) {
		const t = e.target as HTMLElement | null;
		return !!t && (t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName));
	}
	function keys(e: KeyboardEvent) {
		if (e.ctrlKey || e.metaKey || e.altKey || typing(e) || view !== 'inbox' || !items.length) return;
		const k = e.key.toLowerCase();
		if (k === 'j' || k === 'k') {
			e.preventDefault();
			sel = Math.min(items.length - 1, Math.max(0, sel + (k === 'j' ? 1 : -1)));
			panel = null;
			document.querySelector<HTMLElement>(`[data-row="${items[sel]?.id}"]`)?.scrollIntoView({ block: 'nearest' });
			return;
		}
		const map: Record<string, TriageDecision> = { '1': 'accept', '2': 'decline', '3': 'duplicate', h: 'snooze' };
		if (map[k] && current && !current.snoozed_until) {
			e.preventDefault();
			open(current, map[k]);
		}
	}

	// Preview of the selected row (§6.4 right column): description, 3 notes,
	// possible duplicates by subject.
	let preview = $state<{ key: string; issue: SpaIssue | null; dups: { key: string; subject: string }[]; error: unknown } | null>(null);
	let pseq = 0;
	$effect(() => {
		const it = current;
		if (!it) {
			preview = null;
			return;
		}
		const s = ++pseq;
		preview = { key: it.key, issue: null, dups: [], error: null };
		Promise.all([
			api('GET /issues/{key}/view', { path: { key: it.key }, query: { include: 'recent_notes', note_limit: 3 } }),
			api('POST /issues/search', { body: { q: it.subject, project: pkey(it), limit: 4, explain: 'none', fields: ['key', 'subject'] } }).catch(() => null)
		])
			.then(([v, r]) => {
				if (s !== pseq) return;
				const dups = (r?.items ?? []).filter((x) => x.key !== it.key).slice(0, 3).map((x) => ({ key: x.key, subject: String(x.subject ?? '') }));
				preview = { key: it.key, issue: v, dups, error: null };
			})
			.catch((e) => {
				if (s === pseq) preview = { key: it.key, issue: null, dups: [], error: e };
			});
	});

	const DECISION: Record<string, string> = { accepted: '接受', declined: '拒絕', duplicate: '重複' };
</script>

<svelte:window onkeydown={keys} />

<div class="triage" data-testid="triage">
	<div class="row" style="gap:8px;flex-wrap:wrap">
		<h2 style="margin:0">Triage 收件匣{projectKey ? ` · ${projectKey}` : ''}</h2>
		{#if list?.settings?.responsible}<span class="chip c-prop" data-testid="triage-responsible">本週負責：{list.settings.responsible.name}</span>{/if}
		<span style="flex:1"></span>
		{#if view === 'inbox'}<label><input type="checkbox" bind:checked={snoozed} data-testid="show-snoozed" /> 顯示稍後處理的</label>{/if}
		{#if projectKey && canManage}<a class="btn sm" href="/project/{projectKey}/settings/triage" data-testid="triage-settings-link">設定</a>{/if}
		<button class="btn sm ghost" onclick={toggleHelp} aria-expanded={help}>{help ? '收起說明' : '說明'}</button>
	</div>
	{#if help}<p class="hint" data-testid="triage-help">新進的單先停在這裡。接受、拒絕、標為重複或稍後處理；指派由人決定。鍵盤：J／K 上下、1 接受、2 拒絕、3 重複、H 稍後。</p>{/if}
	{#if projectKey}
		<div class="subtabs" role="tablist">
			<button role="tab" class:on={view === 'inbox'} aria-selected={view === 'inbox'} onclick={() => (view = 'inbox')} data-testid="tab-inbox">待分流</button>
			<button role="tab" class:on={view === 'decided'} aria-selected={view === 'decided'} onclick={() => (view = 'decided')} data-testid="tab-decided">最近決定</button>
		</div>
	{/if}
	{#if view === 'decided'}
		<div class="row" style="gap:12px;margin:6px 0">
			<label>決定者
				<select bind:value={byKind} data-testid="decided-by">
					<option value="all">全部</option><option value="user">人</option><option value="agent">agent</option>
				</select>
			</label>
			<label><input type="checkbox" bind:checked={selfOnly} data-testid="self-only" /> 只看自建自分流</label>
		</div>
	{/if}

	<ErrorBox {error} onaction={() => load()} />
	{#if !list && !error}
		<div class="sync"><span class="spin"></span>載入中…</div>
	{:else if list && !enabled}
		<div class="empty" data-testid="triage-off">這個專案沒有開啟 Triage。{#if canManage} <a href="/project/{projectKey}/settings/triage">開啟 Triage</a>{/if}</div>
	{:else if list}
		{#if view === 'inbox' && !canDecide}<div class="banner i" data-testid="triage-readonly">你可以查看，但不能分流。</div>{/if}
		<div class="tq-grid">
			<div>
				{#each items as it, i (it.id)}
					<div class="tq-row" class:sel={i === sel && view === 'inbox'} class:fade={fading.has(it.id)} data-row={it.id} data-testid="triage-row" data-key={it.key}>
						<button class="tq-main" onclick={() => ((sel = i), (panel = null))}>
							<span class="mono key">{it.key}</span>
							<span class="subj">{it.subject}</span>
							<span class="meta">
								{it.tracker.name}{it.priority ? ` · ${it.priority.name}` : ''} ·
								{#if view === 'decided'}
									<b data-testid="decision">{DECISION[it.decision ?? ''] ?? it.decision}</b> ·
									<span title={it.decided_by?.on_behalf_of ? `代表 ${it.decided_by.on_behalf_of.name}` : ''}><Actor name={it.decided_by?.name ?? '—'} kind={it.decided_by?.kind} /></span>
									· {it.decided_at ? fmtTime(it.decided_at) : ''}
									{#if it.self_triage}<span class="chip c-warn" data-testid="self-triage">自建自分流</span>{/if}
									{#if it.same_sponsor}<span class="chip c-dim">同 sponsor 的 agent</span>{/if}
								{:else}
									<span title={it.author.on_behalf_of ? `代表 ${it.author.on_behalf_of.name}` : ''}><Actor name={it.author.name} kind={it.author.kind} /></span>
									· 進來 {ago(it.entered_at)} · 最近活動 {ago(it.last_activity_at)}
									{#if it.snoozed_until}<span class="chip c-dim" data-testid="snoozed-until">稍後到 {fmtTime(it.snoozed_until)}</span>{/if}
								{/if}
							</span>
						</button>
						{#if view === 'decided'}
							<a class="btn sm" href="/issue/{it.key}">開啟</a>
						{:else if canDecide}
							<div class="acts">
								{#if it.snoozed_until}
									<button class="btn sm" onclick={() => open(it, 'unsnooze')} data-testid="act-unsnooze">取消稍後</button>
								{:else}
									<button class="btn sm pri" onclick={() => open(it, 'accept')} data-testid="act-accept">接受</button>
									<button class="btn sm" onclick={() => open(it, 'decline')} data-testid="act-decline">拒絕</button>
									<button class="btn sm" onclick={() => open(it, 'duplicate')} data-testid="act-dup">重複…</button>
									<button class="btn sm" onclick={() => open(it, 'snooze')} data-testid="act-snooze">稍後</button>
								{/if}
							</div>
						{/if}
					</div>
					{#if panel && panel.key === it.key}
						<TriageDecide
							item={it}
							projectId={pid(it)}
							projectKey={pkey(it)}
							settings={list.settings ?? null}
							decision={panel.decision}
							ondone={(r, t) => done(it, r, t)}
							oncancel={() => (panel = null)}
						/>
					{/if}
				{:else}
					<div class="empty" data-testid="triage-empty">
						{#if view === 'decided'}{selfOnly ? '最近 30 天沒有 agent 分流自己建的單。' : '最近 30 天沒有分流決定。'}{:else}收件匣清空了。{/if}
					</div>
				{/each}
				{#if list.next_cursor}<button class="btn" onclick={() => load(true)} disabled={loadingMore} data-testid="triage-more">{loadingMore ? '載入中…' : '載入更多'}</button>{/if}
			</div>
			{#if view === 'inbox' && preview}
				<aside class="card tq-prev" data-testid="triage-preview" aria-label="議題預覽">
					<h3 style="margin-top:0"><a class="mono" href="/issue/{preview.key}">{preview.key}</a></h3>
					<ErrorBox error={preview.error} />
					{#if preview.issue}
						<div class="pre">{(preview.issue.description ?? '').slice(0, 600) || '（沒有描述）'}{(preview.issue.description ?? '').length > 600 ? '…' : ''}</div>
						<h4>最近的 note</h4>
						{#each preview.issue.recent_notes?.items ?? [] as n (n.note_id)}
							<div class="note"><div class="h">{n.author}・{fmtTime(n.created_at)}</div>{#if n.redacted}<div class="dim">（內容已遮蔽）</div>{:else}<div class="pre">{n.body}</div>{/if}</div>
						{:else}<div class="dim">還沒有 note。</div>{/each}
						<h4>可能重複</h4>
						{#each preview.dups as d (d.key)}<div><a class="mono" href="/issue/{d.key}">{d.key}</a> {d.subject}</div>{:else}<div class="dim">沒有找到相似的單。</div>{/each}
					{:else if !preview.error}<div class="sync"><span class="spin"></span></div>{/if}
				</aside>
			{/if}
		</div>
	{/if}
</div>

<style>
	.subtabs {
		display: flex;
		gap: 4px;
		border-bottom: 1px solid var(--line2);
		margin: 8px 0;
	}
	.subtabs button {
		background: none;
		border: none;
		border-bottom: 2px solid transparent;
		padding: 4px 10px;
		cursor: pointer;
		color: inherit;
	}
	.subtabs button.on {
		border-bottom-color: var(--act);
		font-weight: 600;
	}
	.tq-grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 340px);
		gap: 12px;
		align-items: start;
	}
	@media (max-width: 1100px) {
		.tq-grid {
			grid-template-columns: 1fr;
		}
	}
	.tq-row {
		display: flex;
		gap: 8px;
		align-items: center;
		border-bottom: 1px solid var(--line2);
		padding: 6px 4px;
		transition: opacity 0.3s;
	}
	.tq-row.sel {
		background: var(--card2);
		box-shadow: inset 3px 0 0 var(--act);
	}
	.tq-row.fade {
		opacity: 0;
	}
	.tq-main {
		flex: 1;
		min-width: 0;
		text-align: left;
		background: none;
		border: none;
		color: inherit;
		cursor: pointer;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.subj {
		font-weight: 600;
	}
	.meta {
		font-size: 12px;
		color: var(--dim);
	}
	.acts {
		display: flex;
		gap: 4px;
		flex-wrap: wrap;
	}
	.tq-prev h4 {
		margin: 10px 0 4px;
	}
</style>

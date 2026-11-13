<script lang="ts">
	import { safeHref } from '$lib/format';
	// One issue inside its project frame (§18.6), with direct operations
	// (human_direct, §6.8): edit fields, move status (only allowed ones), add a
	// note (optionally private), mark as duplicate. A conflict never
	// overwrites and keeps what the person typed.
	import { page } from '$app/state';
	import type { Assignee, CommandDoc, FieldSchema, SpaIssue, TrackerSchema, VersionView } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { changedFields, doc, issueRef, submit, wireFields } from '$lib/commands';
	import { fmtTime, fmtValue, fieldLabel } from '$lib/format';
	import { refreshCounts } from '$lib/counts.svelte';
	import { session } from '$lib/session.svelte';
	import { toast } from '$lib/toast.svelte';
	import { FLAG } from '$lib/health';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import { issueValues, labelOf, show } from '$lib/render';
	import type { InboxSettings, IssueTriage, TimeEntryView, TriageDecision } from '$api/types';
	import { logTimeDoc, logTimeOp, showHours, type LogInput } from '$lib/hours';
	import { refreshTriageCounts } from '$lib/counts.svelte';
	import TriageDecide from '$lib/components/TriageDecide.svelte';
	import TimeCard from '$lib/components/TimeCard.svelte';
	import LogTime from '$lib/components/LogTime.svelte';
	import FormSections from '$lib/components/FormSections.svelte';
	import MarkdownEditor from '$lib/components/MarkdownEditor.svelte';
	import RichText from '$lib/components/RichText.svelte';
	import { draftKey, dropDraft, formatFor, LIMIT, utf8Bytes, type Mentionable } from '$lib/markdown';
	import type { FormDescription } from '$api/types';
	import { condValues, hiddenKeys, requiredNow } from '$lib/formcond';
	import { RULE_KIND } from '$lib/formdesign';
	import type { WatchInfo } from '$api/types';
	import WatchButton from '$lib/components/WatchButton.svelte';
	import { markIssueRead } from '$lib/notifs.svelte';

	const key = $derived(decodeURIComponent(page.params.key ?? '').toUpperCase());
	let issue = $state<SpaIssue | null>(null);
	let loadError = $state<unknown>(null);
	let schema = $state<TrackerSchema | null>(null);
	let assignees = $state<Assignee[]>([]);
	let versions = $state<VersionView[]>([]);
	let today = $state<string>(new Date().toLocaleDateString('sv-SE', { timeZone: 'Asia/Taipei' }));

	// Editing (kept across a reload so a conflict never loses input).
	let mode = $state<null | 'edit' | 'note' | 'dup' | 'transition'>(null);
	let start = $state<Record<string, unknown>>({});
	let form = $state<Record<string, unknown>>({});
	let note = $state({ body: '', private: false });
	let noteOver = $state(false);
	let dupKey = $state('');
	let actionError = $state<unknown>(null);
	let conflict = $state(false);
	/** Fields the person had changed when a conflict reload merged the newest values. */
	let kept = $state<string[]>([]);
	let busy = $state(false);
	let ripple = $state(false);

	async function load(k: string) {
		loadError = null;
		try {
			const v = await api('GET /issues/{key}/view', {
				path: { key: k },
				query: { include: 'actions,recent_notes,recent_events,dev', note_limit: 10, event_limit: 20 }
			});
			issue = v;
			loadDescribe(v);
			if (!schema || schema.project_id !== v.project_id || schema.tracker_id !== v.tracker_id) {
				const [s, a, vl] = await Promise.all([
					api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: v.project_id, tracker_id: v.tracker_id } }),
					api('GET /projects/{id}/assignees', { path: { id: v.project_id } }).catch(() => ({ assignees: [] as Assignee[], project_id: '' })),
					api('GET /projects/{id}/versions', { path: { id: v.project_id }, query: { status: 'open,locked' } }).catch(() => null)
				]);
				schema = s;
				assignees = a.assignees;
				versions = vl?.versions ?? [];
				if (vl) today = vl.today;
			}
		} catch (e) {
			issue = null;
			loadError = e;
		}
	}
	$effect(() => {
		mode = null;
		conflict = false;
		triagePanel = null;
		load(key);
	});

	// ---- Triage bar (contract 05 §6.4): while the issue waits in the inbox ----
	let triage = $state<IssueTriage | null>(null);
	let inbox = $state<InboxSettings | null>(null);
	let triagePanel = $state<TriageDecision | null>(null);
	$effect(() => {
		const v = issue;
		if (!v || (v.triage !== 'pending' && v.triage !== 'snoozed')) {
			triage = null;
			return;
		}
		const k = v.key;
		api('GET /issues/{key}', { path: { key: k } })
			.then((d) => {
				if (issue?.key === k) triage = d.triage;
			})
			.catch(() => (triage = null));
		api('GET /projects/{id}/triage', { path: { id: v.project_id }, query: { limit: 1 } })
			.then((l) => (inbox = l.settings ?? null))
			.catch(() => (inbox = null));
	});
	function triageDone(r: 'done' | 'gone' | 'conflict', text?: string) {
		triagePanel = null;
		toast(text ?? '', r === 'done' ? 'ok' : 'w');
		load(key);
		refreshTriageCounts();
	}

	// ---- time (contract 05 §6.6) ----
	let timeEntries = $state<TimeEntryView[]>([]);
	let timeReload = $state(0);
	let logWith = $state(false);
	let logEntry = $state<LogInput | null>(null);
	let timeHint = $state('');

	const currentValues = (v: SpaIssue) => issueValues(v, schema?.fields ?? []);
	const editable = $derived.by(() => {
		const a = issue?.actions;
		if (!a || !schema) return [] as { f: FieldSchema; required: boolean }[];
		return schema.fields
			.filter((f) => f.ops.includes('update'))
			.map((f) => ({ f, act: a.fields.find((x) => x.key === f.key) }))
			.filter((x) => x.act?.editable)
			.map((x) => ({ f: x.f, required: !!x.act?.required }));
	});

	// ---- form (contract 05 §6.14): layout, show_if, required_if, rule marks ----
	let describe = $state<FormDescription | null>(null);
	/** The view does not carry `form_rule_marks` (gap G32): read the issue itself. */
	let fetchedMarks = $state<string[]>([]);
	/** §6.16: the caller's watch state, also only on `GET /issues/{key}`. */
	let watch = $state<WatchInfo | null>(null);
	let readFor = '';
	function loadDescribe(v: SpaIssue) {
		api('GET /issues/{key}', { path: { key: v.key } })
			.then((d) => {
				if (issue?.key !== v.key) return;
				fetchedMarks = v.form_rule_marks ?? d.form_rule_marks ?? [];
				watch = d.watch ?? null;
			})
			.catch(() => {
				fetchedMarks = v.form_rule_marks ?? [];
				watch = null;
			});
		// §3.11.3: opening the issue reads its notification.
		if (readFor !== v.key) {
			readFor = v.key;
			markIssueRead(v.key);
		}
		api('GET /projects/{id}/trackers/{t}/form', { path: { id: v.project_id, t: v.tracker_id }, query: { op: 'update', issue: v.key } })
			.then((d) => {
				if (issue?.key === v.key) describe = d;
			})
			.catch(() => (describe = null));
	}
	/** Rule id → 「根本原因 必填」 (from the rules the reader may see). */
	const ruleText = $derived.by(() => {
		const m = new Map<string, { key: string; text: string }>();
		for (const f of describe?.fields ?? []) {
			const name = f.name ?? fieldLabel(f.key);
			for (const [list, kind] of [[f.required_if, 'required_if'], [f.allowed_values_if, 'allowed_values_if'], [f.readonly_if, 'readonly_if']] as const)
				for (const r of list ?? []) m.set(r.rule_id, { key: f.key, text: `${name} ${kind === 'allowed_values_if' ? '的值不在允許清單' : RULE_KIND[kind]}` });
		}
		return m;
	});
	const marks = $derived(fetchedMarks);
	const markedKeys = $derived(new Set(marks.map((id) => ruleText.get(id)?.key).filter((k): k is string => !!k)));
	const editValues = $derived(issue ? condValues(form, schema?.fields ?? [], { tracker: issue.tracker_id, status: issue.status_id, status_category: issue.status_category ?? null, is_private: issue.is_private ?? null }) : {});
	const editHidden = $derived(hiddenKeys(describe?.layout, editValues));
	const editEntries = $derived(
		editable.map((e) => {
			const d = describe?.fields.find((f) => f.key === e.f.key);
			return { f: e.f, required: e.required || (d ? requiredNow({ required: false, required_if: d.required_if }, editValues) : false) };
		})
	);
	const editMissing = $derived(editEntries.filter((e) => e.required && !editHidden.has(e.f.key) && (form[e.f.key] === undefined || form[e.f.key] === null || String(form[e.f.key]).trim() === '')));

	// §6.2 / R-M9: the description is edited as Markdown; a plain one shows the
	// conversion notice, an agent's one the link warning (R-M3).
	const editMd = $derived(
		issue
			? {
					context: { issue: issue.key },
					notice: issue.description_format === 'plain' ? ('plain' as const) : (issue as { description_by_agent?: boolean | null }).description_by_agent ? ('agent' as const) : null,
					draft: session.user ? draftKey(session.user.user_id, `edit:${issue.key}`) : null,
					// After a conflict reload `start` is the newer version: compare to it.
					base: String(start.description ?? '')
				}
			: null
	);
	const descOver = $derived(mode === 'edit' && utf8Bytes(String(form.description ?? '')) > LIMIT.description);
	function openEdit() {
		if (!issue) return;
		start = currentValues(issue);
		form = { ...start };
		mode = 'edit';
		actionError = null;
		conflict = false;
		kept = [];
	}
	function openMode(m: 'note' | 'dup' | 'transition') {
		mode = m;
		actionError = null;
		conflict = false;
	}

	// The last document sent and its Idempotency-Key: a transient failure
	// ("尚未確認是否完成") is retried with exactly the same pair (§8.7);
	// anything else gets a fresh key next time.
	let last = $state<{ d: CommandDoc; op: string; done: string } | null>(null);
	/** Outcome unknown: only the same request may go out again (no edits, no new key). */
	const uncertain = $derived(last !== null && !busy);

	async function send(d: CommandDoc, done: string, op = newOperationId()) {
		busy = true;
		actionError = null;
		last = { d, op, done };
		try {
			await submit(d, op);
			last = null;
			toast(done, 'ok');
			// What was just sent is no longer a draft (§6.2).
			if (session.user && (mode === 'note' || mode === 'edit')) dropDraft(draftKey(session.user.user_id, `${mode}:${issue?.key ?? key}`));
			mode = null;
			conflict = false;
			note = { body: '', private: false };
			dupKey = '';
			logWith = false;
			timeReload++;
			ripple = true;
			setTimeout(() => (ripple = false), 400);
			await load(key);
			refreshCounts();
		} catch (e) {
			actionError = e;
			if (!(e instanceof ApiError && e.retryable)) last = null;
			if (e instanceof ApiError && e.code === 'conflict') conflict = true;
		} finally {
			busy = false;
		}
	}
	function retry() {
		if (last) send(last.d, last.done, last.op);
	}
	const op = () => newOperationId();
	function onErrorAction(a: string) {
		if (a === 'retry') retry();
		else if (a === 'transitions') reloadKeep().then(() => openMode('transition'));
		else if (a === 'login') session.expired = true;
		else reloadKeep();
	}

	function saveEdit() {
		if (!issue) return;
		// A field the layout hides right now is not sent (show_if, §6.14).
		const changed = changedFields(start, form);
		for (const k of editHidden) delete changed[k];
		const set = wireFields(changed, schema?.fields ?? []);
		// 「同時記工時」 (§6.6): the entry rides in the same document (§3.10.2).
		if (logWith && !logEntry) {
			// Opened but not valid: never save silently without the time.
			timeHint = '「同時記工時」的時數或日期不完整；請修正，或收起「同時記工時」再儲存。';
			return;
		}
		timeHint = '';
		const withTime = logWith && logEntry ? logEntry : null;
		if (!Object.keys(set).length && !withTime) {
			mode = null;
			return;
		}
		const lock = issue.lock_version ?? 0;
		// R-M9: rewriting a description written before M2 (plain) converts it to Markdown.
		const fmt = formatFor(issue.description_format, 'description' in set);
		const ops = Object.keys(set).length ? [{ kind: 'update_fields' as const, op_id: op(), target: issueRef(key), set, ...(fmt ? { format: fmt } : {}) }] : [];
		if (withTime) ops.push(logTimeOp(withTime) as never);
		const done = withTime ? (ops.length > 1 ? `已儲存，並記了 ${showHours(withTime.hours)} 小時` : `已記 ${showHours(withTime.hours)} 小時`) : '已儲存';
		const d = ops.length > 1 || !withTime ? doc(ops, [{ key, lock }]) : logTimeDoc(withTime);
		return send(d, done);
	}
	function transition(to: string, name: string) {
		if (!issue) return;
		const lock = issue.lock_version ?? 0;
		return send(doc([{ kind: 'transition', op_id: op(), target: issueRef(key), to_status_id: to }], [{ key, lock }]), `已轉到「${name}」`);
	}
	function addNote() {
		if (!issue || !note.body.trim()) return;
		const lock = issue.lock_version ?? 0;
		return send(
			doc([{ kind: 'add_note', op_id: op(), target: issueRef(key), body: note.body, is_private: note.private }], [{ key, lock }]),
			note.private ? '已新增私有 note' : '已新增 note'
		);
	}
	async function markDup() {
		if (!issue) return;
		const target = dupKey.trim().toUpperCase();
		const lock = issue.lock_version ?? 0;
		actionError = null;
		let t;
		try {
			t = await api('GET /issues/{key}', { path: { key: target } });
		} catch (e) {
			actionError = e;
			return;
		}
		return send(
			doc([{ kind: 'duplicate_of', op_id: op(), source: issueRef(key), target: issueRef(t.key) }], [
				{ key, lock },
				{ key: t.key, lock: t.lock_version }
			]),
			`已標為 ${t.key} 的重複（不會關單）`
		);
	}
	/** Conflict: show the newest values, keep the person's input. */
	async function reloadKeep() {
		// Only what the person actually changed survives; every other field
		// takes the newest value, so saving never reverts someone else's edit.
		const mine = mode === 'edit' ? changedFields(start, form) : {};
		await load(key);
		if (mode === 'edit' && issue) {
			const fresh = currentValues(issue);
			start = fresh;
			form = { ...fresh, ...mine };
			kept = Object.keys(mine);
		}
		conflict = false;
		actionError = null;
	}

	const flags = $derived.by(() => {
		const f: string[] = [];
		if (!issue) return f;
		const closed = issue.status_category === 'done';
		if (!closed && issue.due_date && issue.due_date < today) f.push('overdue');
		const vd = issue.fixed_version?.effective_date;
		if (vd && issue.due_date && issue.due_date > vd) f.push('late_for_version');
		if (!closed && issue.fixed_version && (issue.fixed_version.status === 'closed' || issue.fixed_version.deleted)) f.push('in_closed_version');
		return f;
	});
	const EVENT: Record<string, string> = {
		created: '建立',
		fields_changed: '修改欄位',
		status_changed: '轉狀態',
		note_added: '新增 note',
		duplicate_added: '標為重複',
		triage_changed: '分流'
	};
	const TRIAGE: Record<string, string> = { pending: '待分流', snoozed: '稍後處理', accepted: '接受', declined: '拒絕', duplicate: '重複' };
	/** History with time entries merged in (§6.6: they come from their own list, by their own visibility). */
	type HistItem = { at: string; ev?: NonNullable<NonNullable<SpaIssue['recent_events']>['items']>[number]; te?: TimeEntryView };
	const history = $derived.by(() => {
		const out: HistItem[] = (issue?.recent_events?.items ?? []).map((ev) => ({ at: ev.at, ev }));
		for (const te of timeEntries) out.push({ at: te.created_at, te });
		return out.sort((a, b) => (a.at < b.at ? 1 : a.at > b.at ? -1 : 0));
	});
	/** History names custom fields `cf:N` and stores `{"<type>": v}`. */
	const lookups = $derived({ fields: schema?.fields ?? [], assignees, versions });
	const changeLabel = (k: string) => labelOf(k, lookups);
	const changeValue = (k: string, v: unknown) => show(k, v, lookups);
	function eventLines(detail: unknown): string[] {
		if (!detail || typeof detail !== 'object') return [];
		const d = detail as Record<string, unknown>;
		const out: string[] = [];
		const changes = (d.changes ?? d.fields) as Record<string, { old?: unknown; new?: unknown; before?: unknown; after?: unknown }> | undefined;
		if (changes && typeof changes === 'object') {
			for (const [k, c] of Object.entries(changes)) {
				if (c && typeof c === 'object') out.push(`${changeLabel(k)}：${changeValue(k, c.old ?? c.before)} → ${changeValue(k, c.new ?? c.after)}`);
			}
		}
		if ('old' in d || 'new' in d) out.push(`${changeValue('', d.old)} → ${changeValue('', d.new)}`);
		if (typeof d.other_key === 'string') out.push(d.role === 'source' ? `標為 ${d.other_key} 的重複` : `${d.other_key} 標為這張的重複`);
		for (const [k, v] of Object.entries(d)) {
			if (k === 'changes' || k === 'fields') continue;
			if (['from', 'to', 'status', 'target', 'source', 'approver', 'origin', 'on_behalf_of', 'is_private'].includes(k)) out.push(`${fieldLabel(k)}：${fmtValue(v)}`);
		}
		return out;
	}
	const PR: Record<string, string> = { open: '開啟', draft: '草稿', merged: '已合併', closed: '已關閉' };
	const CI: Record<string, { t: string; c: string }> = {
		success: { t: 'CI 通過', c: 'c-ok' },
		failure: { t: 'CI 失敗', c: 'c-bad' },
		pending: { t: 'CI 執行中', c: 'c-warn' }
	};
	const REVIEW: Record<string, string> = { approved: '已核可', changes_requested: '要求修改', pending: '等 review', none: '沒有 review' };
</script>

<svelte:head><title>{key}・oxledger</title></svelte:head>

{#if loadError}
	<div class="page">
		<!-- §18.6: no project frame when the issue cannot be seen. -->
		<ErrorBox error={loadError} onaction={() => load(key)} />
		<a href="/search">← 回到議題</a>
	</div>
{:else if issue}
	{@const v = issue}
	{@const vx = v as typeof v & { description_html?: string | null; description_by_agent?: boolean | null }}
	<ProjectFrame projectKey={v.project_key} issuePage>
		{#if v.requested_key && v.requested_key !== v.key}
			<div class="banner i">{v.requested_key} 已搬到 {v.key}。</div>
		{/if}
		<div class="row" style="gap:8px;margin:8px 0" class:ripple>
			<span class="key" style="font-size:15px">{v.key}</span>
			<h1 style="margin:0" data-testid="issue-subject">{v.subject}</h1>
			{#if v.is_private}<span class="chip c-warn">私有</span>{/if}
			<span style="flex:1"></span>
			{#if watch}<WatchButton issueKey={v.key} {watch} />{/if}
		</div>
		<div class="propstrip" data-testid="props">
			<div class="pc"><span class="k">狀態</span><span data-testid="status">{v.status}</span></div>
			<div class="pc"><span class="k">類型</span>{v.tracker}</div>
			<div class="pc"><span class="k">優先度</span>{v.priority ?? '—'}</div>
			<div class="pc"><span class="k">指派</span>{v.assignee ?? '未指派'}{v.assignee_kind === 'group' ? '（群組）' : ''}</div>
			<div class="pc"><span class="k">里程碑</span>
				{#if v.fixed_version}<a href="/project/{v.project_key}/milestones">{v.fixed_version.name}{v.fixed_version.deleted ? '（已刪除）' : ''}</a>{:else}—{/if}
			</div>
			<div class="pc"><span class="k">預估工時</span><span data-testid="estimated">{v.estimated_hours != null ? `${showHours(v.estimated_hours)} 小時` : '—'}</span></div>
			<div class="pc"><span class="k">開始 → 到期</span><span class="mono">{v.start_date ?? '—'} → {v.due_date ?? '—'}</span></div>
			<div class="pc"><span class="k">作者</span>{v.author}</div>
			<div class="pc"><span class="k">更新</span>{fmtTime(v.updated_at)}</div>
			{#each flags as f (f)}<div class="pc"><span class="chip {FLAG[f].cls}">{FLAG[f].label}</span></div>{/each}
		</div>
		{#if v.custom_fields?.length}
			<dl class="field card" style="margin-bottom:10px">
				{#each v.custom_fields as cf (cf.name)}<dt>{cf.name}</dt><dd>{fmtValue(cf.value)}</dd>{/each}
			</dl>
		{/if}

		{#if marks.length}
			<div class="banner w" data-testid="rule-marks">
				這張單不符合目前的表單規則（{marks.map((id) => ruleText.get(id)?.text ?? '一條你看不到的規則').join('、')}），下次修改時要補齊。
			</div>
		{/if}

		<div class="actbar" data-testid="actbar">
			{#if editable.length}<button class="btn" onclick={openEdit} disabled={uncertain} data-testid="edit">編輯</button>{/if}
			{#if v.actions?.transitions.length}<button class="btn" onclick={() => openMode('transition')} disabled={uncertain} data-testid="transition">轉狀態</button>{/if}
			{#if v.actions?.notes?.can_add}<button class="btn" onclick={() => openMode('note')} disabled={uncertain} data-testid="add-note">新增 note</button>{/if}
			{#if v.actions?.duplicate_of?.can_propose}<button class="btn" onclick={() => openMode('dup')} disabled={uncertain} data-testid="dup">標為重複</button>{/if}
		</div>

		{#if triage && (triage.state === 'pending' || triage.state === 'snoozed')}
			<div class="banner i triagebar" data-testid="triage-bar">
				<b>這張單在 Triage 收件匣（{v.project_key}）</b>{#if triage.snoozed_until}・稍後處理到 {fmtTime(triage.snoozed_until)}{/if}
				<span class="sp"></span>
				{#if triage.can_decide}
					{#if triage.snoozed_until}
						<button class="btn sm" onclick={() => (triagePanel = 'unsnooze')} data-testid="tb-unsnooze">取消稍後</button>
					{:else}
						<button class="btn sm pri" onclick={() => (triagePanel = 'accept')} data-testid="tb-accept">接受</button>
						<button class="btn sm" onclick={() => (triagePanel = 'decline')} data-testid="tb-decline">拒絕</button>
						<button class="btn sm" onclick={() => (triagePanel = 'duplicate')} data-testid="tb-dup">重複…</button>
						<button class="btn sm" onclick={() => (triagePanel = 'snooze')} data-testid="tb-snooze">稍後</button>
					{/if}
				{:else}
					<span class="dim">你可以查看，但不能分流。</span>
				{/if}
				<a class="btn sm ghost" href="/project/{v.project_key}/triage">收件匣 →</a>
			</div>
			{#if triagePanel}
				<TriageDecide
					item={{ key: v.key, lock_version: v.lock_version ?? 0, subject: v.subject ?? '' }}
					projectId={v.project_id}
					projectKey={v.project_key}
					settings={inbox}
					decision={triagePanel}
					ondone={triageDone}
					oncancel={() => (triagePanel = null)}
				/>
			{/if}
		{/if}

		{#if conflict}
			<div class="banner w" data-testid="conflict">
				這張單在你開啟後被別人改過，尚未套用任何變更；你填的內容還在。
				<span class="sp"></span><button class="btn sm" onclick={reloadKeep}>載入最新內容（保留我的輸入）</button>
			</div>
		{:else}
			<ErrorBox error={actionError} onaction={onErrorAction} />
		{/if}

		{#if mode === 'edit'}
			<div class="card">
				<h3>編輯</h3>
				{#if kept.length}
					<div class="hint" data-testid="kept">已載入最新內容；只保留你改過的欄位：{kept.map((k) => labelOf(k, lookups)).join('、')}。其他欄位是別人剛改的最新值。</div>
				{/if}
				<FormSections fields={editEntries} layout={describe?.layout} bind:form values={editValues} {assignees} {versions} marks={markedKeys} md={editMd} />
				{#if editMissing.length}<div class="hint" data-testid="edit-missing">還要填：{editMissing.map((e) => e.f.name ?? fieldLabel(e.f.key)).join('、')}</div>{/if}
				{#if v.spent_hours != null}
					<details class="withtime" bind:open={logWith} data-testid="edit-with-time">
						<summary>同時記工時</summary>
						{#if logWith}<LogTime projectId={v.project_id} issueKey={v.key} embedded bind:value={logEntry} />{/if}
						{#if timeHint && logWith}<div class="err" role="alert" data-testid="time-hint">{timeHint}</div>{/if}
					</details>
				{/if}
				<div class="row">
					<button class="btn pri" onclick={saveEdit} disabled={busy || conflict || uncertain || descOver} data-testid="save">{busy ? '儲存中…' : '儲存'}</button>
					<button class="btn" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{:else if mode === 'transition'}
			<div class="card">
				<h3>轉到哪個狀態？</h3>
				<p class="hint">只列出你現在可以用的轉換。</p>
				<div class="row">
					{#each v.actions?.transitions ?? [] as t (t.to_status_id)}
						<button class="btn" disabled={busy || t.blocked || conflict || uncertain} title={t.blocked ? '有你看不到的條件擋住這個轉換' : t.then_required.length ? `轉換後必填：${t.then_required.map(fieldLabel).join('、')}` : ''} onclick={() => transition(t.to_status_id, t.name)}>{t.name}</button>
					{/each}
					<button class="btn ghost" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{:else if mode === 'note'}
			<div class="card">
				<h3>新增 note</h3>
				<MarkdownEditor
					bind:value={note.body}
					bind:over={noteOver}
					people={assignees as Mentionable[]}
					context={{ issue: v.key }}
					limit={LIMIT.note}
					draft={session.user ? draftKey(session.user.user_id, `note:${v.key}`) : null}
					disabled={uncertain}
					placeholder="寫給專案成員看的補充"
					label="note 內容"
					testid="note-md"
					inputTestid="note-body"
					base=""
				/>
				<div class="row" style="margin-top:6px">
					{#if v.actions?.notes?.can_set_private}<label><input type="checkbox" bind:checked={note.private} /> 私有（只有能看私有 note 的人看得到）</label>{/if}
					<span style="flex:1"></span>
					<button class="btn pri" onclick={addNote} disabled={busy || !note.body.trim() || noteOver || conflict || uncertain} data-testid="note-submit">新增</button>
					<button class="btn" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{:else if mode === 'dup'}
			<div class="card">
				<h3>標為重複</h3>
				<p class="hint">只建立「重複」關係，不會關閉任何單；兩張要在同一個專案。</p>
				<div class="inline-form">
					<span>{v.key} 是</span>
					<input type="text" bind:value={dupKey} placeholder="{v.project_key}-1" style="width:140px" data-testid="dup-target" />
					<span>的重複</span>
					<button class="btn pri" onclick={markDup} disabled={busy || !dupKey.trim() || conflict || uncertain} data-testid="dup-submit">建立關係</button>
					<button class="btn" onclick={() => (mode = null)}>取消</button>
				</div>
			</div>
		{/if}

		<div class="grid2">
			<div>
				<h2>描述</h2>
				<div class="card"><RichText html={vx.description_html} text={v.description} agent={!!vx.description_by_agent} empty="（沒有描述）" testid="description" /></div>
				{#if v.children?.length}
					<h2>子議題</h2>
					{#each v.children as c (c.key)}<div><a class="key" href="/issue/{c.key}">{c.key}</a> {c.subject} <span class="dim">{c.status}</span></div>{/each}
				{/if}
				<h2>Note</h2>
				{#each v.recent_notes?.items ?? [] as n (n.note_id)}
					<div class="note" class:priv={n.is_private} data-testid="note">
						<div class="h">{n.author}・{fmtTime(n.created_at)} {#if n.is_private}<span class="chip c-warn">私有</span>{/if}</div>
						{#if n.redacted}<div class="dim">（內容已遮蔽）</div>
						{:else if n.retracted}<div class="dim">（已由 {n.retracted_by ?? '委派者'} 撤回）</div>
						{:else}{@const nx = n as typeof n & { body_html?: string | null }}<RichText html={nx.body_html} text={n.body} testid="note-text" />{/if}
					</div>
				{:else}
					<div class="empty">還沒有 note。</div>
				{/each}
				{#if v.recent_notes?.truncated}<div class="hint">只顯示最新 10 則（第一版沒有完整 note 分頁）。</div>{/if}
			</div>
			<div>
				{#if v.spent_hours != null}
					<TimeCard issueKey={v.key} projectId={v.project_id} projectKey={v.project_key} estimated={v.estimated_hours} spent={v.spent_hours} reloadKey={timeReload} bind:entries={timeEntries} onlogged={() => { timeReload++; load(key); }} />
				{/if}
				{#if v.dev}
					<div class="card" data-testid="dev-card">
						<h3>開發</h3>
						{#if v.dev.merged_open}<div class="banner i" style="font-size:12.5px">有 PR 已合併，但這張單還沒結案（只提示，不會自動改單）。</div>{/if}
						{#each v.dev.prs as pr (pr.pr_id)}
							<div class="dyn">
								<a href={safeHref(pr.url)} target="_blank" rel="noopener noreferrer">{pr.repo}#{pr.number}</a> <span class="chip c-dim">{PR[pr.state] ?? pr.state}</span>
								{#if pr.ci_state}<span class="chip {CI[pr.ci_state]?.c ?? 'c-dim'}">{CI[pr.ci_state]?.t ?? pr.ci_state}</span>{/if}
								<div style="font-size:12.5px">{pr.title}</div>
								<div class="hint">{REVIEW[pr.review_state] ?? pr.review_state}{pr.accessible ? '' : '・App 已無法讀取此 repo'}</div>
							</div>
						{:else}
							<div class="empty">還沒有 PR 提到 {v.key}。在 PR 標題、分支名或 commit 訊息寫上單號就會出現。</div>
						{/each}
						{#if v.dev.synced_at}<div class="hint">同步於 {fmtTime(v.dev.synced_at)}；可能落後 GitHub 幾分鐘。</div>{/if}
					</div>
				{/if}
				<h2>歷史</h2>
				{#each history as h (h.ev ? `e${h.ev.event_id}` : `t${h.te?.id}`)}
					{#if h.ev}
						{@const ev = h.ev}
						<div class="ev" data-testid="event">
							<b>{EVENT[ev.kind] ?? ev.kind}</b>・<span>{ev.actor}</span>
							<span class="dim">・{fmtTime(ev.at)}</span>
							{#if ev.redacted}<div class="dim">（內容已遮蔽）</div>{/if}
							{#if ev.kind === 'triage_changed' && ev.detail && typeof ev.detail === 'object'}
								{@const d = ev.detail as Record<string, unknown>}
								<div>{TRIAGE[String(d.old)] ?? d.old ?? '—'} → {TRIAGE[String(d.new)] ?? d.new}{#if d.self_triage}<span class="chip c-warn" style="margin-left:4px">自建自分流</span>{/if}</div>
							{:else}
								{#each eventLines(ev.detail) as line, i (i)}<div>{line}</div>{/each}
							{/if}
						</div>
					{:else if h.te}
						{@const te = h.te}
						<div class="ev tev" data-testid="time-event">
							<span>{te.user.name}</span>{#if te.author.kind === 'agent'}<span class="agent-label" style="margin-left:4px" title={te.author.on_behalf_of ? `代表 ${te.author.on_behalf_of.name}` : ''}>agent</span>{/if}
							記了 {showHours(te.hours)} 小時・{te.activity.name}
							<span class="dim">・{fmtTime(te.created_at)}（{te.spent_on}）</span>
						</div>
					{/if}
				{:else}
					<div class="empty">沒有歷史。</div>
				{/each}
				{#if v.recent_events?.truncated}<div class="hint">只顯示最新 20 筆。</div>{/if}
			</div>
		</div>
	</ProjectFrame>
{:else}
	<div class="page"><div class="sync"><span class="spin"></span>載入中</div></div>
{/if}

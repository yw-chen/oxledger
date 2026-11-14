<script lang="ts">
	// 回報問題 (§14, human_direct create_issue). Fields come from the tracker
	// schema (only those the caller may set on create); nothing is written
	// until the person presses 建立. After creating, the issue page reads the
	// DB directly; the search page shows it as "剛建立・同步中" (Q-S10) until
	// the index catches up.
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import type { Assignee, Candidate, TrackerSchema, VersionView } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { doc, submit, wireFields } from '$lib/commands';
	import { session } from '$lib/session.svelte';
	import { refreshCounts } from '$lib/counts.svelte';
	import { rememberCreated } from '$lib/created.svelte';
	import { toast } from '$lib/toast.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import FormSections from '$lib/components/FormSections.svelte';
	import { draftKey, dropDraft, LIMIT, utf8Bytes } from '$lib/markdown';
	import type { FormDescription } from '$api/types';
	import { condValues, hiddenKeys, requiredNow } from '$lib/formcond';
	import { statusCategories } from '$lib/triage';
	import { listTrackers } from '$lib/trackers';

	const initialProject = (page.url.searchParams.get('project') ?? '').toUpperCase();
	let projectKey = $state(session.projects.some((p) => p.key === initialProject) ? initialProject : (session.projects[0]?.key ?? ''));
	let trackers = $state<Candidate[]>([]);
	let trackerId = $state('');
	let schema = $state<TrackerSchema | null>(null);
	/** Layout and conditional rules (contract 05 §6.14); the schema still decides what may be set. */
	let describe = $state<FormDescription | null>(null);
	let assignees = $state<Assignee[]>([]);
	let versions = $state<VersionView[]>([]);
	let form = $state<Record<string, unknown>>({ subject: '', description: page.url.searchParams.get('description') ?? '' });
	let isPrivate = $state(false);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	/** Sent: leaving for the new issue needs no "unsent text" question. */
	let created = $state(false);
	// §6.2: the description as Markdown, with a draft per person.
	const newDraft = $derived(session.user ? draftKey(session.user.user_id, 'new') : null);
	const descOver = $derived(utf8Bytes(String(form.description ?? '')) > LIMIT.description);
	// Same document + Idempotency-Key when retrying a transient failure (§8.7).
	// While it is set the outcome is unknown: the form is locked so the only way
	// on is the same request again (a second 建立 would create a second issue,
	// and edits would silently not be what gets sent).
	let last = $state<{ body: ReturnType<typeof doc>; op: string } | null>(null);
	const uncertain = $derived(last !== null && !busy);

	const project = $derived(session.projects.find((p) => p.key === projectKey) ?? null);

	$effect(() => {
		listTrackers()
			.then((t) => {
				trackers = t;
				if (!trackerId && t.length) trackerId = t[0].id;
			})
			.catch((e) => (loadError = e));
	});

	$effect(() => {
		const p = project;
		const t = trackerId;
		if (!p || !t) return;
		schema = null;
		describe = null;
		loadError = null;
		api('GET /projects/{id}/trackers/{t}/form', { path: { id: p.id, t }, query: { op: 'create' } })
			.then((d) => {
				if (p === project && t === trackerId) describe = d;
			})
			.catch(() => {});
		Promise.all([
			api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: p.id, tracker_id: t } }),
			api('GET /projects/{id}/assignees', { path: { id: p.id } }).catch(() => ({ assignees: [] as Assignee[], project_id: '' })),
			api('GET /projects/{id}/versions', { path: { id: p.id }, query: { status: 'open' } }).catch(() => null)
		])
			.then(([s, a, v]) => {
				if (p !== project || t !== trackerId) return;
				schema = s;
				assignees = a.assignees;
				versions = v?.versions ?? [];
				isPrivate = s.creation.is_private.default;
			})
			.catch((e) => (loadError = e));
	});

	const fields = $derived((schema?.fields ?? []).filter((f) => f.ops.includes('create') && f.key !== 'is_private'));
	const privateChoice = $derived(schema?.creation.is_private.allowed_values ?? []);
	/** Status → category, so rules on status_category see what the server will (the initial status). */
	let cats = $state<Map<string, { label: string; category: string }>>(new Map());
	$effect(() => {
		statusCategories()
			.then((m) => (cats = m))
			.catch(() => {});
	});
	const initialStatus = $derived(describe?.creation?.initial_status_id ?? null);
	const values = $derived(condValues(form, fields, { tracker: trackerId, status: initialStatus, status_category: initialStatus ? (cats.get(initialStatus)?.category ?? null) : null, is_private: isPrivate }));
	const hidden = $derived(hiddenKeys(describe?.layout, values));
	const entries = $derived(
		fields.map((f) => {
			const d = describe?.fields.find((x) => x.key === f.key);
			return { f, required: f.required || (d ? requiredNow({ required: false, required_if: d.required_if }, values) : false) };
		})
	);
	const missing = $derived(entries.filter((e) => e.required && !hidden.has(e.f.key) && (form[e.f.key] === undefined || form[e.f.key] === null || String(form[e.f.key]).trim() === '')).map((e) => e.f));

	function build() {
		const set: Record<string, unknown> = {};
		for (const f of fields) {
			// A field the layout hides right now is not sent (show_if, §6.14).
			if (hidden.has(f.key)) continue;
			const v = form[f.key];
			if (v === undefined || v === null || v === '') continue;
			set[f.key] = v;
		}
		if (privateChoice.length > 1 || privateChoice[0] !== schema?.creation.is_private.default) set.is_private = isPrivate;
		return doc(
			[{ kind: 'create_issue', op_id: newOperationId(), project_id: project!.id, tracker_id: trackerId, fields: wireFields(set, fields) }],
			[]
		);
	}

	async function create(retry = false) {
		if (!project || !schema || missing.length) return;
		busy = true;
		error = null;
		const sent = retry && last ? last : { body: build(), op: newOperationId() };
		last = sent;
		try {
			const r = await submit(sent.body, sent.op);
			last = null;
			const e = r.effects.find((x) => x.created) ?? r.effects[0];
			rememberCreated({ key: e.key, subject: String(form.subject ?? ''), after: r.search.consistent_after });
			toast(`已建立 ${e.key}`, 'ok');
			refreshCounts();
			created = true;
			if (newDraft) dropDraft(newDraft);
			goto(`/issue/${e.key}`);
		} catch (e) {
			error = e;
			if (!(e instanceof ApiError && e.retryable)) last = null;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>回報問題・oxledger</title></svelte:head>
<div class="page">
	<h1>回報問題</h1>
	{#if !session.projects.length}
		<div class="empty">你目前沒有可以看到的專案。</div>
	{:else}
		<div class="row" style="gap:12px;margin-bottom:12px">
			<label class="f" for="new-project"
				><span>專案</span>
				<select id="new-project" bind:value={projectKey} data-testid="new-project">
					{#each session.projects as p (p.id)}<option value={p.key}>{p.key}・{p.name}</option>{/each}
				</select>
			</label>
			<label class="f" for="new-tracker"
				><span>類型</span>
				<select id="new-tracker" bind:value={trackerId} data-testid="new-tracker">
					{#each trackers as t (t.id)}<option value={t.id}>{t.name}</option>{/each}
				</select>
			</label>
		</div>
		<ErrorBox error={loadError} />
		{#if schema && !schema.creation.allowed}
			<div class="banner w" data-testid="cannot-create">你在 {project?.key} 不能建立「{trackers.find((t) => t.id === trackerId)?.name}」類型的單。換一個專案或類型試試。</div>
		{:else if schema}
			<div class="card" data-testid="new-form">
				<FormSections fields={entries} layout={describe?.layout} bind:form {values} {assignees} {versions} disabled={busy || uncertain} md={{ context: project ? { project: project.id } : null, draft: newDraft, guard: !busy && !created }} />
				{#if privateChoice.length > 1}
					<label class="row" style="gap:6px;margin-top:6px"><input type="checkbox" bind:checked={isPrivate} disabled={busy || uncertain} data-testid="new-private" />私有（只有有權看私有單的成員看得到）</label>
				{:else if privateChoice[0] === true}
					<div class="hint">這個類型的單一律是私有的。</div>
				{/if}
				{#if missing.length}
					<div class="hint" style="margin-top:6px">還要填：{missing.map((f) => f.name ?? (f.key === 'subject' ? '標題' : f.key)).join('、')}</div>
				{/if}
				<ErrorBox {error} onaction={() => create(true)} />
				{#if uncertain}
					<div class="hint" data-testid="new-uncertain">還不確定剛才那筆有沒有建立：按「重試」會用同一筆內容確認，不會重複建立。要改內容請先重試確認結果。</div>
				{/if}
				<div class="row" style="gap:8px;margin-top:10px">
					<button class="btn pri" onclick={() => create()} disabled={busy || uncertain || !!missing.length || descOver} data-testid="new-submit">{busy ? '建立中…' : '建立'}</button>
					<a class="btn" href="/">取消</a>
				</div>
			</div>
		{:else if !loadError}
			<div class="sync"><span class="spin"></span>載入欄位…</div>
		{/if}
	{/if}
</div>

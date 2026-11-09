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
	import FieldInput from '$lib/components/FieldInput.svelte';
	import { listTrackers } from '$lib/trackers';

	const initialProject = (page.url.searchParams.get('project') ?? '').toUpperCase();
	let projectKey = $state(session.projects.some((p) => p.key === initialProject) ? initialProject : (session.projects[0]?.key ?? ''));
	let trackers = $state<Candidate[]>([]);
	let trackerId = $state('');
	let schema = $state<TrackerSchema | null>(null);
	let assignees = $state<Assignee[]>([]);
	let versions = $state<VersionView[]>([]);
	let form = $state<Record<string, unknown>>({ subject: '', description: page.url.searchParams.get('description') ?? '' });
	let isPrivate = $state(false);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	// Same document + Idempotency-Key when retrying a transient failure (§8.7).
	let last: { body: ReturnType<typeof doc>; op: string } | null = null;

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
		loadError = null;
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
	const missing = $derived(fields.filter((f) => f.required && (form[f.key] === undefined || form[f.key] === null || String(form[f.key]).trim() === '')));

	function build() {
		const set: Record<string, unknown> = {};
		for (const f of fields) {
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
				{#each fields as f (f.key)}
					<FieldInput field={f} bind:value={form[f.key]} {assignees} {versions} required={f.required} disabled={busy} />
				{/each}
				{#if privateChoice.length > 1}
					<label class="row" style="gap:6px;margin-top:6px"><input type="checkbox" bind:checked={isPrivate} disabled={busy} data-testid="new-private" />私有（只有有權看私有單的成員看得到）</label>
				{:else if privateChoice[0] === true}
					<div class="hint">這個類型的單一律是私有的。</div>
				{/if}
				{#if missing.length}
					<div class="hint" style="margin-top:6px">還要填：{missing.map((f) => f.name ?? (f.key === 'subject' ? '標題' : f.key)).join('、')}</div>
				{/if}
				<ErrorBox {error} onaction={() => create(true)} />
				<div class="row" style="gap:8px;margin-top:10px">
					<button class="btn pri" onclick={() => create()} disabled={busy || !!missing.length} data-testid="new-submit">{busy ? '建立中…' : '建立'}</button>
					<a class="btn" href="/">取消</a>
				</div>
			</div>
		{:else if !loadError}
			<div class="sync"><span class="spin"></span>載入欄位…</div>
		{/if}
	{/if}
</div>

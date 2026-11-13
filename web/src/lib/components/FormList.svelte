<script lang="ts">
	// The forms of the site or of a project (contract 05 §6.10 清單): one row
	// per tracker with its published versions and drafts. A tracker without a
	// form yet gets one on 「設計」 (POST /admin/forms is idempotent).
	import { goto } from '$app/navigation';
	import type { Candidate, FormSummary } from '$api/types';
	import { api, newOperationId } from '$lib/api';
	import { listTrackers } from '$lib/trackers';
	import ErrorBox from './ErrorBox.svelte';

	let { projectKey = null }: { projectKey?: string | null } = $props();

	let forms = $state<FormSummary[]>([]);
	let trackers = $state<Candidate[]>([]);
	let error = $state<unknown>(null);
	let loaded = $state(false);
	let busy = $state<string | null>(null);

	async function load() {
		error = null;
		try {
			[forms, trackers] = await Promise.all([api('GET /admin/forms', { query: projectKey ? { project: projectKey } : {} }), listTrackers()]);
			loaded = true;
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void projectKey;
		load();
	});
	const href = (f: FormSummary) => (projectKey ? `/project/${projectKey}/settings/forms/${f.tracker_id}` : `/admin/forms/${f.id}`);
	async function design(t: Candidate) {
		const f = forms.find((x) => x.tracker_id === t.id);
		if (f) return goto(href(f));
		if (projectKey) return goto(`/project/${projectKey}/settings/forms/${t.id}`);
		busy = t.id;
		error = null;
		try {
			const d = await api('POST /admin/forms', { operationId: newOperationId(), body: { tracker_id: t.id } });
			goto(`/admin/forms/${d.id}`);
		} catch (e) {
			error = e;
		} finally {
			busy = null;
		}
	}
</script>

<ErrorBox {error} onaction={load} />
{#if loaded}
	<table class="t" data-testid="form-list">
		<thead><tr><th>類型</th><th>資料規則</th><th>版面</th><th></th></tr></thead>
		<tbody>
			{#each trackers as t (t.id)}
				{@const f = forms.find((x) => x.tracker_id === t.id)}
				<tr data-testid="form-row">
					<td>{t.name}</td>
					<td>{f?.published_rules_version ? `v${f.published_rules_version}` : '—'}{f?.rules_draft ? '・有草稿' : ''}</td>
					<td>{f?.published_layout_version ? `v${f.published_layout_version}` : '—'}{f?.layout_draft ? '・有草稿' : ''}</td>
					<td style="text-align:right"><button class="btn sm" onclick={() => design(t)} disabled={busy === t.id} data-testid="form-design">設計</button></td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

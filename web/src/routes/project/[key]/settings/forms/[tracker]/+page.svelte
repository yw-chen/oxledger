<script lang="ts">
	// 專案層表單設計器（contract 05 §6.10, manage_project）: the form of
	// (project, tracker), created on first visit (POST /admin/forms is idempotent).
	import { page } from '$app/state';
	import { api, newOperationId } from '$lib/api';
	import { session } from '$lib/session.svelte';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import FormDesigner from '$lib/components/FormDesigner.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	const tracker = $derived(page.params.tracker ?? '');
	let formId = $state<string | null>(null);
	let error = $state<unknown>(null);
	async function ensure(k: string, t: string) {
		formId = null;
		error = null;
		try {
			formId = (await api('POST /admin/forms', { operationId: newOperationId(), body: { project: k, tracker_id: t } })).id;
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		ensure(key, tracker);
	});
</script>

<svelte:head><title>{key}・表單設計器・oxledger</title></svelte:head>
<ProjectFrame projectKey={key}>
	<div class="hint"><a href="/project/{key}/settings/forms">← 專案表單</a></div>
	<ErrorBox {error} onaction={() => ensure(key, tracker)} />
	{#if formId}{#key formId}<FormDesigner {formId} isAdmin={!!session.user?.is_admin} />{/key}{/if}
</ProjectFrame>

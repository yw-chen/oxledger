<script lang="ts">
	// A project view inside the project frame (§6.1, 02 §18.6).
	import { page } from '$app/state';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import ViewPage from '#lib/components/ViewPage.svelte';
	const key = $derived((page.params.key ?? '').toUpperCase());
	const id = $derived(decodeURIComponent(page.params.id ?? ''));
</script>

<svelte:head><title>{key}・視圖・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active={null}>
	{#key id}<ViewPage viewId={id} projectKey={key} inFrame />{/key}
</ProjectFrame>

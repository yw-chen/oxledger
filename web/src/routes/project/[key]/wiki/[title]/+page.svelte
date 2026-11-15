<script lang="ts">
	// Design 06 §6.3: one wiki page inside its project frame.
	import { page } from '$app/state';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import WikiPageView from '#lib/components/WikiPageView.svelte';
	import { titleFromParam } from '#lib/wiki.js';

	const key = $derived((page.params.key ?? '').toUpperCase());
	const title = $derived(page.params.title ?? '');
</script>

<svelte:head><title>{titleFromParam(title)}・{key} Wiki・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active="wiki">
	{#key `${key}/${title}`}<WikiPageView projectKey={key} {title} />{/key}
</ProjectFrame>

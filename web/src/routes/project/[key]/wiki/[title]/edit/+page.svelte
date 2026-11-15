<script lang="ts">
	// Design 06 §6.4: edit a wiki page, or create it when it does not exist.
	import { page } from '$app/state';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import WikiEditor from '#lib/components/WikiEditor.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
	const title = $derived(page.params.title ?? '');
</script>

<ProjectFrame projectKey={key} active="wiki">
	{#key `${key}/${title}`}<WikiEditor projectKey={key} {title} />{/key}
</ProjectFrame>

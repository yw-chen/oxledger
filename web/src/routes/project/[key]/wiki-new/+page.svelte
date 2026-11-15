<script lang="ts">
	// Design 06 §6.3 「新增頁面」: the editor with an empty title (`?parent=` preselects the parent).
	import { page } from '$app/state';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import WikiEditor from '#lib/components/WikiEditor.svelte';

	const key = $derived((page.params.key ?? '').toUpperCase());
</script>

<ProjectFrame projectKey={key} active="wiki">
	{#key key}<WikiEditor projectKey={key} />{/key}
</ProjectFrame>

<script lang="ts">
	// 專案表單（contract 05 §6.10, manage_project）.
	import { page } from '$app/state';
	import ProjectFrame from '$lib/components/ProjectFrame.svelte';
	import FormList from '$lib/components/FormList.svelte';
	const key = $derived((page.params.key ?? '').toUpperCase());
</script>

<svelte:head><title>{key}・表單・oxledger</title></svelte:head>
<ProjectFrame projectKey={key}>
	<div class="hint"><a href="/project/{key}/settings">← 專案設定</a></div>
	<h2>表單</h2>
	<p class="hint">專案層只能加規則（只會更嚴），版面會整份取代站台的版面。</p>
	{#key key}<FormList projectKey={key} />{/key}
</ProjectFrame>

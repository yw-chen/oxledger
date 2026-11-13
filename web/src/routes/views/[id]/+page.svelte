<script lang="ts">
	import { page } from '$app/state';
	import ViewPage from '$lib/components/ViewPage.svelte';
	const id = $derived(decodeURIComponent(page.params.id ?? ''));
</script>

<svelte:head><title>視圖・oxledger</title></svelte:head>
<div class="page">
	<a href="/views" class="hint">← 我的視圖</a>
	{#key id}<ViewPage viewId={id} />{/key}
</div>

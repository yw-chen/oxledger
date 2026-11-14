<script lang="ts">
	// 站台表單設計器（contract 05 §6.10）.
	import { page } from '$app/state';
	import { session } from '#lib/session.svelte.js';
	import FormDesigner from '#lib/components/FormDesigner.svelte';
	const admin = $derived(!!session.user?.is_admin);
</script>

<svelte:head><title>表單設計器・oxledger</title></svelte:head>
<div class="page wide">
	<div class="hint"><a href="/admin/forms">← 表單</a></div>
	{#if !admin}
		<div class="empty" data-testid="fd-denied">這一頁只有系統管理員可以使用。</div>
	{:else}
		{#key page.params.id}<FormDesigner formId={page.params.id ?? ''} isAdmin />{/key}
	{/if}
</div>

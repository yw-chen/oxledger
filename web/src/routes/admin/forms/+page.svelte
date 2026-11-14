<script lang="ts">
	// 表單設計器清單（contract 05 §6.10）: the site forms, one per tracker.
	import { session } from '#lib/session.svelte.js';
	import FormList from '#lib/components/FormList.svelte';
	const admin = $derived(!!session.user?.is_admin);
</script>

<svelte:head><title>表單・oxledger</title></svelte:head>
<div class="page">
	<h1>表單</h1>
	{#if !admin}
		<div class="empty" data-testid="fd-denied">這一頁只有系統管理員可以使用。專案層的表單在專案設定裡。</div>
	{:else}
		<p class="hint">每個類型一份表單：「資料規則」決定什麼情況下欄位必填、只能是哪些值或不能改；「版面」決定新建與修改時欄位怎麼分區。專案可以再加規則、覆寫版面。</p>
		<FormList />
	{/if}
</div>

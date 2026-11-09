<script lang="ts">
	// Router-level errors (an unknown address, or a page that failed to
	// load). API errors are shown in place by ErrorBox; this is only for
	// what never reached a page.
	import { page } from '$app/state';

	const notFound = $derived(page.status === 404);
</script>

<svelte:head><title>{notFound ? '找不到頁面' : '出了點問題'}・oxledger</title></svelte:head>
<div class="page" data-testid="error-page">
	<h1>{notFound ? '找不到這個頁面' : '頁面載入失敗'}</h1>
	<p class="hint">
		{#if notFound}網址 <span class="mono">{page.url.pathname}</span> 不存在。如果是單號，請確認拼字；單可能已經搬到別的專案，用上方搜尋找找看。
		{:else}沒有改到任何資料。重新整理再試一次；一直發生的話請回報給管理者（狀態 {page.status}）。{/if}
	</p>
	<div class="row" style="gap:8px">
		<a class="btn pri" href="/">回首頁</a>
		<a class="btn" href="/search">全部議題</a>
		{#if !notFound}<button class="btn" onclick={() => location.reload()}>重新整理</button>{/if}
	</div>
</div>

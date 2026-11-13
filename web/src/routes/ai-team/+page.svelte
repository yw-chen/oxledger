<script lang="ts">
	// AI 團隊 (contract 05 §6.18): the roles, their sponsor and safety state.
	// Anyone signed in may create a role (and becomes its sponsor).
	import AiTeamList from '$lib/components/AiTeamList.svelte';
</script>

<svelte:head><title>AI 團隊・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px">
		<h1 style="margin:0">AI 團隊</h1>
		<span style="flex:1"></span>
		<a class="btn pri" href="/ai-team/new" data-testid="new-role">＋ 建立角色</a>
	</div>
	<p class="hint">每個角色都是一個 agent，以 sponsor 的名義、在委派的專案裡工作；權限＝sponsor 目前的權限 ∩ 委派 ∩ 政策。讀不到私有內容；直接寫入可在活動頁回復。</p>
	<AiTeamList />
</div>

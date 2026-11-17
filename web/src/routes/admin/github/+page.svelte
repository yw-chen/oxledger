<script lang="ts">
	// GitHub 連動 (system administrators, §19.3): installations of the App and
	// which repositories project managers may link. Installing the App itself
	// happens on GitHub; this page only marks repositories linkable.
	import type { AdminGithub, AdminRepo } from '$api/types';
	import { api } from '#lib/api.js';
	import { fmtTime } from '#lib/format.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	let data = $state<AdminGithub | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		try {
			data = await api('GET /admin/github');
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		load();
	});
	async function setLinkable(r: AdminRepo, linkable: boolean) {
		busy = true;
		error = null;
		try {
			data = await api('PUT /admin/github', { body: { repos: [{ repo_id: r.repo_id, linkable }] } });
			toast(linkable ? `${r.full_name} 可以連動了` : `${r.full_name} 不再開放連動（既有連動保留）`, 'ok');
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	const STATE: Record<string, string> = { active: '正常', suspended: '已暫停', deleted: '已移除' };
	const REPO_STATE: Record<string, string> = { accessible: '可讀取', inaccessible: 'App 無法讀取' };
</script>

<svelte:head><title>GitHub 連動・oxledger</title></svelte:head>
<div class="page">
	<h1>GitHub 連動</h1>
	<div class="hint">安裝或移除 GitHub App 要到 GitHub 上操作。這裡決定哪些 repo 可以讓專案管理者連動：開放後，PR 標題、分支名與 commit 訊息會顯示給看得到相關議題的人。關閉只影響之後的新連動，既有連動照常接收。</div>
	<ErrorBox {error} onaction={load} />
	{#snippet repoRow(r: AdminRepo)}
		<tr data-testid="admin-repo">
			<td><b>{r.full_name}</b>{#if r.is_private}<span class="chip c-dim">私有</span>{/if}</td>
			<td class="hint">{REPO_STATE[r.state] ?? r.state}</td>
			<td>{r.links ? `${r.links} 個專案` : '—'}</td>
			<td><label class="row" style="gap:4px"><input type="checkbox" checked={r.linkable} disabled={busy} onchange={(e) => setLinkable(r, e.currentTarget.checked)} /> 可連動</label></td>
		</tr>
	{/snippet}
	{#if data}
		{#each data.installations as inst (inst.id)}
			<div class="card">
				<div class="row" style="gap:8px"><b>{inst.account_login}</b><span class="chip {inst.state === 'active' ? 'c-ok' : 'c-warn'}">{STATE[inst.state] ?? inst.state}</span></div>
				<div class="hint" data-testid="admin-push">push 事件：{inst.last_push_at ? `最後收到 ${fmtTime(inst.last_push_at)}` : '從未收到'}</div>
				{#if !inst.last_push_at && inst.state === 'active'}<div class="banner o" data-testid="admin-push-missing">GitHub App 還沒訂閱 push 事件，commit 連結與修正關鍵字不會運作。請到 GitHub 的 App 設定加訂「Push」事件（見升級說明）。</div>{/if}
				<table class="t">
					<thead><tr><th>repo</th><th>狀態</th><th>連動中</th><th></th></tr></thead>
					<tbody>{#each inst.repos as r (r.repo_id)}{@render repoRow(r)}{:else}<tr><td colspan="4" class="hint">這個安裝沒有授權任何 repo。</td></tr>{/each}</tbody>
				</table>
			</div>
		{:else}
			<div class="empty">還沒有安裝 GitHub App。</div>
		{/each}
		{#if data.detached_repos.length}
			<h2>App 已無法讀取的 repo</h2>
			<div class="hint">這些 repo 的資料停在最後一次同步；重新授權後會恢復。</div>
			<table class="t"><tbody>{#each data.detached_repos as r (r.repo_id)}{@render repoRow(r)}{/each}</tbody></table>
		{/if}
	{/if}
</div>

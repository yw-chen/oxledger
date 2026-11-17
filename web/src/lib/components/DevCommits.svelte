<script lang="ts">
	// Design 06 §6.8 開發卡「Commits」(R-C6): sha (to GitHub), first line,
	// author (not verified by GitHub; hover: who pushed), time, 「已在
	// 〈預設分支〉」. Collected from GitHub's push notifications, so the list
	// may be incomplete. Managers hide a commit in this project (R-C7).
	import type { DevCommit } from '$api/types';
	import { api } from '#lib/api.js';
	import { fmtTime, safeHref } from '#lib/format.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from './ErrorBox.svelte';

	let {
		commits,
		truncated = false,
		canManage = false,
		projectId,
		onchanged
	}: {
		commits: DevCommit[];
		truncated?: boolean;
		canManage?: boolean;
		projectId: string;
		/** After hiding or showing a commit: the parent reloads. */
		onchanged?: () => void;
	} = $props();

	let busy = $state(false);
	let error = $state<unknown>(null);

	async function hide(c: DevCommit, hidden: boolean) {
		busy = true;
		error = null;
		try {
			const path = { id: projectId, repo_id: c.repo_id, sha: c.sha };
			if (hidden) await api('POST /projects/{id}/dev/commits/{repo_id}/{sha}/hide', { path });
			else await api('DELETE /projects/{id}/dev/commits/{repo_id}/{sha}/hide', { path });
			toast(hidden ? '已在這個專案隱藏這個 commit' : '已取消隱藏', 'ok');
			onchanged?.();
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
</script>

{#if commits.length}
	<div class="commits" data-testid="dev-commits">
		<div class="head"><b>Commits</b><span class="sp"></span><span class="hint" title="依 GitHub 的 push 通知收集，可能不完整">依 GitHub 通知收集</span></div>
		{#each commits as c (c.repo_id + c.sha)}
			<div class="dyn" class:off={c.hidden} data-testid="dev-commit">
				<div class="line">
					<a class="sha" href={safeHref(c.url)} target="_blank" rel="noopener noreferrer" title="{c.repo}@{c.sha}">{c.sha.slice(0, 7)}</a>
					<span class="title">{c.title}</span>
				</div>
				<div class="hint meta">
					{#if c.author_login}<span title={c.sender_login ? `由 ${c.sender_login} 推送` : undefined} data-testid="dev-commit-author">{c.author_login}（GitHub 未驗證）</span>
					{:else if c.sender_login}<span>由 {c.sender_login} 推送</span>{/if}
					<span>{fmtTime(c.at)}</span>
					{#if c.on_default_branch}<span class="chip c-ok" data-testid="dev-commit-default">已在 {c.default_branch ?? '預設分支'}</span>{/if}
					{#if c.hidden}<span class="chip c-dim">已隱藏</span>{/if}
					{#if canManage}<span class="sp"></span><button type="button" class="btn sm ghost" disabled={busy} onclick={() => hide(c, !c.hidden)} aria-label="{c.hidden ? '取消隱藏' : '隱藏'} {c.sha.slice(0, 7)}" data-testid="dev-commit-hide">{c.hidden ? '取消隱藏' : '隱藏'}</button>{/if}
				</div>
			</div>
		{/each}
		{#if truncated}<div class="hint">還有更多 commit（只顯示最新 20 筆）。</div>{/if}
		<ErrorBox {error} />
	</div>
{/if}

<style>
	.commits { display: flex; flex-direction: column; gap: 4px; margin-top: 8px; }
	.head, .line, .meta { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
	.sp { flex: 1; }
	.sha { font-family: var(--mono, monospace); font-size: 12.5px; }
	.title { font-size: 12.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; min-width: 0; }
	.off { opacity: 0.6; }
</style>

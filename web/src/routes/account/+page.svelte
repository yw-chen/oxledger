<script lang="ts">
	// 帳號 (§3.3, §15.1): who is signed in, the browser sessions of this
	// account (sign another one out), and this browser's layout conveniences.
	// Agent tokens live under 「我的 agent 委派」, not here.
	import { goto } from '$app/navigation';
	import type { SessionItem } from '$api/types';
	import { api } from '#lib/api.js';
	import { fmtShort } from '#lib/format.js';
	import { prefs, savePrefs } from '#lib/prefs.svelte.js';
	import { session, signOut } from '#lib/session.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	let items = $state<SessionItem[] | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);

	async function load() {
		error = null;
		try {
			items = await api('GET /auth/sessions');
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		load();
	});
	async function revoke(s: SessionItem) {
		if (s.current) {
			if (!confirm('登出這個瀏覽器？')) return;
			await signOut();
			goto('/login');
			return;
		}
		busy = true;
		error = null;
		try {
			await api('DELETE /auth/sessions/{id}', { path: { id: s.id } });
			toast('已登出那個工作階段', 'ok');
			await load();
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	async function revokeOthers() {
		if (!items || !confirm('登出其他所有瀏覽器？這個瀏覽器保持登入。')) return;
		busy = true;
		error = null;
		try {
			for (const s of items.filter((x) => !x.current)) await api('DELETE /auth/sessions/{id}', { path: { id: s.id } });
			toast('已登出其他工作階段', 'ok');
		} catch (e) {
			error = e;
		} finally {
			busy = false;
			await load();
		}
	}
	function resetLayout() {
		prefs.navMini = false;
		prefs.collabHidden = false;
		savePrefs();
		toast('欄位版面已恢復預設', 'ok');
	}
	const others = $derived(items?.filter((s) => !s.current).length ?? 0);
</script>

<svelte:head><title>帳號・oxledger</title></svelte:head>
<div class="page">
	<h1>帳號</h1>
	{#if session.user}
		<div class="card">
			<div><b>{session.user.name}</b> <span class="mono dim">{session.user.login}</span>{#if session.user.is_admin}<span class="chip c-dim" style="margin-left:6px">系統管理者</span>{/if}</div>
			<div class="hint">這個工作階段到 {fmtShort(session.user.expires_at)} 為止。</div>
		</div>
	{/if}

	<h2>登入中的瀏覽器</h2>
	<ErrorBox {error} onaction={load} />
	{#if items}
		<table class="t" data-testid="sessions">
			<thead><tr><th>登入時間</th><th>最近使用</th><th>到期</th><th></th></tr></thead>
			<tbody>
				{#each items as s (s.id)}
					<tr data-testid="session-row">
						<td>{fmtShort(s.created_at)}{#if s.current}<span class="chip c-ok" style="margin-left:6px">這個瀏覽器</span>{/if}</td>
						<td>{fmtShort(s.last_seen_at)}</td>
						<td>{fmtShort(s.expires_at)}</td>
						<td><button class="btn sm ghost" onclick={() => revoke(s)} disabled={busy}>登出</button></td>
					</tr>
				{/each}
			</tbody>
		</table>
		{#if others}<button class="btn" style="margin-top:6px" onclick={revokeOthers} disabled={busy}>登出其他 {others} 個</button>{/if}
		<div class="hint">agent 用的 token 不在這裡，請到「我的 agent 委派」管理。</div>
	{/if}

	<h2 style="margin-top:22px">偏好</h2>
	<div class="card">
		<div>介面角色、首頁、外觀、左欄捷徑與通知，存在你的帳號，換電腦也會跟著走。</div>
		<div class="row" style="gap:6px;margin-top:6px"><a class="btn sm" href="/me/preferences" data-testid="link-preferences">偏好設定</a><a class="btn sm" href="/me/notifications">通知偏好</a></div>
	</div>

	<h2 style="margin-top:22px">這個瀏覽器的版面</h2>
	<div class="card">
		<div class="hint">左欄收合、右欄開關只存在這個瀏覽器。</div>
		<button class="btn sm" style="margin-top:6px" onclick={resetLayout}>欄位恢復預設</button>
	</div>
</div>

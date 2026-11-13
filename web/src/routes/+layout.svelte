<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto, afterNavigate } from '$app/navigation';
	import { session, loadSession, signOut } from '$lib/session.svelte';
	import { loadPrefs, prefs, toggleTheme } from '$lib/prefs.svelte';
	import { refreshCounts } from '$lib/counts.svelte';
	import { toasts } from '$lib/toast.svelte';
	import LeftNav from '$lib/components/LeftNav.svelte';
	import Collab from '$lib/components/Collab.svelte';
	import Palette from '$lib/components/Palette.svelte';
	import StepUpDialog from '$lib/components/StepUpDialog.svelte';
	import LoginForm from '$lib/components/LoginForm.svelte';
	import ErrorBox from '$lib/components/ErrorBox.svelte';

	let { children } = $props();
	let loadError = $state<unknown>(null);
	let palette = $state(false);
	const onLogin = $derived(page.url.pathname === '/login');

	onMount(async () => {
		try {
			await loadSession();
		} catch (e) {
			loadError = e;
		}
		loadPrefs(session.user?.user_id ?? null);
		if (session.user) refreshCounts();
	});

	$effect(() => {
		if (session.ready && !session.user && !session.expired && !onLogin && !loadError) {
			const next = page.url.pathname + page.url.search;
			goto(`/login?next=${encodeURIComponent(next)}`, { replaceState: true });
		}
	});

	afterNavigate(() => {
		if (session.user) refreshCounts();
	});

	function keys(e: KeyboardEvent) {
		if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
			e.preventDefault();
			if (session.user) palette = true;
		}
	}
	async function logout() {
		await signOut();
		goto('/login');
	}
	function relogged() {
		// The page under the dialog keeps its state (§15.1: nothing typed is lost).
		loadPrefs(session.user?.user_id ?? null);
		refreshCounts();
	}
</script>

<svelte:window onkeydown={keys} />

<div id="app">
	{#if !session.ready}
		<div class="login"><div class="sync"><span class="spin"></span>載入中</div></div>
	{:else if loadError && !session.user}
		<div class="login"><ErrorBox error={loadError} onaction={() => location.reload()} /></div>
	{:else if onLogin || (!session.user && !session.expired)}
		{@render children()}
	{:else}
		<div class="app">
			<header class="top">
				<a class="logo" href="/" style="color:inherit;text-decoration:none"><i></i>oxledger</a>
				<button class="kbar" onclick={() => (palette = true)} data-testid="kbar">
					<span class="kbd">Ctrl K</span><span class="t">搜尋紀錄、開單號，或輸入 /new</span>
				</button>
				<div class="me">
					<button class="btn sm ghost" onclick={toggleTheme} title="切換深色／淺色" data-testid="theme">{prefs.theme === 'dark' ? '☾ 深色' : '☀ 淺色'}</button>
					{#if session.user}
						<a href="/account" class="nm" style="color:inherit">{session.user.name}</a>
						<button class="btn sm" onclick={logout}>登出</button>
					{/if}
				</div>
			</header>
			<div class="tri">
				<LeftNav />
				<main class="center" id="main">{@render children()}</main>
				<Collab />
			</div>
		</div>
		<Palette bind:open={palette} />
		<StepUpDialog />
	{/if}
	{#if session.expired}
		<div class="modal-bg">
			<div class="modal" role="dialog" aria-modal="true" aria-label="重新登入" data-testid="relogin">
				<h3>登入已過期</h3>
				<p class="muted" style="font-size:13px">重新登入後留在這一頁，剛才填的內容還在。</p>
				<LoginForm done={relogged} />
			</div>
		</div>
	{/if}
	<div class="toasts" aria-live="polite">
		{#each toasts as t (t.id)}<div class="toast {t.kind}">{t.text}</div>{/each}
	</div>
</div>

<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto, afterNavigate } from '$app/navigation';
	import { session, loadSession, signOut } from '#lib/session.svelte.js';
	import { loadPrefs, prefs, THEME_LABEL } from '#lib/prefs.svelte.js';
	import { loadMe, me, setPrefs } from '#lib/me.svelte.js';
	import { untrack } from 'svelte';
	import Bell from '#lib/components/Bell.svelte';
	import Onboarding from '#lib/components/Onboarding.svelte';
	import { refreshCounts } from '#lib/counts.svelte.js';
	import { toasts } from '#lib/toast.svelte.js';
	import LeftNav from '#lib/components/LeftNav.svelte';
	import Collab from '#lib/components/Collab.svelte';
	import { closeDrawer, collabUi, openDrawer, watchNarrow } from '#lib/collab.svelte.js';
	import Palette from '#lib/components/Palette.svelte';
	import StepUpDialog from '#lib/components/StepUpDialog.svelte';
	import LoginForm from '#lib/components/LoginForm.svelte';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

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
	// Personal settings follow whoever is signed in (also after the login page or a re-login as someone else).
	$effect(() => {
		const id = session.user?.user_id ?? null;
		untrack(() => {
			if (id) loadMe();
			else me.prefs = null;
		});
	});

	$effect(() => {
		if (session.ready && !session.user && !session.expired && !onLogin && !loadError) {
			const next = page.url.pathname + page.url.search;
			goto(`/login?next=${encodeURIComponent(next)}`, { replace: true });
		}
	});

	afterNavigate(() => {
		if (session.user) refreshCounts();
		// A link followed from the drawer shows its page, not the drawer over it.
		closeDrawer(false);
	});
	// §6.19: below 1280 px the right column is a drawer.
	onMount(() => watchNarrow());

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
	/** The top bar button flips dark and light; the other themes are in 偏好. */
	function flipTheme() {
		setPrefs({ theme: prefs.theme === 'light' ? 'dark' : 'light' });
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
					{#if collabUi.narrow && session.user}
						<button class="btn sm" class:pri={collabUi.drawer} onclick={() => (collabUi.drawer ? closeDrawer() : openDrawer())} aria-expanded={collabUi.drawer} title={collabUi.ai?.enabled ? 'AI 助理與 agent 動態' : 'agent 動態'} data-testid="top-ai">{collabUi.ai?.enabled ? 'AI' : '動態'}</button>
					{/if}
					<button class="btn sm ghost" onclick={flipTheme} title="切換深色／淺色（其他選項在偏好設定）" data-testid="theme">{prefs.theme === 'light' ? '☀' : '☾'} {THEME_LABEL[prefs.theme]}</button>
					{#if session.user}<Bell />{/if}
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
		{#if page.url.pathname === '/'}<Onboarding />{/if}
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

<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import type { LoginContextData } from '$api/types';
	import { api } from '#lib/api.js';
	import { googleErrorText, googleStartUrl } from '#lib/google.js';
	import LoginForm from '#lib/components/LoginForm.svelte';
	import { loadPrefs } from '#lib/prefs.svelte.js';
	import { session } from '#lib/session.svelte.js';

	// Read once and untracked: after goto() leaves /login the URL has no
	// `next` any more, and a reactive read sent the person on to "/".
	const target = untrack(() => {
		const n = page.url.searchParams.get('next');
		if (!n || !n.startsWith('/')) return '/';
		// Resolve it the way the browser will (tabs and newlines dropped,
		// backslashes read as slashes): this origin only, never an open redirect.
		try {
			const u = new URL(n, page.url.origin);
			// `/.//evil.example` normalises to the path `//evil.example`, which
			// a browser reads as another host: never a target.
			if (u.origin !== page.url.origin || u.pathname.startsWith('//')) return '/';
			return u.pathname + u.search + u.hash;
		} catch {
			return '/';
		}
	});
	let leaving = false;
	function leave() {
		if (leaving) return;
		leaving = true;
		// Kit 3 rejects goto() for a path with no route (kit 2 showed the 404
		// page); only then load it directly (as an absolute URL on this
		// origin), so `next=/no/such/page` still ends on "找不到這個頁面"
		// instead of staying on /login. Other rejections (a newer navigation,
		// e.g. the person pressed Back) are left alone.
		goto(target, { replace: true }).catch((e: unknown) => {
			if (String((e as Error)?.message).includes('navigation_route_missing')) {
				location.replace(new URL(target, location.origin).href);
			}
		});
	}
	function done() {
		loadPrefs(session.user?.user_id ?? null);
		leave();
	}
	// Why a Google round trip came back here without a session (read once).
	const googleError = untrack(() => googleErrorText(page.url.searchParams.get('google_error')));
	$effect(() => {
		// With an error to read, a person who is still signed in (a step-up
		// that did not work) stays on this page until they leave it.
		if (session.user && !googleError) leave();
	});

	// Whether Google sign-in is on and whether the site is still new. Only
	// extras hang on it: when it fails the password form works as before.
	let ctx = $state<LoginContextData | null>(null);
	onMount(async () => {
		try {
			ctx = await api('GET /auth/login-context');
		} catch {
			ctx = null;
		}
	});
</script>

<svelte:head><title>登入・oxledger</title></svelte:head>
<div class="login">
	<div class="logo" style="font-size:20px;margin-bottom:16px"><i></i>oxledger</div>
	{#if ctx?.org_name}<p class="muted" style="margin:-8px 0 14px" data-testid="login-org">{ctx.org_name}</p>{/if}
	{#if googleError}<p class="err" role="alert" data-testid="google-error">{googleError}</p>{/if}
	{#if session.user}
		<p><a class="btn" href="/">回首頁</a></p>
	{:else}
		{#if ctx?.needs_setup}
			<p class="hint" data-testid="login-needs-setup">這個站還沒有任何帳號。用 Google 登入來建立機構，你會成為第一位系統管理員。</p>
		{/if}
		{#if ctx?.google}
			<p><a class="btn" style="display:block;text-align:center" href={googleStartUrl(target)} data-sveltekit-reload rel="external" data-testid="google-login">{ctx.needs_setup ? '用 Google 登入並建立機構' : '使用 Google 登入'}</a></p>
			{#if !ctx.needs_setup}<p class="hint" style="text-align:center;margin:10px 0">或用帳號密碼</p>{/if}
		{/if}
		{#if !ctx?.needs_setup}<LoginForm {done} />{/if}
	{/if}
</div>

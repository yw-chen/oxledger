<script lang="ts">
	import { untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
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
	$effect(() => {
		if (session.user) leave();
	});
</script>

<svelte:head><title>登入・oxledger</title></svelte:head>
<div class="login">
	<div class="logo" style="font-size:20px;margin-bottom:16px"><i></i>oxledger</div>
	<LoginForm {done} />
</div>

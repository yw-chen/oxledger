<script lang="ts">
	import { untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import LoginForm from '$lib/components/LoginForm.svelte';
	import { loadPrefs } from '$lib/prefs.svelte';
	import { session } from '$lib/session.svelte';

	// Read once and untracked: after goto() leaves /login the URL has no
	// `next` any more, and a reactive read sent the person on to "/".
	const target = untrack(() => {
		const n = page.url.searchParams.get('next');
		if (!n || !n.startsWith('/')) return '/';
		// Resolve it the way the browser will (tabs and newlines dropped,
		// backslashes read as slashes): this origin only, never an open redirect.
		try {
			const u = new URL(n, page.url.origin);
			return u.origin === page.url.origin ? u.pathname + u.search + u.hash : '/';
		} catch {
			return '/';
		}
	});
	let leaving = false;
	function leave() {
		if (leaving) return;
		leaving = true;
		goto(target, { replaceState: true });
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

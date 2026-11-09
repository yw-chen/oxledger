<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import LoginForm from '$lib/components/LoginForm.svelte';
	import { loadPrefs } from '$lib/prefs.svelte';
	import { session } from '$lib/session.svelte';

	function next() {
		const n = page.url.searchParams.get('next');
		// Only same-site paths (no open redirect).
		return n && n.startsWith('/') && !n.startsWith('//') && !n.startsWith('/\\') ? n : '/';
	}
	function done() {
		loadPrefs(session.user?.user_id ?? null);
		goto(next(), { replaceState: true });
	}
	$effect(() => {
		if (session.user) goto(next(), { replaceState: true });
	});
</script>

<svelte:head><title>登入・oxledger</title></svelte:head>
<div class="login">
	<div class="logo" style="font-size:20px;margin-bottom:16px"><i></i>oxledger</div>
	<LoginForm {done} />
</div>

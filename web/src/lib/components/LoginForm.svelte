<script lang="ts">
	// Password + one submit (§15.1); works with password managers.
	import { signIn } from '#lib/session.svelte.js';
	import ErrorBox from './ErrorBox.svelte';
	let { done }: { done: () => void } = $props();
	let login = $state('');
	let password = $state('');
	let busy = $state(false);
	let error = $state<unknown>(null);
	async function submit(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		error = null;
		try {
			await signIn(login.trim(), password);
			password = '';
			done();
		} catch (err) {
			error = err;
		} finally {
			busy = false;
		}
	}
</script>

<form onsubmit={submit} data-testid="login-form">
	<ErrorBox {error} />
	<label class="f"><span>帳號</span><input type="text" name="username" autocomplete="username" bind:value={login} required /></label>
	<label class="f"><span>密碼</span><input type="password" name="password" autocomplete="current-password" bind:value={password} required /></label>
	<button class="btn pri" type="submit" disabled={busy}>{busy ? '登入中…' : '登入'}</button>
	<p class="hint">忘記密碼請聯絡管理員。</p>
</form>

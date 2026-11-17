<script lang="ts">
	// The step-up dialog (stepup.svelte.ts); one for the whole app. A person
	// with a password types it; one who signs in with Google goes to Google
	// and back (a page navigation: what is on the page is not kept).
	import { page } from '$app/state';
	import { googleStartUrl } from '#lib/google.js';
	import { stepUp } from '#lib/stepup.svelte.js';
	import { reauthenticate, session } from '#lib/session.svelte.js';
	import ErrorBox from './ErrorBox.svelte';

	const hasPassword = $derived(session.user?.has_password ?? true);
	const google = $derived(session.user?.google ?? false);
	const back = $derived(page.url.pathname + page.url.search);

	let password = $state('');
	let busy = $state(false);
	let error = $state<unknown>(null);

	async function confirm(e: Event) {
		e.preventDefault();
		busy = true;
		error = null;
		try {
			await reauthenticate(password);
			password = '';
			stepUp.resolve?.(true);
		} catch (err) {
			error = err;
		} finally {
			busy = false;
		}
	}
	function cancel() {
		password = '';
		error = null;
		stepUp.resolve?.(false);
	}
</script>

{#if stepUp.why}
	<div class="modal-bg">
		<div class="modal" role="dialog" aria-modal="true" aria-label={hasPassword ? '確認密碼' : '重新驗證身分'} data-testid="step-up">
			<h3>{hasPassword ? '請再輸入一次密碼' : '請重新驗證身分'}</h3>
			<p class="muted" style="font-size:13px">{stepUp.why}（15 分鐘內不會再問）</p>
			{#if hasPassword}
				<form class="inline-form" onsubmit={confirm}>
					<!-- svelte-ignore a11y_autofocus -->
					<input type="password" bind:value={password} autocomplete="current-password" aria-label="密碼" data-testid="step-up-password" autofocus />
					<button class="btn pri" disabled={busy || !password} data-testid="step-up-confirm">確認</button>
					<button type="button" class="btn" onclick={cancel}>取消</button>
				</form>
			{/if}
			{#if google}
				<p class="muted" style="font-size:13px;margin:10px 0 6px">{hasPassword ? '或用 Google 帳號驗證。' : '你的帳號用 Google 登入，沒有密碼。'}會離開這一頁，回來後請再做一次剛才的動作；還沒儲存的內容不會保留。</p>
				<div class="inline-form">
					<a class="btn" class:pri={!hasPassword} href={googleStartUrl(back, true)} data-sveltekit-reload rel="external" data-testid="step-up-google">用 Google 驗證</a>
					{#if !hasPassword}<button type="button" class="btn" onclick={cancel}>取消</button>{/if}
				</div>
			{:else if !hasPassword}
				<p class="err">這個帳號沒有密碼，Google 登入也沒有開啟，無法驗證身分。請聯絡管理員。</p>
				<button type="button" class="btn" onclick={cancel}>關閉</button>
			{/if}
			<ErrorBox {error} />
		</div>
	</div>
{/if}

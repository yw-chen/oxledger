<script lang="ts">
	// The step-up password dialog (stepup.svelte.ts); one for the whole app.
	import { stepUp } from '$lib/stepup.svelte';
	import { reauthenticate } from '$lib/session.svelte';
	import ErrorBox from './ErrorBox.svelte';

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
		<div class="modal" role="dialog" aria-modal="true" aria-label="確認密碼" data-testid="step-up">
			<h3>請再輸入一次密碼</h3>
			<p class="muted" style="font-size:13px">{stepUp.why}（15 分鐘內不會再問）</p>
			<form class="inline-form" onsubmit={confirm}>
				<!-- svelte-ignore a11y_autofocus -->
				<input type="password" bind:value={password} autocomplete="current-password" aria-label="密碼" data-testid="step-up-password" autofocus />
				<button class="btn pri" disabled={busy || !password} data-testid="step-up-confirm">確認</button>
				<button type="button" class="btn" onclick={cancel}>取消</button>
			</form>
			<ErrorBox {error} />
		</div>
	</div>
{/if}

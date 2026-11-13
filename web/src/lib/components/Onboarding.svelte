<script lang="ts">
	// First sign-in (contract 05 §6.12): 「你主要做什麼？」 sets the interface
	// roles and replaces the home with their template; 「略過」 uses the generic one.
	import { api, newOperationId } from '$lib/api';
	import { adoptPrefs, me, ROLE_LABEL, ROLES, setPrefs } from '$lib/me.svelte';
	import ErrorBox from './ErrorBox.svelte';

	let picked = $state<string[]>([]);
	let busy = $state(false);
	let error = $state<unknown>(null);

	async function done(roles: string[]) {
		busy = true;
		error = null;
		try {
			// The template first: if it fails the dialog stays and says why (onboarded closes it).
			const r = await api('POST /me/home/apply-template', { operationId: newOperationId(), body: { roles, mode: 'replace' } });
			adoptPrefs(r.preferences);
			me.homeRev++;
			await setPrefs({ interface_roles: roles, onboarded: true });
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
</script>

{#if me.prefs && !me.prefs.onboarded}
	<div class="modal-bg">
		<div class="modal" role="dialog" aria-modal="true" aria-labelledby="ob-t" data-testid="onboarding">
			<h3 id="ob-t">你主要做什麼？（可複選）</h3>
			<div class="row" style="gap:12px;margin:10px 0">
				{#each ROLES as r (r)}
					<label class="row" style="gap:4px"><input type="checkbox" value={r} bind:group={picked} data-testid="ob-role-{r}" />{ROLE_LABEL[r]}</label>
				{/each}
			</div>
			<p class="hint">只影響預設的首頁和捷徑，不影響權限，之後可以改。</p>
			<ErrorBox {error} />
			<div class="row" style="justify-content:flex-end;gap:6px;margin-top:10px">
				<button class="btn" onclick={() => done([])} disabled={busy} data-testid="ob-skip">略過</button>
				<button class="btn pri" onclick={() => done(picked)} disabled={busy || !picked.length} data-testid="ob-submit">開始使用</button>
			</div>
		</div>
	</div>
{/if}

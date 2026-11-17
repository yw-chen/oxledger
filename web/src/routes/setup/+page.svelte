<script lang="ts">
	// Opening a new site (server: google::setup). Reached from Google's
	// redirect with the __Host-ox_setup cookie: the first Google account to
	// sign in names the organization and becomes its administrator.
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import type { SetupContextData } from '$api/types';
	import { api, ApiError, setCsrf } from '#lib/api.js';
	import { defaultTimeZone, setupProblems, timeZones } from '#lib/google.js';
	import { loadPrefs } from '#lib/prefs.svelte.js';
	import { loadSession, session } from '#lib/session.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	let ctx = $state<SetupContextData | null>(null);
	let gone = $state(false);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let busy = $state(false);
	let tried = $state(false);
	const zones = timeZones();
	let form = $state({ org_name: '', login: '', name: '', timezone: '', load_defaults: true });
	const problems = $derived(setupProblems(form));

	onMount(async () => {
		try {
			const c = await api('GET /setup/context');
			ctx = c;
			form.login = c.login;
			form.name = c.name;
			form.timezone = defaultTimeZone(zones, c.timezone);
		} catch (e) {
			if (e instanceof ApiError && e.code === 'not_found') gone = true;
			else error = e;
		} finally {
			loading = false;
		}
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		tried = true;
		if (Object.keys(problems).length) return;
		busy = true;
		error = null;
		try {
			const r = await api('POST /setup', {
				body: {
					org_name: form.org_name.trim(),
					login: form.login.trim(),
					name: form.name.trim(),
					timezone: form.timezone.trim(),
					load_defaults: form.load_defaults
				}
			});
			setCsrf(r.csrf_token);
			await loadSession();
			loadPrefs(session.user?.user_id ?? null);
			await goto('/', { replace: true });
		} catch (err) {
			if (err instanceof ApiError && err.code === 'not_found') gone = true;
			else error = err;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>建立機構・oxledger</title></svelte:head>
<div class="login" data-testid="setup">
	<div class="logo" style="font-size:20px;margin-bottom:16px"><i></i>oxledger</div>
	{#if loading}
		<div class="sync"><span class="spin"></span>載入中</div>
	{:else if gone}
		<h3>沒有可以建立的機構</h3>
		<p class="hint" data-testid="setup-gone">這個站已經設定完成，或這次建立的時限（15 分鐘）已過。</p>
		<p><a class="btn" href={session.user ? '/' : '/login'}>{session.user ? '回首頁' : '回登入頁'}</a></p>
	{:else if ctx}
		<h3>建立機構</h3>
		<p class="hint">這個站還沒有任何帳號。你（{ctx.email}）會成為第一位系統管理員，之後用同一個 Google 帳號登入。</p>
		<form onsubmit={submit} data-testid="setup-form" novalidate>
			<ErrorBox {error} />
			<label class="f"><span>機構名稱 <em>*</em></span>
				<!-- svelte-ignore a11y_autofocus -->
				<input type="text" bind:value={form.org_name} maxlength="100" autocomplete="organization" aria-invalid={tried && !!problems.org_name} autofocus data-testid="setup-org" />
				{#if tried && problems.org_name}<span class="hint" style="color:var(--bad)">{problems.org_name}</span>{/if}
			</label>
			<label class="f"><span>時區 <em>*</em></span>
				<input type="text" list="setup-zones" bind:value={form.timezone} autocomplete="off" aria-invalid={tried && !!problems.timezone} data-testid="setup-timezone" />
				<datalist id="setup-zones">{#each zones as z (z)}<option value={z}></option>{/each}</datalist>
				{#if tried && problems.timezone}<span class="hint" style="color:var(--bad)">{problems.timezone}</span>{/if}
				<span class="hint">IANA 名稱，例如 Asia/Taipei；到期日、工時的「今天」依這個時區。</span>
			</label>
			<label class="f"><span>你的帳號 <em>*</em></span>
				<input type="text" bind:value={form.login} maxlength="64" autocomplete="username" aria-invalid={tried && !!problems.login} data-testid="setup-login" />
				{#if tried && problems.login}<span class="hint" style="color:var(--bad)">{problems.login}</span>{/if}
			</label>
			<label class="f"><span>你的姓名 <em>*</em></span>
				<input type="text" bind:value={form.name} maxlength="100" autocomplete="name" aria-invalid={tried && !!problems.name} data-testid="setup-name" />
				{#if tried && problems.name}<span class="hint" style="color:var(--bad)">{problems.name}</span>{/if}
			</label>
			<label class="f" style="display:flex;gap:8px;align-items:flex-start">
				<input type="checkbox" bind:checked={form.load_defaults} data-testid="setup-defaults" style="margin-top:3px" />
				<span style="margin:0;color:inherit">載入預設設定（角色：管理者、開發者、回報者；追蹤類型：缺陷、功能、支援；狀態與流程）</span>
			</label>
			<button class="btn pri" type="submit" disabled={busy} data-testid="setup-submit">{busy ? '建立中…' : '建立機構'}</button>
		</form>
	{:else}
		<ErrorBox {error} onaction={() => location.reload()} />
	{/if}
</div>

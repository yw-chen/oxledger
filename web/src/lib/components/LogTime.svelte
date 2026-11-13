<script lang="ts">
	// 記工時 (contract 05 §6.6): date, hours (1.5, 1:30, 1h30m), activity,
	// comments, and "替誰記" for people with `log_time_for_other_users`. The
	// rule errors (daily limit, future date, closed issue) show right here.
	// `embedded`: the fields only, for 「同時記工時」 in the issue edit form.
	import { untrack } from 'svelte';
	import type { Named } from '$api/types';
	import { session } from '$lib/session.svelte';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { submit } from '$lib/commands';
	import { logTimeDoc, parseHours, showHours, todayLocal, type LogInput } from '$lib/hours';
	import ErrorBox from './ErrorBox.svelte';

	let {
		projectId,
		issueKey = null,
		embedded = false,
		value = $bindable(null),
		ondone,
		oncancel
	}: {
		projectId: string;
		issueKey?: string | null;
		embedded?: boolean;
		/** embedded: the entry to send with the issue document (null = nothing valid). */
		value?: LogInput | null;
		ondone?: (text: string) => void;
		oncancel?: () => void;
	} = $props();

	let spentOn = $state(todayLocal());
	let hoursText = $state('');
	let activity = $state('');
	let comments = $state('');
	let user = $state('');
	let activities = $state<Named[]>([]);
	let users = $state<Named[]>([]);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	let opId = newOperationId();

	$effect(() => {
		const p = projectId;
		loadError = null;
		Promise.all([api('GET /time-activities', { query: { project: p } }), api('GET /projects/{id}/time-users', { path: { id: p } }).catch(() => [] as Named[])])
			.then(([a, u]) => {
				activities = a.items;
				untrack(() => {
					if (!activity || !a.items.some((x) => x.id === activity)) activity = a.default_id ?? a.items[0]?.id ?? '';
				});
				users = u;
			})
			.catch((e) => (loadError = e));
	});

	const hours = $derived(parseHours(hoursText));
	const entry = $derived<LogInput | null>(
		hours === null || !spentOn ? null : { issueKey, projectId: issueKey ? null : projectId, spent_on: spentOn, hours, activity_id: activity || null, comments, user_id: user || null }
	);
	$effect(() => {
		if (embedded) value = entry;
	});

	async function send() {
		if (!entry) return;
		busy = true;
		error = null;
		try {
			await submit(logTimeDoc(entry), opId);
			opId = newOperationId();
			const who = user ? users.find((u) => u.id === user)?.name : null;
			ondone?.(`已記 ${showHours(entry.hours)} 小時${who ? `（替 ${who}）` : ''}`);
		} catch (e) {
			error = e;
			if (!(e instanceof ApiError && e.retryable)) opId = newOperationId();
		} finally {
			busy = false;
		}
	}
</script>

<div class={embedded ? 'logtime' : 'card logtime'} data-testid="logtime">
	{#if !embedded}<h3 style="margin-top:0">記工時{issueKey ? `：${issueKey}` : ''}</h3>{/if}
	<ErrorBox error={loadError} />
	<div class="lt-grid">
		<label>日期 <input type="date" bind:value={spentOn} data-testid="lt-date" /></label>
		<label>時數
			<input type="text" inputmode="decimal" bind:value={hoursText} placeholder="1.5、1:30 或 1h30m" style="width:140px" data-testid="lt-hours" aria-invalid={hoursText.trim() !== '' && hours === null} />
			<span class="hint" data-testid="lt-hours-parsed">{hoursText.trim() === '' ? '' : hours === null ? '看不懂這個時數' : `＝${hours} 小時`}</span>
		</label>
		<label>活動
			<select bind:value={activity} data-testid="lt-activity">
				{#each activities as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
			</select>
		</label>
		{#if users.length > 1}
			<label>替誰記
				<select bind:value={user} data-testid="lt-user">
					<option value="">我自己</option>
					{#each users.filter((u) => u.id !== session.user?.user_id) as u (u.id)}<option value={u.id}>{u.name}</option>{/each}
				</select>
			</label>
		{/if}
	</div>
	<label class="lt-c">說明 <span class="hint">{comments.length}/1024</span>
		<textarea bind:value={comments} maxlength="1024" rows="2" data-testid="lt-comments"></textarea>
	</label>
	{#if !embedded}
		<ErrorBox {error} />
		<div class="row">
			<button class="btn pri" onclick={send} disabled={busy || !entry} data-testid="lt-submit">{busy ? '送出中…' : '記工時'}</button>
			<button class="btn" onclick={() => oncancel?.()} disabled={busy}>取消</button>
		</div>
	{/if}
</div>

<style>
	.lt-grid {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: flex-start;
	}
	.lt-grid label,
	.lt-c {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
	.lt-c {
		margin: 8px 0;
	}
</style>

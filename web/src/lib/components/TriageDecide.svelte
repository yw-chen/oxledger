<script lang="ts">
	// One Triage decision (contract 05 §6.4): the small panel under a row of
	// the inbox, or under the issue page's Triage bar. The decision is one
	// command document; `triage_state_invalid` and `conflict` go back to the
	// caller (remove the row / reload it), anything else stays here.
	import { untrack } from 'svelte';
	import type { Assignee, InboxSettings, TransitionAction, TriageDecision } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { submit } from '#lib/commands.js';
	import { DECLINE_PREFIX, dupNote, noteText, statusCategories, triageDoc } from '#lib/triage.js';
	import { snoozePreset } from '#lib/hours.js';
	import ErrorBox from './ErrorBox.svelte';

	let {
		item,
		projectId,
		projectKey,
		settings,
		decision,
		ondone,
		oncancel
	}: {
		item: { key: string; lock_version: number; subject: string };
		projectId: string;
		projectKey: string;
		settings: InboxSettings | null;
		decision: TriageDecision;
		ondone: (r: 'done' | 'gone' | 'conflict', text?: string) => void;
		oncancel: () => void;
	} = $props();

	let assignees = $state<Assignee[]>([]);
	let transitions = $state<TransitionAction[]>([]);
	let doneIds = $state<Set<string>>(new Set());
	let assignee = $state('');
	let toStatus = $state('');
	let note = $state('');
	let dupQ = $state('');
	let dupHits = $state<{ key: string; subject: string }[]>([]);
	let dupPick = $state<string | null>(null);
	let snoozeKind = $state<'tomorrow' | 'monday' | 'custom'>('tomorrow');
	let snoozeAt = $state('');
	let busy = $state(false);
	let error = $state<unknown>(null);
	let loadError = $state<unknown>(null);
	// The same decision retried after a transient failure keeps its key (§8.7).
	let opId = newOperationId();

	// Starts over only when the decision or the issue changes: a parent that
	// rebuilds `item` or `settings` (a reload, "load more") keeps what was typed.
	$effect(() => {
		const d = decision;
		const key = item.key;
		const pid = projectId;
		untrack(() => start(d, key, pid));
	});
	function start(d: TriageDecision, key: string, projectId: string) {
		error = null;
		loadError = null;
		opId = newOperationId();
		note = d === 'decline' ? DECLINE_PREFIX : '';
		toStatus = d === 'decline' || d === 'duplicate' ? (settings?.decline_status?.id ?? '') : '';
		if (d === 'accept' || d === 'decline' || d === 'duplicate') {
			Promise.all([
				api('GET /issues/{key}/actions', { path: { key } }),
				d === 'accept' ? api('GET /projects/{id}/assignees', { path: { id: projectId } }) : Promise.resolve(null),
				d === 'accept' ? Promise.resolve(null) : statusCategories()
			])
				.then(([a, as, cats]) => {
					transitions = a.transitions.filter((t) => !t.blocked);
					if (as) assignees = as.assignees;
					if (cats) doneIds = new Set([...cats].filter(([, v]) => v.category === 'done').map(([k]) => k));
					// No preset (cross-project inbox) or the preset cannot be
					// reached: start from the first closed status offered.
					if (d !== 'accept' && !transitions.some((t) => t.to_status_id === toStatus && doneIds.has(t.to_status_id))) {
						toStatus = transitions.find((t) => doneIds.has(t.to_status_id))?.to_status_id ?? '';
					}
				})
				.catch((e) => (loadError = e));
		}
	}

	/** Decline and duplicate move to a closed status (§3.10.1). */
	const closing = $derived(transitions.filter((t) => doneIds.has(t.to_status_id)));
	const statusChoices = $derived(decision === 'accept' ? transitions : closing);
	const presetMissing = $derived(
		(decision === 'decline' || decision === 'duplicate') && !!settings?.decline_status && transitions.length > 0 && !closing.some((t) => t.to_status_id === settings?.decline_status?.id)
	);

	let dupSeq = 0;
	async function findDup() {
		const q = dupQ.trim();
		const seq = ++dupSeq;
		if (!q) {
			dupHits = [];
			return;
		}
		try {
			const r = await api('POST /issues/search', { body: { q, project: projectKey, limit: 8, explain: 'none', fields: ['key', 'subject'] } });
			if (seq !== dupSeq) return;
			dupHits = r.items.filter((i) => i.key !== item.key).map((i) => ({ key: i.key, subject: String(i.subject ?? '') }));
		} catch (e) {
			if (seq === dupSeq) error = e;
		}
	}
	function pickDup(k: string) {
		const before = dupPick ? dupNote(dupPick) : '';
		dupPick = k;
		if (!note.trim() || note === before) note = dupNote(k);
	}

	/** §3.10.1: 1 hour to 90 days from now. */
	const snoozeOut = $derived.by(() => {
		if (snoozeKind !== 'custom' || !snoozeAt) return false;
		const t = new Date(snoozeAt).getTime() - Date.now();
		return !(t >= 3600_000 && t <= 90 * 86400_000);
	});
	const declineEmpty = $derived(decision === 'decline' && !noteText(note));
	const canSend = $derived.by(() => {
		if (busy) return false;
		if (decision === 'decline') return !!toStatus;
		if (decision === 'duplicate') return !!toStatus && !!dupPick;
		if (decision === 'snooze') return snoozeKind !== 'custom' || (!!snoozeAt && !snoozeOut);
		return true;
	});

	async function send() {
		busy = true;
		error = null;
		try {
			let dupOf = null;
			if (decision === 'duplicate' && dupPick) {
				const t = await api('GET /issues/{key}', { path: { key: dupPick } });
				dupOf = { key: t.key, lock: t.lock_version };
			}
			const snoozeUntil = decision === 'snooze' ? (snoozeKind === 'custom' ? new Date(snoozeAt).toISOString() : snoozePreset(snoozeKind)) : null;
			const d = triageDoc(item.key, item.lock_version, { decision, assignee, toStatus, note, dupOf, snoozeUntil });
			await submit(d, opId);
			const DONE: Record<TriageDecision, string> = {
				accept: `已接受 ${item.key}`,
				decline: `已拒絕 ${item.key}`,
				duplicate: `已把 ${item.key} 標為 ${dupPick} 的重複`,
				snooze: `${item.key} 稍後再處理`,
				unsnooze: `已取消 ${item.key} 的稍後處理`
			};
			ondone('done', DONE[decision]);
		} catch (e) {
			if (e instanceof ApiError && e.code === 'triage_state_invalid') return ondone('gone', '這張單已經被處理了');
			if (e instanceof ApiError && e.code === 'conflict') {
				// Someone decided first: the stale lock answers conflict, not
				// triage_state_invalid. Say which it was.
				const now = await api('GET /issues/{key}', { path: { key: item.key } }).catch(() => null);
				const st = now?.triage?.state;
				if (now && st !== 'pending' && st !== 'snoozed') return ondone('gone', '這張單已經被處理了');
				return ondone('conflict', '這張單剛被別人更新，已重新載入');
			}
			error = e;
			if (!(e instanceof ApiError && e.retryable)) opId = newOperationId();
		} finally {
			busy = false;
		}
	}
	const TITLE: Record<TriageDecision, string> = { accept: '接受並…', decline: '拒絕', duplicate: '標為重複', snooze: '稍後處理', unsnooze: '取消稍後' };
</script>

<div class="card tdecide" data-testid="triage-panel" data-decision={decision}>
	<h3>{TITLE[decision]}：<span class="mono">{item.key}</span> {item.subject}</h3>
	<ErrorBox error={loadError} />
	{#if decision === 'accept'}
		<label class="tfield">指派給
			<select bind:value={assignee} data-testid="tp-assignee">
				<option value="">（維持不變）</option>
				{#each assignees as a (a.id)}<option value={a.id}>{a.name}{a.kind === 'group' ? '（群組）' : ''}</option>{/each}
			</select>
		</label>
		<label class="tfield">狀態
			<select bind:value={toStatus} data-testid="tp-status">
				<option value="">（維持不變）</option>
				{#each statusChoices as t (t.to_status_id)}<option value={t.to_status_id}>{t.name}</option>{/each}
			</select>
		</label>
	{:else if decision === 'decline' || decision === 'duplicate'}
		{#if decision === 'duplicate'}
			<label class="tfield">原單（同專案）
				<input type="search" bind:value={dupQ} oninput={findDup} placeholder="輸入單號或標題" data-testid="tp-dup-q" />
			</label>
			{#if dupHits.length}
				<div class="duphits" role="listbox" aria-label="原單候選">
					{#each dupHits as h (h.key)}
						<button type="button" class="btn sm" class:pri={dupPick === h.key} role="option" aria-selected={dupPick === h.key} onclick={() => pickDup(h.key)} data-testid="tp-dup-hit"><span class="mono">{h.key}</span> {h.subject}</button>
					{/each}
				</div>
			{:else if dupQ.trim()}
				<div class="hint">同專案裡找不到符合的單。</div>
			{/if}
		{/if}
		<label class="tfield">轉到
			<select bind:value={toStatus} data-testid="tp-status">
				{#if !statusChoices.length}<option value="">（沒有可用的已結案狀態）</option>{/if}
				{#each statusChoices as t (t.to_status_id)}<option value={t.to_status_id}>{t.name}</option>{/each}
			</select>
		</label>
		{#if presetMissing}<div class="hint" data-testid="tp-preset-missing">設定的「{settings?.decline_status?.name}」目前轉不過去，請選其他已結案狀態。</div>{/if}
	{:else if decision === 'snooze'}
		<div class="row" role="radiogroup" aria-label="稍後到">
			<label><input type="radio" bind:group={snoozeKind} value="tomorrow" /> 明天早上 9 點</label>
			<label><input type="radio" bind:group={snoozeKind} value="monday" /> 下週一 9 點</label>
			<label><input type="radio" bind:group={snoozeKind} value="custom" /> 自訂</label>
			{#if snoozeKind === 'custom'}<input type="datetime-local" bind:value={snoozeAt} data-testid="tp-snooze-at" />{/if}
			{#if snoozeOut}<span class="hint" style="color:var(--bad)" data-testid="tp-snooze-range">要在 1 小時到 90 天之後。</span>{/if}
		</div>
		<div class="hint">到時間會回到收件匣；可以是 1 小時到 90 天之後。</div>
	{/if}
	{#if decision !== 'snooze' && decision !== 'unsnooze'}
		<label class="tfield">附註{decision === 'accept' ? '（選填）' : ''}
			<textarea bind:value={note} rows="2" data-testid="tp-note"></textarea>
		</label>
		{#if declineEmpty}<div class="hint" data-testid="tp-reason-hint">留個理由，回報者比較知道為什麼。</div>{/if}
	{/if}
	<ErrorBox {error} />
	<div class="row">
		<button class="btn pri" onclick={send} disabled={!canSend} data-testid="tp-submit">{busy ? '送出中…' : '送出'}</button>
		<button class="btn" onclick={oncancel} disabled={busy}>取消</button>
	</div>
</div>

<style>
	.tdecide h3 {
		margin-top: 0;
	}
	.tfield {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin-bottom: 8px;
	}
	.duphits {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-bottom: 8px;
	}
</style>

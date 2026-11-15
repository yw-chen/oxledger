<script lang="ts">
	// D12 for the wiki (design 06 §3.13, §6.6): a version's content, a page's
	// titles, or one non-content event. Reason code and reason, after a
	// step-up; the title asks for the page's new title too. The web had no
	// redaction dialog before M2.2, so this is the first one.
	import { api } from '#lib/api.js';
	import { stepped } from '#lib/stepup.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from './ErrorBox.svelte';
	import type { RedactTarget } from '#lib/wiki.js';

	let { projectKey, pageId, target, onclose, ondone }: { projectKey: string; pageId: string; target: RedactTarget; onclose: () => void; ondone: (newTitle?: string) => void } = $props();

	let code = $state('personal_data');
	let reason = $state('');
	let newTitle = $state('');
	let busy = $state(false);
	let error = $state<unknown>(null);
	const CODES: [string, string][] = [
		['personal_data', '個人資料'],
		['confidential', '機密'],
		['legal', '法律要求'],
		['other', '其他']
	];
	const heading = $derived(target.kind === 'version' ? `遮蔽第 ${target.n} 版的內容` : target.kind === 'title' ? '遮蔽這一頁的所有標題' : '遮蔽這筆變更紀錄');
	const ok = $derived(reason.trim().length >= 1 && reason.trim().length <= 500 && (target.kind !== 'title' || newTitle.trim().length > 0));

	async function send(e: Event) {
		e.preventDefault();
		busy = true;
		error = null;
		try {
			const body = { reason_code: code, reason: reason.trim() };
			const path = { id: projectKey, title: pageId };
			const r = await stepped(async () => {
				if (target.kind === 'version') return api('POST /projects/{id}/wiki/pages/{title}/versions/{n}/redact', { path: { ...path, n: target.n }, body });
				if (target.kind === 'title') return api('POST /projects/{id}/wiki/pages/{title}/title/redact', { path, body: { ...body, new_title: newTitle.trim() } });
				return api('POST /projects/{id}/wiki/pages/{title}/events/{event}/redact', { path: { ...path, event: target.id }, body });
			}, '遮蔽需要再確認一次密碼');
			if (r === null) return;
			toast(r.changed ? '已遮蔽' : '先前已經遮蔽過了', 'ok');
			ondone(target.kind === 'title' ? newTitle.trim() : undefined);
		} catch (err) {
			error = err;
		} finally {
			busy = false;
		}
	}
</script>

<div class="modal-bg">
	<div class="modal" role="dialog" aria-modal="true" aria-label={heading} data-testid="wiki-redact">
		<h3>{heading}</h3>
		<p class="muted" style="font-size:13px">
			{#if target.kind === 'version'}這一版的內容與修改說明會變成「[已遮蔽]」，所有人都看不到，不能復原。
			{:else if target.kind === 'title'}這一頁過去用過的標題都會變成「[已遮蔽]」，舊標題的轉址會刪除，這一頁改用新標題。
			{:else}「{target.text}」的內容會變成「[已遮蔽]」。{/if}
		</p>
		<form onsubmit={send} style="display:flex;flex-direction:column;gap:8px">
			<label>原因<select bind:value={code} data-testid="wiki-redact-code">{#each CODES as [v, t] (v)}<option value={v}>{t}</option>{/each}</select></label>
			<label>說明（不要貼上要遮蔽的原文）<textarea bind:value={reason} maxlength="500" rows="3" data-testid="wiki-redact-reason"></textarea></label>
			{#if target.kind === 'title'}<label>新標題<input bind:value={newTitle} maxlength="100" data-testid="wiki-redact-title" /></label>{/if}
			<ErrorBox {error} />
			<div class="row" style="justify-content:flex-end;gap:6px">
				<button type="button" class="btn" onclick={onclose}>取消</button>
				<button class="btn bad" disabled={busy || !ok} data-testid="wiki-redact-confirm">遮蔽</button>
			</div>
		</form>
	</div>
</div>

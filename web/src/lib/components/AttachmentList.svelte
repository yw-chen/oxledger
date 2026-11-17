<script lang="ts">
	// Design 06 §6.8 附件卡: name (icon by type), size, who, when, 「下載」;
	// images as a thumbnail grid; 「刪除」 for those who may (confirm 「刪除附
	// 件？30 天內可以復原。」), then 「復原」 (R-A7 undo) on the same card.
	// With `onfiles`, an 「附加檔案」 button (the parent uploads and attaches).
	import type { AttachmentMeta } from '$api/types';
	import { api, newOperationId } from '#lib/api.js';
	import { contentUrl, deleteDoc, iconFor, sizeText, split, UNDO_REASON } from '#lib/attachments.js';
	import { fmtTime } from '#lib/format.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from './ErrorBox.svelte';

	let {
		attachments = [],
		title = '附件',
		onfiles,
		busy = false,
		onchanged,
		testid = 'attachments'
	}: {
		attachments?: AttachmentMeta[] | null;
		title?: string;
		/** Files picked with 「附加檔案」; absent: no button. */
		onfiles?: (files: File[]) => void;
		/** The parent is uploading or attaching. */
		busy?: boolean;
		/** After a delete or its undo: the parent reloads. */
		onchanged?: () => void;
		testid?: string;
	} = $props();

	const parts = $derived(split(attachments ?? []));
	let picker = $state<HTMLInputElement | null>(null);
	let error = $state<unknown>(null);
	let working = $state(false);
	/** The last delete, while it may be undone from here. */
	let undo = $state<{ execution: string; name: string; note?: string } | null>(null);

	async function remove(a: AttachmentMeta) {
		if (!confirm('刪除附件？30 天內可以復原。')) return;
		working = true;
		error = null;
		try {
			const r = await api('POST /commands', { body: deleteDoc(a.id, newOperationId()) });
			undo = { execution: r.execution_id, name: a.filename };
			toast(`已刪除「${a.filename}」`, 'ok');
			onchanged?.();
		} catch (e) {
			error = e;
		} finally {
			working = false;
		}
	}

	async function restore() {
		if (!undo) return;
		working = true;
		error = null;
		try {
			const p = await api('GET /executions/{id}/undo-preview', { path: { id: undo.execution } });
			const a = p.attachments[0];
			if (p.expired || !a?.restorable) {
				undo = { ...undo, note: UNDO_REASON[(p.expired ? 'expired' : a?.reason) ?? ''] ?? '不能復原' };
				return;
			}
			await api('POST /executions/{id}/undo', { path: { id: undo.execution }, body: {}, operationId: newOperationId() });
			toast(`已復原「${undo.name}」`, 'ok');
			undo = null;
			onchanged?.();
		} catch (e) {
			error = e;
		} finally {
			working = false;
		}
	}

	function picked(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		const files = Array.from(input.files ?? []);
		input.value = '';
		if (files.length) onfiles?.(files);
	}
</script>

{#if (attachments?.length ?? 0) > 0 || onfiles || undo}
	<div class="att" data-testid={testid}>
		<div class="head">
			<span class="t">{title}{#if attachments?.length}<span class="dim">（{attachments.length}）</span>{/if}</span>
			<span class="sp"></span>
			{#if onfiles}
				<button type="button" class="btn sm" disabled={busy || working} onclick={() => picker?.click()} data-testid="{testid}-add">附加檔案</button>
				<input bind:this={picker} type="file" multiple hidden onchange={picked} data-testid="{testid}-input" />
			{/if}
		</div>
		{#if undo}
			<div class="banner o row" data-testid="{testid}-undo-bar">
				<span>已刪除「{undo.name}」。</span>
				{#if undo.note}<span class="hint">{undo.note}</span>{/if}
				<span class="sp"></span>
				{#if !undo.note}<button class="btn sm" disabled={working} onclick={restore} data-testid="{testid}-undo">復原</button>{/if}
				<button class="btn sm ghost" aria-label="關閉" onclick={() => (undo = null)}>✕</button>
			</div>
		{/if}
		{#if parts.images.length}
			<div class="grid">
				{#each parts.images as a (a.id)}
					<figure class="thumb" data-testid="{testid}-image">
						<a href={contentUrl(a.id)} target="_blank" rel="noopener"><img src={contentUrl(a.id)} alt={a.filename} loading="lazy" /></a>
						<figcaption>
							<span class="name" title={a.filename}>{a.filename}</span>
							<span class="dim">{sizeText(a.size)}・{a.author}・{fmtTime(a.created_at)}</span>
							{#if a.can_delete}<button type="button" class="btn sm ghost del" disabled={working} onclick={() => remove(a)} data-testid="{testid}-delete">刪除</button>{/if}
						</figcaption>
					</figure>
				{/each}
			</div>
		{/if}
		{#each parts.files as a (a.id)}
			<div class="file" data-testid="{testid}-file">
				<span aria-hidden="true">{a.redacted ? '🚫' : iconFor(a)}</span>
				<span class="name" title={a.filename}>{a.filename}</span>
				<span class="dim">{sizeText(a.size)}・{a.author}・{fmtTime(a.created_at)}</span>
				<span class="sp"></span>
				{#if a.redacted}<span class="hint">內容已遮蔽</span>
				{:else}<a class="btn sm" href={contentUrl(a.id)} download={a.filename} data-testid="{testid}-download">下載</a>{/if}
				{#if a.can_delete && !a.redacted}<button type="button" class="btn sm ghost del" disabled={working} onclick={() => remove(a)} data-testid="{testid}-delete">刪除</button>{/if}
			</div>
		{/each}
		<ErrorBox {error} />
	</div>
{/if}

<style>
	.att { display: flex; flex-direction: column; gap: 6px; margin-top: 8px; }
	.head, .file, .row { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
	.head .t { font-weight: 600; }
	.sp { flex: 1; }
	.name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 28ch; }
	.file { border: 1px solid var(--line); border-radius: 7px; padding: 4px 8px; font-size: 13px; }
	.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(140px, 1fr)); gap: 8px; }
	.thumb { margin: 0; border: 1px solid var(--line); border-radius: 7px; overflow: hidden; display: flex; flex-direction: column; }
	.thumb img { width: 100%; height: 110px; object-fit: cover; display: block; background: var(--bg2); }
	.thumb figcaption { display: flex; flex-direction: column; gap: 2px; padding: 4px 6px; font-size: 12px; }
	/* §6.8: 「刪除」 shows on hover (and keyboard focus); touch screens always show it. */
	.del { opacity: 0; }
	.file:hover .del, .thumb:hover .del, .del:focus-visible { opacity: 1; }
	@media (hover: none) { .del { opacity: 1; } }
</style>

<script lang="ts">
	// R-W9: the person's last wiki write can be undone from the wiki pages
	// (preview first, like the agent activity page's undo, then one click).
	import type { UndoPreview } from '$api/types';
	import { api, newOperationId } from '#lib/api.js';
	import { wikiWrite } from '#lib/wikiwrite.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import ErrorBox from './ErrorBox.svelte';

	let { projectKey, onundone }: { projectKey: string; onundone?: () => void } = $props();

	const last = $derived(wikiWrite.last && wikiWrite.last.projectKey === projectKey.toUpperCase() ? wikiWrite.last : null);
	let preview = $state<UndoPreview | null>(null);
	let error = $state<unknown>(null);
	let busy = $state(false);
	let skip = $state<Record<string, boolean>>({});

	const CHANGE: Record<string, string> = {
		created: '刪除這次建立的頁（30 天內可以還原）',
		updated: '回到這次修改之前（內容會新增一版）',
		deleted: '還原這次刪除的頁',
		restored: '再刪除這次還原的頁',
		reparented: '把子頁搬回被刪的頁下面'
	};
	const REASON: Record<string, string> = {
		expired: '超過 30 天，已不能回復',
		changed_since: '之後已被修改（保留後來的決定）',
		redacted_source: '原本的版本已遮蔽',
		redacted: '內容已遮蔽',
		title_taken: '原本的標題已被別的頁使用',
		has_children: '這一頁現在有子頁',
		dependency: '它依賴的頁不能回復',
		value_no_longer_valid: '原本的位置現在已不合法',
		permission: '你目前沒有權限修改',
		wiki_depth: '搬回去會超過 10 層',
		wiki_page_limit: '這個 wiki 的頁數已達上限（5,000 頁）',
		wiki_version_limit: '這一頁的版本數已達上限（10,000 版）'
	};

	async function open() {
		if (!last) return;
		error = null;
		skip = {};
		busy = true;
		try {
			preview = await api('GET /executions/{id}/undo-preview', { path: { id: last.executionId } });
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	const restorable = $derived(!!preview && !preview.expired && preview.wiki.some((w) => w.restorable && !skip[w.page_id]));
	async function confirmUndo() {
		if (!preview) return;
		busy = true;
		error = null;
		try {
			const r = await api('POST /executions/{id}/undo', {
				path: { id: preview.execution_id },
				// Pages that cannot be undone are skipped too: one at a page or version
				// limit left in makes the whole undo `invalid_value` (no partial undo).
				body: { skip_wiki_pages: preview.wiki.filter((w) => !w.restorable || skip[w.page_id]).map((w) => w.page_id) },
				operationId: newOperationId()
			});
			// The undo is an execution of its own: it can be undone in turn.
			wikiWrite.last = { executionId: r.execution_id, projectKey: projectKey.toUpperCase(), text: '復原' };
			preview = null;
			toast(`已復原 ${r.wiki_pages_restored} 頁`, 'ok');
			onundone?.();
		} catch (e) {
			error = e;
		} finally {
			busy = false;
		}
	}
	function dismiss() {
		wikiWrite.last = null;
		preview = null;
		error = null;
	}
</script>

{#if last}
	<div class="banner o" data-testid="wiki-undo-bar">
		<div class="row" style="gap:8px">
			<span>已{last.text}。</span>
			<span style="flex:1"></span>
			{#if !preview}<button class="btn sm" onclick={open} disabled={busy} data-testid="wiki-undo">復原…</button>{/if}
			<button class="btn sm ghost" onclick={dismiss} aria-label="關閉">✕</button>
		</div>
		{#if preview}
			<div style="margin-top:6px" data-testid="wiki-undo-preview">
				{#if preview.expired}<div class="hint">超過 30 天，這筆已不能回復。</div>{/if}
				{#each preview.wiki as w (w.page_id)}
					<label class="row" style="gap:6px">
						{#if w.restorable}<input type="checkbox" checked={!skip[w.page_id]} onchange={(e) => (skip[w.page_id] = !e.currentTarget.checked)} aria-label="回復〈{w.title}〉" />{:else}<span class="chip c-dim">跳過</span>{/if}
						〈{w.title}〉：{CHANGE[w.change] ?? w.change}
						{#if !w.restorable}<span class="hint">（{REASON[w.reason ?? ''] ?? w.reason ?? '不能回復'}）</span>{/if}
					</label>
				{:else}
					<div class="hint">這筆變更沒有可以回復的 wiki 頁。</div>
				{/each}
				<div class="row" style="gap:6px;margin-top:6px">
					<button class="btn sm pri" onclick={confirmUndo} disabled={busy || !restorable} data-testid="wiki-undo-confirm">復原選取的部分</button>
					<button class="btn sm" onclick={() => (preview = null)}>取消</button>
				</div>
			</div>
		{/if}
		<ErrorBox {error} />
	</div>
{/if}

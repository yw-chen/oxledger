<script lang="ts">
	// Design 06 §6.4: create or edit a wiki page. A new page has its title and
	// parent here; an existing page is renamed from the page's 「改名／移動」.
	// `wiki_put` carries the lock_version the person opened (R-W3). A
	// conflict never overwrites: a red bar names who saved and which version,
	// 「比較差異」 shows the newest version beside the person's text, then
	// 「以我的內容為基礎重新儲存」 (asked twice) or 「放棄我的修改」. The text
	// stays in the editor whatever happens.
	import { goto } from '$app/navigation';
	import { page as route } from '$app/state';
	import type { Assignee, UploadView, WikiIndex, WikiPage } from '$api/types';
	import { linked, MAX_PER_OP, TOO_MANY } from '#lib/attachments.js';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { FORBIDDEN, NOT_FOUND, wikiTitleProblem } from '#lib/errors.js';
	import { draftKey, dropDraft, LIMIT, type Mentionable } from '#lib/markdown.js';
	import { session } from '#lib/session.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import { buildTree, diffText, flatTree, titleFromParam, urlTitle, wikiPath } from '#lib/wiki.js';
	import { runWiki } from '#lib/wikiwrite.svelte.js';
	import ErrorBox from './ErrorBox.svelte';
	import MarkdownEditor from './MarkdownEditor.svelte';
	import WikiDiffView from './WikiDiffView.svelte';

	let { projectKey, title = null }: { projectKey: string; title?: string | null } = $props();

	let index = $state<WikiIndex | null>(null);
	let page = $state<WikiPage | null>(null);
	let loadError = $state<unknown>(null);
	let ready = $state(false);
	let people = $state<Mentionable[]>([]);

	let newTitle = $state('');
	let parentId = $state('');
	let body = $state('');
	let base = $state('');
	let comment = $state('');
	let over = $state(false);
	let dirty = $state(false);
	let busy = $state(false);
	let saved = $state(false);
	let error = $state<unknown>(null);
	let titleError = $state<{ text: string; pageId?: string } | null>(null);
	/** The newest version after a conflict (§6.4). */
	let conflict = $state<WikiPage | null>(null);
	let compare = $state(false);
	let opKey = newOperationId();
	// §6.2 附檔 (an existing page; a new one gets files once it exists).
	let uploads = $state<UploadView[]>([]);
	let uploading = $state(0);
	const sentFiles = $derived(linked(uploads, body));
	const withFiles = () => (sentFiles.length ? { attachments: sentFiles.map((u) => ({ upload_id: u.upload_id })) } : {});

	async function load() {
		ready = false;
		const [ix, pg] = await Promise.allSettled([
			api('GET /projects/{id}/wiki', { path: { id: projectKey } }),
			title ? api('GET /projects/{id}/wiki/pages/{title}', { path: { id: projectKey, title: urlTitle(title) } }) : Promise.reject(null)
		]);
		index = ix.status === 'fulfilled' ? ix.value : null;
		page = pg.status === 'fulfilled' ? pg.value : null;
		const missing = pg.status === 'rejected' && (pg.reason === null || (pg.reason instanceof ApiError && pg.reason.code === 'not_found'));
		loadError = pg.status === 'rejected' && !missing ? pg.reason : null;
		if (page) {
			body = base = page.body;
		} else {
			newTitle = title ? titleFromParam(title) : '';
			parentId = route.url.searchParams.get('parent') ?? '';
			if (parentId && !index?.tree.some((t) => t.id === parentId)) parentId = '';
		}
		const pid = index?.project.id;
		if (pid)
			api('GET /projects/{id}/assignees', { path: { id: pid } })
				.then((a) => (people = a.assignees as Assignee[] as Mentionable[]))
				.catch(() => {});
		ready = true;
	}
	$effect(() => {
		void title;
		void projectKey;
		load();
	});

	const creating = $derived(ready && !page);
	const options = $derived(index ? flatTree(buildTree(index.tree)) : []);
	const target = $derived(page ? `wiki:${page.id}` : `wiki:${index?.project.id ?? projectKey}:new`);
	const draft = $derived(session.user && ready ? draftKey(session.user.user_id, target) : null);
	/** Why saving is off (05 §6): shown on the button. */
	const blocked = $derived.by(() => {
		if (!ready) return '';
		const ro = page?.read_only ?? index?.read_only;
		if (ro) return '專案已關閉，wiki 唯讀';
		if (page && !page.can.edit) return page.protected && index?.can.create ? '這一頁受保護，只有有保護權限的人能編輯' : FORBIDDEN;
		if (!page && !index?.can.create) return '你沒有權限建立頁面';
		return '';
	});

	async function save() {
		busy = true;
		error = null;
		titleError = null;
		try {
			if (page) {
				const { effect } = await runWiki(
					projectKey,
					{ kind: 'wiki_put', op_id: newOperationId(), target: { project: projectKey, page_id: page.id }, body, ...(comment.trim() ? { comment: comment.trim() } : {}), ...withFiles() },
					{ pageId: page.id, lock: page.lock_version },
					'儲存',
					opKey
				);
				done(effect?.title ?? page.title, effect?.changed === false ? '內容沒有變更' : `已儲存（第 ${effect?.version ?? ''} 版）`);
			} else {
				const t = newTitle.trim();
				const { effect } = await runWiki(
					projectKey,
					{ kind: 'wiki_put', op_id: newOperationId(), target: { project: projectKey, page: t }, body, ...(comment.trim() ? { comment: comment.trim() } : {}), ...(parentId ? { parent: { page_id: parentId } } : {}) },
					null,
					`建立〈${t}〉`,
					opKey
				);
				done(effect?.title ?? t, '已建立');
			}
		} catch (e) {
			// A refused write changed nothing: the next try is a new request.
			opKey = newOperationId();
			if (!(e instanceof ApiError)) error = e;
			else if (e.code === 'conflict' && page) await onConflict();
			else if (e.code === 'wiki_page_exists') titleError = { text: '已經有同名的頁面', pageId: typeof e.details.page_id === 'string' ? e.details.page_id : undefined };
			else if (e.code === 'wiki_title_invalid') titleError = { text: wikiTitleProblem(e.details) };
			else error = e;
		} finally {
			busy = false;
		}
	}
	function done(t: string, msg: string) {
		if (draft) dropDraft(draft);
		saved = true;
		toast(msg, 'ok');
		goto(wikiPath(projectKey, t));
	}
	async function onConflict() {
		if (!page) return;
		try {
			conflict = await api('GET /projects/{id}/wiki/pages/{title}', { path: { id: projectKey, title: urlTitle(page.title) } });
		} catch (e) {
			error = e;
		}
	}
	/** 「以我的內容為基礎重新儲存」: on top of the newest version, after a second confirmation. */
	async function saveMine() {
		if (!conflict || !page) return;
		if (!confirm(`要用你的內容覆蓋第 ${conflict.version} 版嗎？${conflict.updated_by?.name ?? '別人'}在那一版的修改會被取代（仍保留在歷史裡）。`)) return;
		page = { ...page, lock_version: conflict.lock_version, version: conflict.version };
		conflict = null;
		compare = false;
		await save();
	}
	/** 「放棄我的修改」: the newest version in the editor. */
	function discardMine() {
		if (!conflict) return;
		page = conflict;
		body = base = conflict.body;
		conflict = null;
		compare = false;
		if (draft) dropDraft(draft);
	}
	const sameTitle = $derived(titleError?.pageId ? (index?.tree.find((t) => t.id === titleError!.pageId)?.title ?? newTitle.trim()) : null);
	const cancelHref = $derived(page ? wikiPath(projectKey, page.title) : `/project/${projectKey}/wiki`);
	const conflictHunks = $derived(conflict && compare ? diffText(conflict.body, body) : []);
</script>

<svelte:head><title>{page ? `編輯〈${page.title}〉` : '新增 wiki 頁面'}・{projectKey}・oxledger</title></svelte:head>

{#if !ready}
	<div class="sync"><span class="spin"></span>載入中…</div>
{:else if !index}
	<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
{:else if loadError}
	<ErrorBox error={loadError} />
{:else}
	<div class="wed" data-testid="wiki-editor">
		<h1 style="margin:10px 0 6px">{page ? `編輯〈${page.title}〉` : '新增頁面'}</h1>
		{#if blocked}<div class="banner w" data-testid="wiki-edit-blocked">{blocked}</div>{/if}
		{#if conflict}
			<div class="banner b" role="alert" data-testid="wiki-conflict">
				<div>這一頁在你編輯時被 {conflict.updated_by?.name ?? '別人'} 更新了（第 {conflict.version} 版）。你的內容還在下面，尚未儲存。</div>
				<div class="row" style="gap:6px;margin-top:6px">
					<button class="btn sm" onclick={() => (compare = true)} data-testid="wiki-conflict-compare">比較差異</button>
					<button class="btn sm" onclick={saveMine} disabled={busy} data-testid="wiki-conflict-mine">以我的內容為基礎重新儲存</button>
					<button class="btn sm" onclick={discardMine} data-testid="wiki-conflict-discard">放棄我的修改</button>
				</div>
			</div>
		{/if}
		{#if creating}
			<label class="fld">標題
				<input bind:value={newTitle} maxlength="100" aria-invalid={!!titleError} aria-describedby="wiki-title-err" oninput={() => (titleError = null)} data-testid="wiki-title-input" />
			</label>
			{#if titleError}
				<div class="forbid-note" id="wiki-title-err" data-testid="wiki-title-error">
					{titleError.text}{#if sameTitle}：<a href={wikiPath(projectKey, sameTitle)}>開啟〈{sameTitle}〉</a>{/if}
				</div>
			{/if}
			<label class="fld">上層
				<select bind:value={parentId} data-testid="wiki-parent">
					<option value="">（最上層）</option>
					{#each options as o (o.id)}<option value={o.id}>{'　'.repeat(o.depth)}{o.title}</option>{/each}
				</select>
			</label>
		{:else if titleError}
			<div class="forbid-note" data-testid="wiki-title-error">{titleError.text}</div>
		{/if}
		<MarkdownEditor
			bind:value={body}
			bind:over
			bind:dirty
			{base}
			{people}
			context={{ wiki_project: index.project.id }}
			limit={LIMIT.wiki}
			notice={page?.updated_by?.kind === 'agent' || page?.by_agent ? 'agent' : null}
			{draft}
			guard={!saved}
			disabled={!!blocked}
			label="內容"
			uploadProject={page?.can.attach ? index.project.id : null}
			onuploaded={(u) => (uploads = [...uploads, u])}
			bind:uploading
			testid="wiki-md"
		/>
		<label class="fld">修改說明<input bind:value={comment} maxlength="255" placeholder="（選填）這次改了什麼" data-testid="wiki-comment" /></label>
		<ErrorBox {error} />
		<div class="row" style="gap:8px;margin-top:8px">
			<button class="btn pri" onclick={save} disabled={busy || !!blocked || over || !!conflict || (creating && !newTitle.trim()) || uploading > 0 || sentFiles.length > MAX_PER_OP} title={blocked} data-testid="wiki-save">{busy ? '儲存中…' : '儲存'}</button>
			<a class="btn" href={cancelHref} data-testid="wiki-cancel">取消</a>
			{#if sentFiles.length > MAX_PER_OP}<span class="hint" data-testid="wiki-too-many">{TOO_MANY}</span>{/if}
		</div>
	</div>
{/if}

{#if compare && conflict}
	<div class="modal-bg">
		<div class="modal wide" role="dialog" aria-modal="true" aria-label="比較差異" data-testid="wiki-conflict-diff">
			<h3>第 {conflict.version} 版（最新）和你的內容</h3>
			<WikiDiffView hunks={conflictHunks} newText={body} leftLabel="第 {conflict.version} 版（最新）" rightLabel="你的內容" testid="wiki-conflict-diffview" />
			<div class="row" style="justify-content:flex-end;gap:6px;margin-top:8px">
				<button class="btn" onclick={() => (compare = false)}>關閉</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.wed { display: flex; flex-direction: column; gap: 8px; }
	.fld { display: flex; flex-direction: column; gap: 2px; font-size: 13px; }
	.fld input, .fld select { max-width: 520px; }
	.modal.wide { width: min(1100px, 96vw); max-height: 90vh; overflow: auto; }
</style>

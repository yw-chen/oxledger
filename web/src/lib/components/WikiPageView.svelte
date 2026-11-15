<script lang="ts">
	// Design 06 §6.3 / §6.5: one wiki page. Breadcrumbs and actions (編輯, ⋯:
	// 改名／移動, 保護, 歷史, 遮蔽標題, 刪除), the status line, the content,
	// the child pages and the tree on the left. An action the person may not
	// do stays visible, disabled, with the reason on hover (05 §6); 歷史 is a
	// read permission, so without it the item is not there. Keys: E edit, H history.
	import { goto } from '$app/navigation';
	import type { WikiIndex, WikiPage } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { FORBIDDEN, NOT_FOUND } from '#lib/errors.js';
	import { fmtShort } from '#lib/format.js';
	import { session } from '#lib/session.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import { buildTree, flatTree, subtreeIds, titleFromParam, urlTitle, wikiPath, type RedactTarget } from '#lib/wiki.js';
	import { runWiki } from '#lib/wikiwrite.svelte.js';
	import ErrorBox from './ErrorBox.svelte';
	import RichText from './RichText.svelte';
	import WikiLayout from './WikiLayout.svelte';
	import WikiUndoBar from './WikiUndoBar.svelte';
	import WikiRedactDialog from './WikiRedactDialog.svelte';
	import type { Snippet } from 'svelte';

	let { projectKey, title, index: given = null, below }: { projectKey: string; title: string; index?: WikiIndex | null; below?: Snippet } = $props();

	let index = $state<WikiIndex | null>(null);
	let wikiError = $state<unknown>(null);
	let page = $state<WikiPage | null>(null);
	let pageError = $state<unknown>(null);
	let loading = $state(true);
	/** manage_project (D12) — from the tab settings, the same permission. */
	let canRedact = $state(false);
	/** After a write the preloaded index is stale: fetch it again. */
	let fresh = false;

	async function load() {
		loading = true;
		const t = title;
		const [ix, pg] = await Promise.allSettled([
			given && !fresh ? Promise.resolve(given) : api('GET /projects/{id}/wiki', { path: { id: projectKey } }),
			api('GET /projects/{id}/wiki/pages/{title}', { path: { id: projectKey, title: urlTitle(t) } })
		]);
		if (t !== title) return;
		index = ix.status === 'fulfilled' ? ix.value : null;
		wikiError = ix.status === 'rejected' ? ix.reason : null;
		page = pg.status === 'fulfilled' ? pg.value : null;
		pageError = pg.status === 'rejected' ? pg.reason : null;
		loading = false;
	}
	$effect(() => {
		void title;
		void projectKey;
		page = null;
		menu = false;
		load();
	});
	$effect(() => {
		const k = projectKey;
		canRedact = !!session.user?.is_admin;
		api('GET /projects/{id}/tabs', { path: { id: k } })
			.then((t) => {
				if (k === projectKey) canRedact = canRedact || t.can_manage;
			})
			.catch(() => {});
	});

	const notFound = $derived(pageError instanceof ApiError && pageError.code === 'not_found');
	/** The page is missing but the wiki is readable (R-W1): the person may create it. */
	const missing = $derived(notFound && !!index);
	const missingTitle = $derived(titleFromParam(title));
	const ro = $derived(!!(page?.read_only ?? index?.read_only));

	/** Why an action is off (05 §6: disabled with the reason on hover). */
	function why(kind: 'edit' | 'rename' | 'delete' | 'protect'): string {
		if (!page) return '';
		if (page.can[kind]) return '';
		if (ro) return '專案已關閉，wiki 唯讀';
		if (kind !== 'protect' && page.protected && (kind !== 'edit' || index?.can.create)) return '這一頁受保護，只有有保護權限的人能編輯';
		return FORBIDDEN;
	}

	// ------------------------------------------------------------ keys
	let menu = $state(false);
	let dialog = $state<'move' | 'delete' | null>(null);
	let redact = $state<RedactTarget | null>(null);
	function keys(e: KeyboardEvent) {
		if (!page || e.ctrlKey || e.metaKey || e.altKey || dialog || redact) return;
		const el = e.target as HTMLElement | null;
		if (el && (el.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName))) return;
		const k = e.key.toLowerCase();
		if (k === 'e' && page.can.edit) {
			e.preventDefault();
			goto(wikiPath(projectKey, page.title, '/edit'));
		} else if (k === 'h' && page.can.history) {
			e.preventDefault();
			goto(wikiPath(projectKey, page.title, '/history'));
		}
	}

	// ------------------------------------------------------------ move / delete / protect
	let busy = $state(false);
	let actionError = $state<unknown>(null);
	let mvTitle = $state('');
	let mvParent = $state('');
	let mvKeep = $state(true);
	let opKey = '';
	const options = $derived.by(() => {
		if (!index || !page) return [];
		const out = subtreeIds(index.tree, page.id);
		return flatTree(buildTree(index.tree)).filter((n) => !out.has(n.id));
	});
	function openMove() {
		if (!page) return;
		menu = false;
		mvTitle = page.title;
		mvParent = page.parent?.id ?? '';
		mvKeep = true;
		actionError = null;
		opKey = newOperationId();
		dialog = 'move';
	}
	/** wiki_delete with a protected child (R-W5 as the server checks it): said in the dialog. */
	let childProtected = $state(false);
	function openDelete() {
		menu = false;
		actionError = null;
		childProtected = false;
		opKey = newOperationId();
		dialog = 'delete';
	}
	async function move(e: Event) {
		e.preventDefault();
		if (!page) return;
		const newTitle = mvTitle.trim();
		const titleChanged = newTitle !== page.title;
		const parentChanged = mvParent !== (page.parent?.id ?? '');
		if (!titleChanged && !parentChanged) {
			dialog = null;
			return;
		}
		busy = true;
		actionError = null;
		try {
			const { effect } = await runWiki(
				projectKey,
				{
					kind: 'wiki_move',
					op_id: newOperationId(),
					target: { project: projectKey, page_id: page.id },
					...(titleChanged ? { title: newTitle, keep_redirect: mvKeep } : {}),
					...(parentChanged ? { parent: mvParent ? { page_id: mvParent } : null } : {})
				},
				{ pageId: page.id, lock: page.lock_version },
				titleChanged ? `改名為〈${newTitle}〉` : '移動頁面',
				opKey
			);
			dialog = null;
			toast(titleChanged ? '已改名' : '已移動', 'ok');
			const to = effect?.title ?? newTitle;
			if (to !== page.title) await goto(wikiPath(projectKey, to));
			else await reload();
		} catch (err) {
			actionError = err;
		} finally {
			busy = false;
		}
	}
	async function remove() {
		if (!page) return;
		busy = true;
		actionError = null;
		try {
			const t = page.title;
			await runWiki(
				projectKey,
				{ kind: 'wiki_delete', op_id: newOperationId(), target: { project: projectKey, page_id: page.id }, ...(page.children.length ? { children: 'reparent' } : {}) },
				null,
				`刪除〈${t}〉`,
				opKey
			);
			dialog = null;
			toast(`已刪除〈${t}〉，30 天內可以由 wiki 管理者還原`, 'ok');
			await goto(`/project/${projectKey}/wiki`);
		} catch (err) {
			// The page itself is not protected: a protected child stopped the reparent.
			childProtected = err instanceof ApiError && err.code === 'forbidden_op' && err.details.reason === 'wiki_protected' && !page.protected && page.children.length > 0;
			actionError = childProtected ? null : err;
			opKey = newOperationId();
		} finally {
			busy = false;
		}
	}
	async function protect() {
		if (!page) return;
		menu = false;
		busy = true;
		actionError = null;
		const to = !page.protected;
		try {
			await runWiki(projectKey, { kind: 'wiki_protect', op_id: newOperationId(), target: { project: projectKey, page_id: page.id }, protected: to }, null, to ? '保護這一頁' : '取消保護');
			toast(to ? '已保護' : '已取消保護', 'ok');
			await reload();
		} catch (err) {
			actionError = err;
		} finally {
			busy = false;
		}
	}
	async function reload() {
		fresh = true;
		await load();
	}
	const parentTitle = $derived(page?.parent?.title ?? null);
</script>

<svelte:window onkeydown={keys} />

{#if loading && !page && !pageError}
	<div class="sync"><span class="spin"></span>載入中…</div>
{:else if !index && wikiError}
	<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
{:else}
	<WikiLayout {projectKey} {index} currentId={page?.id ?? null}>
		<WikiUndoBar {projectKey} onundone={reload} />
		{#if ro}<div class="banner w" data-testid="wiki-readonly">專案已關閉，wiki 唯讀。</div>{/if}
		{#if page}
			{#if page.redirected_from}<div class="banner redirect" data-testid="wiki-redirected">由〈{page.redirected_from}〉轉址過來</div>{/if}
			<div class="row wtop">
				<nav class="crumbs" aria-label="頁面位置" data-testid="wiki-crumbs">
					<a href="/project/{projectKey}/wiki">Wiki</a>
					{#each page.breadcrumbs as b (b.id)}<span class="sep">›</span><a href={wikiPath(projectKey, b.title)}>{b.title}</a>{/each}
					<span class="sep">›</span><span aria-current="page">{page.title}</span>
				</nav>
				<span style="flex:1"></span>
				{#if page.can.edit}
					<a class="btn sm pri" href={wikiPath(projectKey, page.title, '/edit')} data-testid="wiki-edit" aria-keyshortcuts="E">編輯</a>
				{:else}
					<button class="btn sm" disabled title={why('edit')} data-testid="wiki-edit">編輯</button>
				{/if}
				<div class="menu">
					<button class="btn sm" onclick={() => (menu = !menu)} aria-haspopup="true" aria-expanded={menu} aria-label="更多動作" data-testid="wiki-menu">⋯</button>
					{#if menu}
						<div class="mlist" role="menu">
							<button role="menuitem" disabled={!page.can.rename} title={why('rename')} onclick={openMove} data-testid="wiki-move">改名／移動</button>
							<button role="menuitem" disabled={!page.can.protect || busy} title={why('protect')} onclick={protect} data-testid="wiki-protect">{page.protected ? '取消保護' : '保護此頁'}</button>
							{#if page.can.history}<button role="menuitem" onclick={() => goto(wikiPath(projectKey, page!.title, '/history'))} data-testid="wiki-history">歷史</button>{/if}
							{#if canRedact}<button role="menuitem" onclick={() => ((menu = false), (redact = { kind: 'title', title: page!.title }))} data-testid="wiki-redact-title-open">遮蔽標題</button>{/if}
							<button role="menuitem" disabled={!page.can.delete} title={why('delete')} onclick={openDelete} data-testid="wiki-delete">刪除</button>
						</div>
					{/if}
				</div>
			</div>
			<h1 style="margin:4px 0 2px" data-testid="wiki-title">{page.title}</h1>
			<div class="hint row" style="gap:6px" data-testid="wiki-status">
				<span>第 {page.version} 版・{#if page.updated_by}{#if page.updated_by.kind === 'agent'}<span class="agname">{page.updated_by.name}</span><span class="agent-label">agent</span>{:else}{page.updated_by.name}{/if}{:else}（不明）{/if} 於 {fmtShort(page.updated_at)} 更新</span>
				{#if page.protected}<span class="chip c-warn" data-testid="wiki-protected">🔒 受保護</span>{/if}
			</div>
			<ErrorBox error={actionError} />
			<div class="card" style="margin-top:8px">
				<RichText html={page.body_html} text={page.body} agent={page.by_agent} empty="（這一頁沒有內容）" testid="wiki-body" />
			</div>
			{#if page.children.length}
				<section class="kids" data-testid="wiki-children">
					<h2>子頁面</h2>
					<ul>{#each page.children as c (c.id)}<li><a href={wikiPath(projectKey, c.title)}>{c.title}</a></li>{/each}</ul>
				</section>
			{/if}
		{:else if missing}
			<div class="card" data-testid="wiki-missing">
				<p>這一頁還不存在。</p>
				{#if index?.can.create}<a class="btn pri" href={wikiPath(projectKey, missingTitle, '/edit')} data-testid="wiki-create">建立〈{missingTitle}〉</a>{/if}
			</div>
		{:else if pageError}
			<ErrorBox error={pageError} />
		{/if}
		{@render below?.()}
	</WikiLayout>
{/if}

{#if dialog === 'move' && page}
	<div class="modal-bg">
		<div class="modal" role="dialog" aria-modal="true" aria-label="改名／移動" data-testid="wiki-move-dialog">
			<h3>改名／移動〈{page.title}〉</h3>
			<form onsubmit={move} style="display:flex;flex-direction:column;gap:8px">
				<label>標題<input bind:value={mvTitle} maxlength="100" data-testid="wiki-move-title" /></label>
				<label>上層
					<select bind:value={mvParent} data-testid="wiki-move-parent">
						<option value="">（最上層）</option>
						{#each options as o (o.id)}<option value={o.id}>{'　'.repeat(o.depth)}{o.title}</option>{/each}
					</select>
				</label>
				<label class="row" style="gap:6px"><input type="checkbox" bind:checked={mvKeep} disabled={mvTitle.trim() === page.title} data-testid="wiki-move-keep" />保留舊標題的轉址</label>
				<ErrorBox error={actionError} />
				<div class="row" style="justify-content:flex-end;gap:6px">
					<button type="button" class="btn" onclick={() => (dialog = null)}>取消</button>
					<button class="btn pri" disabled={busy || !mvTitle.trim()} data-testid="wiki-move-confirm">儲存</button>
				</div>
			</form>
		</div>
	</div>
{/if}
{#if dialog === 'delete' && page}
	<div class="modal-bg">
		<div class="modal" role="dialog" aria-modal="true" aria-label="刪除頁面" data-testid="wiki-delete-dialog">
			<h3>刪除〈{page.title}〉？</h3>
			<p>30 天內可以由 wiki 管理者還原。</p>
			{#if page.children.length}<p data-testid="wiki-delete-children">它的 {page.children.length} 個子頁會移到{parentTitle ? `〈${parentTitle}〉下面` : '最上層'}。</p>{/if}
			{#if childProtected}
				<div class="err" role="alert" data-testid="wiki-delete-child-protected"><b>子頁裡有受保護的頁，要有保護權限的人才能把它們一起移走。</b><div class="chg">沒有改到任何資料。請有保護權限的人刪除，或先把受保護的子頁移到別處。</div></div>
			{/if}
			<ErrorBox error={actionError} />
			<div class="row" style="justify-content:flex-end;gap:6px">
				<button type="button" class="btn" onclick={() => (dialog = null)}>取消</button>
				<button class="btn bad" onclick={remove} disabled={busy} data-testid="wiki-delete-confirm">刪除</button>
			</div>
		</div>
	</div>
{/if}
{#if redact && page}
	<WikiRedactDialog {projectKey} pageId={page.id} target={redact} onclose={() => (redact = null)} ondone={(t) => { redact = null; if (t) goto(wikiPath(projectKey, t)); else reload(); }} />
{/if}

<style>
	.wtop { gap: 6px; }
	.crumbs { display: flex; flex-wrap: wrap; gap: 4px; align-items: center; font-size: 13px; color: var(--muted); }
	.crumbs a { color: var(--act); text-decoration: none; }
	.crumbs .sep { color: var(--dim); }
	.redirect { background: var(--bg2); border: 1px solid var(--line); color: var(--muted); }
	.kids { margin-top: 12px; }
	.kids h2 { font-size: 14px; margin: 0 0 4px; }
	.kids ul { margin: 0; padding-left: 20px; }
</style>

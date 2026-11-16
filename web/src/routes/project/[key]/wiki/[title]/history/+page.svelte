<script lang="ts">
	// Design 06 §6.6: a page's versions (newest first, 50 at a time) with two
	// radios to compare, 「檢視」 and 「回復到這一版」 per row (off for a
	// redacted version, which the server refuses as a revert source), and the
	// non-content changes (rename, move, protect…). D12 (manage_project):
	// 「遮蔽這一版」 and 「遮蔽」 on a change.
	import { goto } from '$app/navigation';
	import { page as route } from '$app/state';
	import type { WikiEventItem, WikiIndex, WikiPage, WikiVersionItem } from '$api/types';
	import { api } from '#lib/api.js';
	import { FORBIDDEN, NOT_FOUND } from '#lib/errors.js';
	import { fmtShort } from '#lib/format.js';
	import { session } from '#lib/session.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import { deltaText, eventText, sizeDeltas, titleFromParam, urlTitle, wikiPath, type RedactTarget } from '#lib/wiki.js';
	import { revertTo } from '#lib/wikiwrite.svelte.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';
	import ProjectFrame from '#lib/components/ProjectFrame.svelte';
	import WikiLayout from '#lib/components/WikiLayout.svelte';
	import WikiUndoBar from '#lib/components/WikiUndoBar.svelte';
	import WikiRedactDialog from '#lib/components/WikiRedactDialog.svelte';

	const key = $derived((route.params.key ?? '').toUpperCase());
	const title = $derived(route.params.title ?? '');

	let index = $state<WikiIndex | null>(null);
	let page = $state<WikiPage | null>(null);
	let items = $state<WikiVersionItem[]>([]);
	let next = $state<string | null>(null);
	let events = $state<WikiEventItem[]>([]);
	let error = $state<unknown>(null);
	let actionError = $state<unknown>(null);
	let loading = $state(true);
	let busy = $state(false);
	let from = $state<number | null>(null);
	let to = $state<number | null>(null);
	let canRedact = $state(false);
	let redact = $state<RedactTarget | null>(null);
	let menu = $state<number | null>(null);

	async function load() {
		loading = true;
		error = null;
		try {
			const t = urlTitle(title);
			const [ix, pg] = await Promise.all([api('GET /projects/{id}/wiki', { path: { id: key } }), api('GET /projects/{id}/wiki/pages/{title}', { path: { id: key, title: t } })]);
			index = ix;
			page = pg;
			const [vs, evs] = await Promise.all([
				api('GET /projects/{id}/wiki/pages/{title}/versions', { path: { id: key, title: t } }),
				api('GET /projects/{id}/wiki/pages/{title}/events', { path: { id: key, title: t } }).catch(() => [] as WikiEventItem[])
			]);
			items = vs.items;
			next = vs.next_cursor;
			events = evs;
			to = vs.items[0]?.version ?? null;
			from = vs.items[1]?.version ?? (vs.items[0] ? 0 : null);
		} catch (e) {
			error = e;
		} finally {
			loading = false;
		}
	}
	async function more() {
		if (!next) return;
		try {
			const vs = await api('GET /projects/{id}/wiki/pages/{title}/versions', { path: { id: key, title: urlTitle(title) }, query: { cursor: next } });
			items = [...items, ...vs.items];
			next = vs.next_cursor;
		} catch (e) {
			actionError = e;
		}
	}
	$effect(() => {
		void key;
		void title;
		load();
	});
	$effect(() => {
		const k = key;
		canRedact = !!session.user?.is_admin;
		api('GET /projects/{id}/tabs', { path: { id: k } })
			.then((t) => {
				if (k === key) canRedact = canRedact || t.can_manage;
			})
			.catch(() => {});
	});

	const deltas = $derived(sizeDeltas(items));
	const titleOf = (id: unknown) => index?.tree.find((t) => t.id === id)?.title ?? '（已刪除或看不到的頁）';
	const revertWhy = (v: WikiVersionItem) => {
		if (v.redacted) return '這一版已遮蔽';
		if (page && v.version === page.version) return '這是目前的版本';
		if (page && !page.can.edit) return page.read_only ? '專案已關閉，wiki 唯讀' : page.protected && index?.can.create ? '這一頁受保護，只有有保護權限的人能編輯' : FORBIDDEN;
		return '';
	};
	async function revert(v: WikiVersionItem) {
		if (!page || revertWhy(v)) return;
		if (!confirm(`要把〈${page.title}〉回復到第 ${v.version} 版嗎？會新增一個版本，現在的內容仍保留在歷史裡。`)) return;
		busy = true;
		actionError = null;
		try {
			const { effect } = await revertTo(key, page, v.version);
			toast(effect?.changed === false ? '內容和目前相同，沒有變更' : `已回復到第 ${v.version} 版`, 'ok');
			await goto(wikiPath(key, effect?.title ?? page.title));
		} catch (e) {
			actionError = e;
		} finally {
			busy = false;
		}
	}
	function compare() {
		if (from === null || to === null || !page) return;
		const [a, b] = from <= to ? [from, to] : [to, from];
		goto(`${wikiPath(key, page.title, '/diff')}?from=${a}&to=${b}`);
	}
	const forbidden = $derived(error && (error as { code?: string }).code === 'forbidden_op');
</script>

<svelte:head><title>歷史・{titleFromParam(title)}・{key} Wiki・oxledger</title></svelte:head>
<ProjectFrame projectKey={key} active="wiki">
	{#if loading && !page}
		<div class="sync"><span class="spin"></span>載入中…</div>
	{:else if !index}
		<div class="empty" data-testid="wiki-not-found">{NOT_FOUND}</div>
	{:else}
		<WikiLayout projectKey={key} {index} currentId={page?.id ?? null}>
			<WikiUndoBar projectKey={key} onundone={load} />
			{#if page}
				<a class="hint" href={wikiPath(key, page.title)}>← 〈{page.title}〉</a>
				<h1 style="margin:4px 0 8px">〈{page.title}〉的歷史</h1>
			{/if}
			{#if forbidden}
				<div class="empty">你沒有權限查看這一頁的歷史。</div>
			{:else if error}
				<ErrorBox {error} />
			{:else if page}
				<ErrorBox error={actionError} />
				<div class="row" style="gap:8px;margin-bottom:6px">
					<button class="btn sm pri" onclick={compare} disabled={from === null || to === null || from === to} data-testid="wiki-compare">比較選取的版本</button>
					<span class="hint">左邊選起點、右邊選終點。</span>
				</div>
				<table class="t" data-testid="wiki-versions">
					<thead><tr><th>起點</th><th>終點</th><th>版本</th><th>時間</th><th>作者</th><th>修改說明</th><th>大小變化</th><th></th></tr></thead>
					<tbody>
						{#each items as v, i (v.version)}
							<tr data-testid="wiki-version-row" data-version={v.version}>
								<td><input type="radio" name="from" value={v.version} bind:group={from} aria-label="從第 {v.version} 版比較" /></td>
								<td><input type="radio" name="to" value={v.version} bind:group={to} aria-label="比較到第 {v.version} 版" /></td>
								<td>第 {v.version} 版{#if v.version === page.version}<span class="chip c-ok" style="margin-left:4px">目前</span>{/if}</td>
								<td class="hint">{fmtShort(v.created_at)}</td>
								<td>{#if v.author}{#if v.author.kind === 'agent'}<span class="agname">{v.author.name}</span><span class="agent-label">agent</span>{#if v.on_behalf_of}<span class="hint" data-testid="wiki-version-behalf">代 {v.on_behalf_of.name}</span>{/if}{:else}{v.author.name}{/if}{:else}（不明）{/if}{#if v.approver}<span class="hint" data-testid="wiki-version-approver">・{v.approver.name} 核准</span>{/if}</td>
								<td>{v.comment}{#if v.redacted}<span class="chip c-dim" style="margin-left:4px" data-testid="wiki-version-redacted">[已遮蔽]</span>{/if}</td>
								<td class="mono" title="位元組">{deltaText(deltas[i])}</td>
								<td class="row" style="gap:4px;flex-wrap:nowrap">
									<a class="btn sm" href={wikiPath(key, page.title, `/versions/${v.version}`)} data-testid="wiki-version-view">檢視</a>
									<button class="btn sm" disabled={!!revertWhy(v) || busy} title={revertWhy(v)} onclick={() => revert(v)} data-testid="wiki-version-revert">回復到這一版</button>
									{#if canRedact}
										<div class="menu">
											<button class="btn sm ghost" onclick={() => (menu = menu === v.version ? null : v.version)} aria-haspopup="true" aria-expanded={menu === v.version} aria-label="第 {v.version} 版的更多動作">⋯</button>
											{#if menu === v.version}
												<div class="mlist" role="menu">
													<button role="menuitem" disabled={v.redacted} title={v.redacted ? '這一版已遮蔽' : ''} onclick={() => ((menu = null), (redact = { kind: 'version', n: v.version }))} data-testid="wiki-version-redact">遮蔽這一版</button>
												</div>
											{/if}
										</div>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
				{#if next}<button class="btn sm" style="margin-top:6px" onclick={more}>載入更舊的版本</button>{/if}

				{#if events.length}
					<h2 style="font-size:14px;margin:16px 0 6px">其他變更</h2>
					<ul class="evs" data-testid="wiki-events">
						{#each [...events].reverse() as ev (ev.id)}
							<li class="ev">
								{fmtShort(ev.at)}・{#if ev.actor}{ev.actor.name}{#if ev.actor.kind === 'agent'}<span class="agent-label">agent</span>{/if}{:else}（不明）{/if}：{eventText(ev.kind, ev.payload, titleOf)}
								{#if canRedact && !ev.redacted}<button class="btn sm ghost" onclick={() => (redact = { kind: 'event', id: ev.id, text: eventText(ev.kind, ev.payload, titleOf) })}>遮蔽</button>{/if}
							</li>
						{/each}
					</ul>
				{/if}
			{/if}
		</WikiLayout>
	{/if}
</ProjectFrame>

{#if redact && page}
	<WikiRedactDialog projectKey={key} pageId={page.id} target={redact} onclose={() => (redact = null)} ondone={() => ((redact = null), load())} />
{/if}

<style>
	.evs { list-style: none; margin: 0; padding: 0; }
</style>

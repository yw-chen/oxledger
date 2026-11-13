<script lang="ts">
	// Issue search (design 03 §3, §6): matched fields and highlights, a mark
	// for "only some characters matched", no total, "還有更多", truncated,
	// and search_lagging shown as "剛建立・同步中" rather than "nothing found".
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import type { SearchItem, SearchParams, SearchResult } from '$api/types';
	import { api } from '$lib/api';
	import { parse, toParams, SyntaxError_ } from '$lib/query';
	import { splitHighlights } from '$lib/highlight';
	import ErrorBox from '$lib/components/ErrorBox.svelte';
	import { pendingCreated, type JustCreated } from '$lib/created.svelte';
	import { projectByKey } from '$lib/session.svelte';
	import { searchToWhere } from '$lib/filter';

	let { scopeProject = null }: { scopeProject?: string | null } = $props();

	const url = $derived(page.url);
	let text = $state('');
	let items = $state<SearchItem[]>([]);
	let result = $state<SearchResult | null>(null);
	let error = $state<unknown>(null);
	let syntax = $state<SyntaxError_ | null>(null);
	let loading = $state(false);
	let slow = $state(false);
	let params: SearchParams = {};
	let justCreated = $state<JustCreated[]>([]);

	const FIELDS = ['key', 'subject', 'project', 'tracker', 'status', 'status_category', 'assignee', 'priority', 'is_private', 'updated_at'];

	function build(u: URL): SearchParams | null {
		const q = u.searchParams.get('q') ?? '';
		let p: SearchParams;
		try {
			p = toParams(parse(q), q);
			syntax = null;
		} catch (e) {
			if (e instanceof SyntaxError_) {
				syntax = e;
				return null;
			}
			throw e;
		}
		if (scopeProject) {
			if (p.project && p.project.toUpperCase() !== scopeProject.toUpperCase()) {
				// §18.6: another project's query leaves the project frame.
				goto(`/search?q=${encodeURIComponent(q)}`);
				return null;
			}
			p.project = scopeProject;
		}
		const a = u.searchParams.get('assignee');
		if (a) p.assignee = a;
		const s = u.searchParams.get('status');
		if (s) p.status_category = s;
		if (u.searchParams.get('overdue') === 'true') p.overdue = true;
		const after = u.searchParams.get('after');
		if (after) p.consistent_after = after;
		p.fields = FIELDS;
		p.explain = 'full';
		p.limit = 25;
		return p;
	}

	async function run(more = false) {
		error = null;
		loading = true;
		slow = false;
		const timer = setTimeout(() => (slow = true), 400);
		try {
			const r = await api('POST /issues/search', { body: more && result?.next_cursor ? { ...params, cursor: result.next_cursor } : params });
			items = more ? [...items, ...r.items] : r.items;
			result = r;
			justCreated = pendingCreated(items.map((i) => i.key)).filter((c) => !scopeProject || c.key.startsWith(scopeProject.toUpperCase() + '-'));
		} catch (e) {
			error = e;
			if (!more) items = [];
		} finally {
			clearTimeout(timer);
			loading = false;
		}
	}

	$effect(() => {
		text = url.searchParams.get('q') ?? '';
		const p = build(url);
		if (!p) {
			items = [];
			result = null;
			return;
		}
		params = p;
		run();
	});

	function submit(e: SubmitEvent) {
		e.preventDefault();
		const u = new URL(url);
		u.searchParams.set('q', text);
		u.searchParams.delete('after');
		goto(u.pathname + u.search, { keepFocus: true });
	}
	const FIELD: Record<string, string> = { subject: '標題', description: '描述' };
	const quick = $derived([
		{ label: '指派給我', key: 'assignee', value: 'me' },
		{ label: '未完成', key: 'status', value: 'open' },
		{ label: '逾期', key: 'overdue', value: 'true' }
	]);
	// 「存成視圖」 (contract 05 §6.1): the current search as a personal list
	// view; the structured part becomes conditions, the rest stays as text.
	let saving = $state<{ name: string } | null>(null);
	let saveError = $state<unknown>(null);
	let saveBusy = $state(false);
	let saveMsg = $state('');
	async function saveView() {
		if (!saving) return;
		saveBusy = true;
		saveError = null;
		saveMsg = '';
		const proj = params.project ? projectByKey(params.project.toUpperCase()) : null;
		if (params.project && !proj) {
			// Dropping the project would widen the view: say so instead.
			saveMsg = `找不到專案 ${params.project}，或你看不到它；這個查詢不能存成視圖。`;
			saveBusy = false;
			return;
		}
		const def = {
			kind: 'list',
			query: { where: searchToWhere(params, proj?.id ?? null), q: params.q || null },
			columns: ['key', 'subject', 'status', 'assignee', 'priority', 'due_date'],
			sort: [{ field: 'updated_at', dir: 'desc' }]
		};
		try {
			const v = await api('POST /views', { body: { scope: 'personal', kind: 'list', name: saving.name.trim(), def, project_id: proj?.id ?? null } });
			saving = null;
			goto(proj ? `/project/${proj.key}/views/${encodeURIComponent(v.id)}` : `/views/${encodeURIComponent(v.id)}`);
		} catch (e) {
			saveError = e;
		} finally {
			saveBusy = false;
		}
	}
	function toggle(k: string, v: string) {
		const u = new URL(url);
		if (u.searchParams.get(k) === v) u.searchParams.delete(k);
		else u.searchParams.set(k, v);
		goto(u.pathname + u.search, { keepFocus: true });
	}
</script>

<svelte:head><title>搜尋・oxledger</title></svelte:head>
<div class={scopeProject ? '' : 'page'}>
	{#if !scopeProject}<h1>議題</h1>{/if}
	<form class="inline-form" onsubmit={submit} style="margin:8px 0" role="search">
		<input type="search" bind:value={text} placeholder={scopeProject ? `在 ${scopeProject} 搜尋：status:open 登入 Safari` : 'project:AUTH status:open 登入 Safari'} style="flex:1;min-width:240px" aria-label="查詢" data-testid="search-input" />
		<button class="btn pri" type="submit">搜尋</button>
	</form>
	<div class="row" style="margin-bottom:10px">
		{#each quick as f (f.key)}
			<button class="btn sm" class:on={url.searchParams.get(f.key) === f.value} onclick={() => toggle(f.key, f.value)}>{f.label}</button>
		{/each}
		<span class="hint">語法：project: status: tracker: assignee: version:，其他字是全文。</span>
		<span style="flex:1"></span>
		{#if !syntax}<button class="btn sm" onclick={() => ((saving = { name: text.trim().slice(0, 60) || '新視圖' }), (saveError = null))} data-testid="save-as-view">存成視圖</button>{/if}
	</div>
	{#if syntax}
		<div class="err" role="alert" data-testid="syntax-error">
			<b>查詢語法有誤（第 {syntax.position + 1} 個字）：{syntax.message}</b>
			<div class="mono" style="margin-top:4px">{Array.from(text).slice(0, syntax.position).join('')}<mark>{Array.from(text)[syntax.position] ?? '␣'}</mark>{Array.from(text).slice(syntax.position + 1).join('')}</div>
		</div>
	{/if}
	<ErrorBox {error} onaction={() => run()} />
	{#if loading && slow}
		<div class="sync" data-testid="search-slow"><span class="spin"></span>搜尋中…（條件很多時可以縮小範圍：加上 project: 或 status:）</div>
	{/if}
	{#each justCreated as c (c.key)}
		<div class="card p" data-testid="just-created">
			<div class="row" style="gap:8px"><span class="chip c-prop">剛建立・同步中</span><a class="key" href="/issue/{c.key}">{c.key}</a><b>{c.subject}</b></div>
			<p class="hint" style="margin:4px 0 0">你剛建立的單可能還沒進搜尋索引，或不符合目前的條件；單本身已經可以開啟。</p>
		</div>
	{/each}
	{#if result?.outcome === 'search_lagging'}
		<div class="card p" data-testid="lagging">
			<b>剛建立・搜尋同步中</b>
			<p class="muted" style="margin:4px 0;font-size:13px">剛寫入的內容還沒進搜尋索引；這不是寫入失敗，也不是「找不到」。單本身已經可以開啟。</p>
			<button class="btn sm" onclick={() => run()}>再找一次</button>
		</div>
	{:else if result}
		{#if result.truncated}
			<div class="banner w" data-testid="truncated">結果太多，系統只看過前面一部分候選；可能還有更多相符的單。請加上條件縮小範圍。</div>
		{/if}
		{#each items as it (it.key)}
			<div class="card" style="padding:8px 12px" data-testid="hit">
				<div class="row" style="gap:8px">
					<a class="key" href="/issue/{it.key}">{it.key}</a>
					<b>{it.subject}</b>
					{#if it.is_private}<span class="chip c-warn">私有</span>{/if}
					<span class="chip c-dim">{it.status}</span>
					{#if it.match}
						<span class="hint">命中：{it.match.fields.map((f) => FIELD[f] ?? f).join('、')}</span>
						{#if it.match.strength === 'fallback'}<span class="chip c-prop" title="只有個別字元或識別字片段相符" data-testid="fallback">部分字元相符</span>{/if}
					{/if}
				</div>
				{#each it.evidence as ev (ev.citation_id)}
					<div class="hint" style="margin-top:3px">
						<span class="k">{FIELD[ev.field] ?? ev.field}</span>
						{#each splitHighlights(ev.text, ev.highlights) as part, i (i)}
							{#if part.mark}<mark class:f={part.mark === 'fallback'}>{part.text}</mark>{:else}{part.text}{/if}
						{/each}
					</div>
				{/each}
				{#if !it.match}
					<div class="hint">{it.project}・{it.tracker}・指派：{it.assignee ?? '—'}{it.priority ? `・${it.priority}` : ''}</div>
				{/if}
			</div>
		{:else}
			<div class="empty">沒有符合的議題。</div>
		{/each}
		{#if result.next_cursor}
			<div class="pager"><button class="btn" onclick={() => run(true)} disabled={loading} data-testid="more">還有更多</button><span class="hint">不顯示總數</span></div>
		{/if}
	{/if}
</div>

{#if saving}
	<div class="modal-bg" role="presentation">
		<div class="modal" role="dialog" aria-modal="true" aria-label="存成視圖" data-testid="save-view-form">
			<h3>存成視圖</h3>
			<label class="f"><span>名稱</span><input type="text" bind:value={saving.name} maxlength="60" data-testid="sv-name" /></label>
			<div class="hint">存成只有你看得到的個人列表視圖；之後可以在視圖裡改條件、欄位與排序。</div>
			{#if saveMsg}<div class="err" role="alert" data-testid="sv-msg">{saveMsg}</div>{/if}
			<ErrorBox error={saveError} />
			<div class="row" style="gap:8px;margin-top:10px">
				<button class="btn pri" onclick={saveView} disabled={saveBusy || !saving.name.trim()} data-testid="sv-submit">{saveBusy ? '儲存中…' : '儲存'}</button>
				<button class="btn" onclick={() => (saving = null)}>取消</button>
			</div>
		</div>
	</div>
{/if}

<script lang="ts">
	// Design 06 §6.2: the editor for descriptions, notes and wiki pages.
	// Write / preview tabs (side by side at 1280 px and wider), a toolbar,
	// @mention autocomplete from the project's assignable users (R-RF3), the
	// chips of who will be mentioned, a byte limit, drafts and a leave guard.
	import { onMount, untrack } from 'svelte';
	import { beforeNavigate } from '$app/navigation';
	import { api, type Route } from '#lib/api.js';
	import RichText from './RichText.svelte';
	import {
		bytesText,
		codeBlock,
		dropDraft,
		filterPeople,
		insertMention,
		mentionChips,
		mentionQuery,
		prefixLines,
		readDraft,
		table,
		utf8Bytes,
		wrapSel,
		writeDraft,
		type Edit,
		type Mentionable
	} from '#lib/markdown.js';

	let {
		value = $bindable(''),
		people = [],
		context = null,
		limit,
		over = $bindable(false),
		dirty = $bindable(false),
		notice = null,
		draft = null,
		guard = true,
		disabled = false,
		placeholder = '',
		id,
		label = '內容',
		testid = 'md',
		inputTestid,
		base
	}: {
		value: string;
		/** The project's assignable users (`GET /projects/{id}/assignees`). */
		people?: Mentionable[];
		/** What the preview resolves references against (R-RF6). */
		context?: { issue: string } | { project: string } | { wiki_project: string } | null;
		/** Bytes (§6.2: notes 64 KiB, wiki 512 KiB). */
		limit: number;
		/** True while over the limit: the parent disables its send button. */
		over?: boolean;
		/** True once the text differs from what it was when the editor opened. */
		dirty?: boolean;
		/** R-M9 / R-M3 banners. */
		notice?: 'plain' | 'agent' | null;
		/** localStorage key for the draft (`draftKey(user, target)`), or none. */
		draft?: string | null;
		/** Ask before leaving with unsent text. */
		guard?: boolean;
		disabled?: boolean;
		placeholder?: string;
		id?: string;
		label?: string;
		testid?: string;
		/** The textarea's test id (default `{testid}-input`). */
		inputTestid?: string;
		/** What counts as unchanged (the saved text). The parent moves it when it
		 *  loads a newer version; default: the text the editor opened with. */
		base?: string;
	} = $props();

	const opened = value;
	const start = $derived(base ?? opened);
	let ta = $state<HTMLTextAreaElement | null>(null);
	let tab = $state<'write' | 'preview'>('write');
	let split = $state(false);
	let wide = $state(false);
	let restorable = $state<string | null>(null);

	const used = $derived(utf8Bytes(value));
	$effect(() => {
		over = used > limit;
	});
	$effect(() => {
		dirty = value !== start;
	});
	const chips = $derived(mentionChips(value, people));
	const noLogin = $derived(people.filter((p) => p.kind !== 'group' && !p.login).length);

	onMount(() => {
		const mq = matchMedia('(min-width: 1280px)');
		const set = () => (wide = mq.matches);
		set();
		mq.addEventListener('change', set);
		if (draft) {
			const d = readDraft(draft);
			if (d && d !== value) restorable = d;
		}
		const unload = (e: BeforeUnloadEvent) => {
			if (guard && dirty && !disabled) e.preventDefault();
		};
		addEventListener('beforeunload', unload);
		return () => {
			mq.removeEventListener('change', set);
			removeEventListener('beforeunload', unload);
		};
	});
	beforeNavigate((nav) => {
		// Leaving the app (another site, closing): beforeunload asks, once.
		if (!guard || !dirty || disabled || nav.type === 'leave' || nav.willUnload) return;
		if (!confirm('有還沒送出的內容，確定要離開嗎？（草稿會保留在這台電腦）')) nav.cancel();
	});

	// Drafts: written as the person types (debounced), dropped when the parent
	// clears the text after sending.
	$effect(() => {
		const v = value;
		const key = draft;
		const s = start;
		if (!key || restorable !== null) return;
		// Back to the saved text: no draft. The timer dies with the editor, so a
		// text sent and cleared is never written back after the parent dropped it.
		const t = setTimeout(() => (v === s ? dropDraft(key) : writeDraft(key, v)), 400);
		return () => clearTimeout(t);
	});
	function restore() {
		value = restorable ?? value;
		restorable = null;
	}
	function discard() {
		if (draft) dropDraft(draft);
		restorable = null;
	}

	// Toolbar: each edit keeps the textarea focused with the new selection.
	function apply(fn: (t: string, s: number, e: number) => Edit) {
		if (!ta || disabled) return;
		const r = fn(value, ta.selectionStart, ta.selectionEnd);
		value = r.text;
		const el = ta;
		queueMicrotask(() => {
			el.focus();
			el.setSelectionRange(r.selStart, r.selEnd);
		});
	}
	const TOOLS: { label: string; title: string; key?: string; run: (t: string, s: number, e: number) => Edit }[] = [
		{ label: 'B', title: '粗體（Ctrl+B）', key: 'b', run: (t, s, e) => wrapSel(t, s, e, '**', '**', '粗體') },
		{ label: 'I', title: '斜體（Ctrl+I）', key: 'i', run: (t, s, e) => wrapSel(t, s, e, '*', '*', '斜體') },
		{ label: 'H', title: '插入標題', run: (t, s, e) => prefixLines(t, s, e, '### ') },
		{ label: '•', title: '清單', run: (t, s, e) => prefixLines(t, s, e, '- ') },
		{ label: '1.', title: '編號清單', run: (t, s, e) => prefixLines(t, s, e, 'n. ') },
		{ label: '☐', title: '工作清單', run: (t, s, e) => prefixLines(t, s, e, '- [ ] ') },
		{ label: '❝', title: '引用', run: (t, s, e) => prefixLines(t, s, e, '> ') },
		{ label: '</>', title: '程式碼', run: (t, s, e) => (t.slice(s, e).includes('\n') ? codeBlock(t, s, e) : wrapSel(t, s, e, '`', '`', '程式碼')) },
		{ label: '🔗', title: '連結（Ctrl+K）', key: 'k', run: (t, s, e) => wrapSel(t, s, e, '[', '](https://)', '連結文字') },
		{ label: '▦', title: '表格', run: (t, s) => table(t, s) }
	];
	let help = $state(false);

	// @mention autocomplete.
	let ac = $state<{ start: number; caret: number; items: Mentionable[]; i: number } | null>(null);
	function scan() {
		if (!ta) return;
		const caret = ta.selectionStart;
		const m = ta.selectionStart === ta.selectionEnd ? mentionQuery(value, caret) : null;
		const items = m ? filterPeople(people, m.q) : [];
		ac = m && items.length ? { start: m.start, caret, items, i: 0 } : null;
	}
	function pick(p: Mentionable) {
		if (!ac || !ta || !p.login) return;
		const r = insertMention(value, ac.start, ac.caret, p.login);
		value = r.text;
		ac = null;
		const el = ta;
		queueMicrotask(() => {
			el.focus();
			el.setSelectionRange(r.caret, r.caret);
		});
	}
	function keydown(e: KeyboardEvent) {
		if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey) {
			const t = TOOLS.find((x) => x.key === e.key.toLowerCase());
			if (t) {
				e.preventDefault();
				apply(t.run);
				return;
			}
		}
		if (!ac) return;
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			const n = ac.items.length;
			ac.i = (ac.i + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
		} else if (e.key === 'Enter' || e.key === 'Tab') {
			const p = ac.items[ac.i];
			if (p?.login) {
				e.preventDefault();
				pick(p);
			}
		} else if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			ac = null;
		}
	}

	// Preview (§6.2): 500 ms after typing stops, while the preview shows.
	const showPreview = $derived(tab === 'preview' || (split && wide));
	let preview = $state<{ html: string | null; failed: boolean; loading: boolean }>({ html: null, failed: false, loading: false });
	let seq = 0;
	$effect(() => {
		if (!showPreview) return;
		const text = value;
		const ctx = context;
		const my = ++seq;
		if (!text.trim()) {
			preview = { html: '', failed: false, loading: false };
			return;
		}
		untrack(() => (preview.loading = true));
		const t = setTimeout(async () => {
			try {
				// The endpoint arrives with the backend's M2.1 (not in the generated types yet).
				const r = (await api('POST /markdown/preview' as Route, { body: { text, context: ctx } as never })) as unknown as { html?: string };
				if (my === seq) preview = { html: typeof r?.html === 'string' ? r.html : null, failed: typeof r?.html !== 'string', loading: false };
			} catch {
				if (my === seq) preview = { html: null, failed: true, loading: false };
			}
		}, 500);
		return () => clearTimeout(t);
	});
</script>

<div class="mde" class:split={split && wide} data-testid={testid}>
	{#if notice === 'plain'}<div class="banner w" data-testid="{testid}-plain">這段說明原本是純文字。儲存後會以 Markdown 顯示，請先看預覽。</div>{/if}
	{#if notice === 'agent'}<div class="banner w" data-testid="{testid}-agent">這段內容含 agent 寫入的連結。儲存後會以你的名義顯示，連結不再有額外的確認保護，請先檢查連結。</div>{/if}
	{#if restorable !== null}
		<div class="banner" data-testid="{testid}-draft">有未送出的草稿<span class="sp"></span><button class="btn sm" onclick={restore} data-testid="{testid}-draft-restore">還原</button><button class="btn sm ghost" onclick={discard} data-testid="{testid}-draft-discard">捨棄</button></div>
	{/if}
	<div class="bar">
		<div class="tabs" role="tablist" aria-label={label}>
			<button role="tab" aria-selected={tab === 'write'} class:on={tab === 'write'} onclick={() => (tab = 'write')} data-testid="{testid}-tab-write">編輯</button>
			<button role="tab" aria-selected={tab === 'preview'} class:on={tab === 'preview'} onclick={() => (tab = 'preview')} data-testid="{testid}-tab-preview">預覽</button>
		</div>
		<div class="tools" role="toolbar" aria-label="格式">
			{#each TOOLS as t (t.title)}<button type="button" class="btn sm ghost" title={t.title} aria-label={t.title} disabled={disabled || tab === 'preview'} onclick={() => apply(t.run)}>{t.label}</button>{/each}
			<button type="button" class="btn sm ghost" onclick={() => (help = !help)} aria-expanded={help} data-testid="{testid}-help">說明</button>
			{#if wide}<button type="button" class="btn sm ghost" class:pri={split} onclick={() => (split = !split)} aria-pressed={split} title="編輯與預覽左右並排">並排</button>{/if}
		</div>
	</div>
	<div class="panes">
		{#if tab === 'write' || (split && wide)}
			<div class="edit">
				<textarea
					bind:this={ta}
					bind:value
					{id}
					{disabled}
					{placeholder}
					aria-label={label}
					oninput={scan}
					onkeydown={keydown}
					onclick={scan}
					onblur={() => setTimeout(() => (ac = null), 150)}
					data-testid={inputTestid ?? `${testid}-input`}
				></textarea>
				{#if ac}
					<ul class="ac" role="listbox" aria-label="提及" data-testid="{testid}-ac">
						{#each ac.items as p, i (p.id)}
							<li role="option" aria-selected={i === ac.i} aria-disabled={!p.login} class:on={i === ac.i} class:off={!p.login}>
								<button type="button" disabled={!p.login} onmousedown={(e) => { e.preventDefault(); pick(p); }} title={p.login ? '' : '伺服器還沒提供這個人的帳號名稱，暫時不能提及'} data-testid="{testid}-ac-item">{p.name}{#if p.login}<span class="dim"> @{p.login}</span>{/if}</button>
							</li>
						{/each}
					</ul>
				{/if}
			</div>
		{/if}
		{#if showPreview}
			<div class="prev" data-testid="{testid}-preview" aria-live="polite">
				{#if preview.failed}<div class="hint" data-testid="{testid}-preview-failed">無法預覽，內容沒有遺失。</div>
				{:else if preview.loading && preview.html === null}<div class="hint">預覽中…</div>
				{:else if !value.trim()}<div class="hint">沒有內容。</div>
				{:else}<RichText html={preview.html} text={value} />{/if}
			</div>
		{/if}
	</div>
	<div class="foot">
		{#if chips.length}
			<span class="dim">會提及：</span>
			{#each chips as p (p.id)}<span class="chip c-dim mention" title="@{p.login}" aria-label="提及 {p.name}" data-testid="{testid}-chip">@{p.name}</span>{/each}
		{/if}
		{#if noLogin && value.includes('@')}<span class="hint" data-testid="{testid}-nologin">有 {noLogin} 人暫時不能從清單提及（伺服器還沒提供帳號名稱）</span>{/if}
		<span class="sp"></span>
		<span class="count" class:bad={over} data-testid="{testid}-count">{bytesText(used, limit)}{over ? '・超過上限' : ''}</span>
	</div>
	{#if help}
		<div class="cheat card" data-testid="{testid}-cheat">
			<b>語法小抄</b>
			<div class="mono">**粗體**　*斜體*　`程式碼`　[文字](https://網址)</div>
			<div class="mono"># 標題　- 清單　1. 編號　- [ ] 待辦　&gt; 引用</div>
			<div class="mono">```（程式碼區塊）```　| 表 | 格 |</div>
			<div class="hint">輸入 @ 加兩個字可以提及本專案可指派的人；寫 AUTH-12 這樣的單號會變成連結。HTML 標籤會照原樣顯示成文字。</div>
		</div>
	{/if}
</div>

<style>
	.mde { display: flex; flex-direction: column; gap: 4px; }
	.bar { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; }
	.bar .tabs { display: flex; gap: 2px; }
	.bar .tabs button { background: none; border: 0; border-bottom: 2px solid transparent; padding: 3px 8px; color: var(--muted); cursor: pointer; }
	.bar .tabs button.on { border-bottom-color: var(--accent, currentColor); color: inherit; }
	.tools { display: flex; gap: 2px; overflow-x: auto; flex: 1; min-width: 0; }
	.panes { display: flex; gap: 8px; }
	.edit, .prev { flex: 1; min-width: 0; position: relative; }
	.edit textarea { width: 100%; min-height: 140px; resize: vertical; box-sizing: border-box; }
	.prev { min-height: 140px; border: 1px solid var(--line); border-radius: 7px; padding: 8px 10px; overflow: auto; }
	.ac { position: absolute; z-index: 30; left: 8px; bottom: -4px; transform: translateY(100%); list-style: none; margin: 0; padding: 4px; background: var(--bg2); border: 1px solid var(--line2); border-radius: 7px; min-width: 200px; max-height: 240px; overflow: auto; }
	.ac li button { display: block; width: 100%; text-align: left; background: none; border: 0; padding: 4px 8px; border-radius: 5px; color: inherit; cursor: pointer; }
	.ac li.on button { background: var(--bg3, var(--line)); }
	.ac li.off button { opacity: 0.55; cursor: not-allowed; }
	.foot { display: flex; flex-wrap: wrap; align-items: center; gap: 4px; font-size: 12px; }
	.foot .sp, .banner .sp { flex: 1; }
	.count { color: var(--muted); }
	.count.bad { color: var(--bad, #d33); font-weight: 600; }
	.cheat { font-size: 12px; display: flex; flex-direction: column; gap: 3px; }
	.mono { font-family: var(--mono, ui-monospace, monospace); }
	@media (max-width: 1279.98px) { .panes { flex-direction: column; } }
</style>

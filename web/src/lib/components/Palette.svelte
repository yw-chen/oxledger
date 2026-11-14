<script lang="ts">
	// ⌘K intent bar (design 01 §15.11). Routes only on Enter or click, never
	// while typing; ignores Enter during IME composition.
	import { goto } from '$app/navigation';
	import { route, type Intent } from '#lib/intent.js';

	let { open = $bindable(false) }: { open?: boolean } = $props();
	let text = $state('');
	let input: HTMLTextAreaElement | undefined = $state();
	let intent = $state<Intent | null>(null);
	const hint = $derived(text.trim() ? route(text) : null);

	$effect(() => {
		if (open) {
			intent = null;
			queueMicrotask(() => input?.focus());
		}
	});

	function go(href: string | null) {
		if (!href) return;
		open = false;
		text = '';
		goto(href);
	}
	function submit() {
		const r = route(text);
		intent = r;
		if (r?.href && r.kind !== 'clarify') go(r.href);
	}
	function onkey(e: KeyboardEvent) {
		if (e.key === 'Escape') open = false;
		if (e.key === 'Enter' && !e.shiftKey) {
			if (e.isComposing || e.keyCode === 229) return;
			e.preventDefault();
			submit();
		}
	}
	const KIND_LABEL: Record<string, string> = {
		issue_open: '開啟議題',
		structured_search: '條件查詢',
		command_draft: '指令',
		text_search: '文字搜尋',
		clarify: '請選擇',
		ai_task: 'AI'
	};
	const shown = $derived(intent ?? hint);
</script>

{#if open}
	<div class="pal" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) open = false; }}>
		<div class="box" role="dialog" aria-label="意圖列" aria-modal="true">
			<textarea
				bind:this={input}
				bind:value={text}
				onkeydown={onkey}
				rows="1"
				placeholder="AUTH-123、project:AUTH status:open 登入、/new，或描述你要找的東西"
				aria-label="搜尋或輸入指令"
				data-testid="palette-input"
				style="border:0;border-bottom:1px solid var(--line);border-radius:0;padding:12px 14px;font-size:15px;min-height:46px;resize:none"
			></textarea>
			{#if shown}
				<div class="opt" style="cursor:default" data-testid="palette-kind" data-kind={shown.kind}>
					<span class="chip c-act">{KIND_LABEL[shown.kind]}</span>
					<span class="muted" style="font-size:13px">{shown.note ?? (shown.href ? '按 Enter 前往' : '')}</span>
				</div>
				{#if intent?.choices}
					{#each intent.choices as c (c.label)}
						<button class="opt" style="width:100%;background:none;border:0;border-bottom:1px solid var(--line);text-align:left" disabled={!!c.disabled} onclick={() => go(c.href)}>
							{c.label}{#if c.disabled}<span class="rk">{c.disabled}</span>{/if}
						</button>
					{/each}
				{/if}
			{:else}
				<div class="opt hint" style="cursor:default">Enter 送出・Esc 關閉・輸入法組字中 Enter 只完成組字</div>
			{/if}
		</div>
	</div>
{/if}

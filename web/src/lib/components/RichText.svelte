<script lang="ts">
	// Design 06 §6.1: shows a description, note or wiki page. The HTML is the
	// server's (`*_html`, sanitized there, R-M2/R-M5); this is the one component
	// allowed to put it into {@html}, and only `*_html` fields (R-M10). Without
	// it (a server before M2.1) the text shows as it is, like M1.
	import { spaWikiHref } from '#lib/wiki.js';
	let {
		html = null,
		text = '',
		agent = false,
		empty = '',
		testid
	}: {
		/** An API `*_html` field. */
		html?: string | null;
		/** The source text, shown when there is no HTML. */
		text?: string | null;
		/** Written by an agent (R-M3): external links open only after a confirmation. */
		agent?: boolean;
		empty?: string;
		testid?: string;
	} = $props();

	let box = $state<HTMLElement | null>(null);
	let leaving = $state<{ host: string; href: string } | null>(null);

	// After each render: a copy button on code blocks; in agent content the
	// address leaves `href`, so a middle click cannot skip the confirmation.
	$effect(() => {
		void html;
		const el = box;
		if (!el) return;
		const isAgent = agent || !!el.closest('[data-ox-agent]') || !!el.querySelector('[data-ox-agent]');
		for (const pre of el.querySelectorAll('pre')) {
			if (pre.querySelector(':scope > .ox-copy')) continue;
			const b = document.createElement('button');
			b.type = 'button';
			b.className = 'ox-copy btn sm ghost';
			b.textContent = '複製';
			b.setAttribute('aria-label', '複製程式碼');
			b.addEventListener('click', () => {
				const code = pre.querySelector('code')?.textContent ?? pre.textContent ?? '';
				navigator.clipboard?.writeText(code).then(
					() => (b.textContent = '已複製'),
					() => (b.textContent = '無法複製')
				);
				setTimeout(() => (b.textContent = '複製'), 1500);
			});
			pre.appendChild(b);
		}
		if (isAgent) {
			// External links and linked external images; one already moved stays moved.
			for (const a of el.querySelectorAll<HTMLAnchorElement>('a.ox-ext[href], a.ox-img-ext[href]')) {
				if (a.dataset.oxHref !== undefined) continue;
				a.dataset.oxHref = a.getAttribute('href') ?? '';
				a.setAttribute('href', '#');
				a.removeAttribute('target');
			}
		}
		// Wiki links (design 06 §3.2) name `/projects/KEY/wiki/…`; the SPA's routes are `/project/…`.
		for (const a of el.querySelectorAll<HTMLAnchorElement>('a.ox-wiki[href]')) {
			const to = spaWikiHref(a.getAttribute('href') ?? '');
			if (to) a.setAttribute('href', to);
		}
		for (const a of el.querySelectorAll<HTMLAnchorElement>('a.ox-issue[title]')) {
			if (!a.hasAttribute('aria-label')) a.setAttribute('aria-label', `${a.textContent ?? ''} ${a.title}`);
		}
		for (const s of el.querySelectorAll<HTMLElement>('.ox-mention')) {
			if (!s.hasAttribute('aria-label')) s.setAttribute('aria-label', `提及 ${(s.textContent ?? '').replace(/^@/, '')}`);
		}
	});

	function click(e: MouseEvent) {
		const a = (e.target as HTMLElement).closest('a.ox-ext, a.ox-img-ext') as HTMLAnchorElement | null;
		if (!a || a.dataset.oxHref === undefined) return;
		e.preventDefault();
		const href = a.dataset.oxHref;
		if (!/^https?:\/\//i.test(href)) return;
		leaving = { host: a.dataset.oxHost ?? new URL(href).host, href };
	}
</script>

{#if html}
	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
	<div class="md" bind:this={box} onclick={click} data-testid={testid} data-agent={agent ? '' : undefined}>{@html html}</div>
{:else}
	<div class="pre" data-testid={testid}>{text || empty}</div>
{/if}

{#if leaving}
	<div class="modal-bg">
		<div class="modal" role="dialog" aria-modal="true" aria-label="前往外部網站" data-testid="rt-leave">
			<h3>即將前往 {leaving.host}</h3>
			<p class="muted" style="font-size:13px;word-break:break-all">網址：{leaving.href}</p>
			<div class="row" style="justify-content:flex-end;gap:6px">
				<button class="btn" onclick={() => (leaving = null)}>取消</button>
				<button class="btn pri" onclick={() => { window.open(leaving!.href, '_blank', 'noopener,noreferrer'); leaving = null; }} data-testid="rt-leave-go">前往</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.md { line-height: 1.6; overflow-wrap: anywhere; }
	.md :global(h1) { font-size: 18px; margin: 12px 0 6px; }
	.md :global(h2) { font-size: 16px; margin: 10px 0 6px; }
	.md :global(h3), .md :global(h4), .md :global(h5), .md :global(h6) { font-size: 14px; margin: 8px 0 4px; }
	.md :global(p) { margin: 0 0 8px; }
	.md :global(ul), .md :global(ol) { margin: 0 0 8px; padding-left: 22px; }
	.md :global(blockquote) { margin: 0 0 8px; padding-left: 10px; border-left: 3px solid var(--line2); color: var(--muted); }
	.md :global(table) { display: block; overflow-x: auto; border-collapse: collapse; margin: 0 0 8px; }
	.md :global(th), .md :global(td) { border: 1px solid var(--line); padding: 3px 8px; }
	.md :global(pre) { position: relative; overflow-x: auto; background: var(--bg2); border: 1px solid var(--line); border-radius: 7px; padding: 8px 10px; margin: 0 0 8px; }
	.md :global(code) { font-family: var(--mono, ui-monospace, monospace); font-size: 0.92em; }
	.md :global(.ox-copy) { position: absolute; top: 4px; right: 4px; }
	.md :global(input[type='checkbox']) { margin-right: 4px; }
	.md :global(a.ox-closed) { text-decoration: line-through; }
	.md :global(.ox-mention) { display: inline-block; padding: 0 6px; border-radius: 10px; background: var(--bg3, var(--bg2)); border: 1px solid var(--line2); font-size: 0.92em; }
	.md :global(a.ox-ext[data-ox-host])::after { content: '（' attr(data-ox-host) '）'; font-size: 11px; color: var(--muted); }
	.md :global(img) { max-width: 100%; }
	.md :global(a.ox-wiki-new) { color: var(--bad, #d33); }
</style>

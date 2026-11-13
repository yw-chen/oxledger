// Design 06 R-M10 asks for a lint rule: {@html} only in <RichText>, and only
// with `*_html` API fields. The project has no ESLint (decided), so this test
// scans the sources instead.
import { describe, expect, it } from 'vitest';

// Every component's source, keyed `lib/components/X.svelte`, `routes/…`.
const raw = import.meta.glob('/src/**/*.svelte', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;
const sources = Object.entries(raw).map(([k, src]) => [k.replace(/^\/src\//, ''), src] as const);

/** Where {@html} may appear, and what it may render there. */
const ALLOWED: Record<string, RegExp> = {
	// The server's sanitized HTML (`*_html` fields reach it through the `html` prop).
	'lib/components/RichText.svelte': /^html$/,
	// The AI chat's own Markdown subset, escaped on every path (aichat.test.ts).
	'lib/components/AiChat.svelte': /^renderMd\(/
};

describe('{@html} guard (R-M10)', () => {
	it('appears only in RichText and the AI chat', () => {
		expect(sources.length).toBeGreaterThan(50);
		const found: string[] = [];
		for (const [rel, src] of sources) {
			for (const m of src.matchAll(/\{@html\s+([^}]*)\}/g)) {
				const ok = ALLOWED[rel]?.test(m[1].trim());
				if (!ok) found.push(`${rel}: {@html ${m[1].trim()}}`);
			}
		}
		expect(found).toEqual([]);
	});
	it('RichText receives only `*_html` fields', () => {
		const bad: string[] = [];
		for (const [rel, src] of sources) {
			for (const m of src.matchAll(/<RichText\b[^>]*?\bhtml=\{([^}]*)\}/g)) {
				const expr = m[1].trim();
				// A field read ending in `_html`, or the preview's own `html` result.
				if (!/(_html|preview\.html)(\s*\?\?\s*null)?$/.test(expr)) bad.push(`${rel}: html={${expr}}`);
			}
		}
		expect(bad).toEqual([]);
	});
});

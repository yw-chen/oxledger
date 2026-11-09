// SPDX-License-Identifier: GPL-2.0-or-later
// Splits a snippet into plain and highlighted parts. Highlight ranges are in
// code points, half-open (design 03 §3.6), so this never cuts a surrogate pair.

import type { Highlight } from '$api/types';

export type Part = { text: string; mark: null | 'primary' | 'fallback' };

export function splitHighlights(text: string, hs: Highlight[]): Part[] {
	const cps = Array.from(text);
	const ranges = [...hs]
		.filter((h) => h.start < h.end && h.start >= 0)
		.sort((a, b) => a.start - b.start)
		.map((h) => ({ start: Math.min(h.start, cps.length), end: Math.min(h.end, cps.length), strength: h.strength }));
	const out: Part[] = [];
	let at = 0;
	for (const r of ranges) {
		const start = Math.max(r.start, at);
		if (start >= r.end) continue;
		if (start > at) out.push({ text: cps.slice(at, start).join(''), mark: null });
		out.push({ text: cps.slice(start, r.end).join(''), mark: r.strength });
		at = r.end;
	}
	if (at < cps.length) out.push({ text: cps.slice(at).join(''), mark: null });
	return out;
}

// SPDX-License-Identifier: GPL-2.0-or-later
// 右欄 AI 對話 (contract 05 §6.19, §3.13): the SSE reader, the markdown
// subset the replies are drawn with, page references and the fixed texts.
// Pure functions, unit-tested in aichat.test.ts.

import type { PageRef } from '$api/types';

// ------------------------------------------------------------------ SSE

export type SseEvent = { id: number | null; event: string; data: string };

/**
 * Feeds text into an SSE parser (`id:`, `event:`, `data:` lines, a blank
 * line ends an event, `:` lines are comments such as the 15 s ping).
 * Returns the complete events and keeps the unfinished tail.
 */
export function sseParser() {
	let buf = '';
	let id: number | null = null;
	let event = '';
	let data: string[] = [];
	return (chunk: string): SseEvent[] => {
		buf += chunk;
		const out: SseEvent[] = [];
		let nl: number;
		while ((nl = buf.search(/\r\n|\n|\r/)) >= 0) {
			// A lone \r at the end may be the first half of \r\n: wait for more.
			if (buf[nl] === '\r' && nl === buf.length - 1) break;
			const line = buf.slice(0, nl);
			buf = buf.slice(nl + (buf.startsWith('\r\n', nl) ? 2 : 1));
			if (line === '') {
				if (data.length || event) out.push({ id, event: event || 'message', data: data.join('\n') });
				event = '';
				data = [];
				continue;
			}
			if (line.startsWith(':')) continue;
			const c = line.indexOf(':');
			const field = c < 0 ? line : line.slice(0, c);
			let value = c < 0 ? '' : line.slice(c + 1);
			if (value.startsWith(' ')) value = value.slice(1);
			if (field === 'id') id = /^\d+$/.test(value) ? Number(value) : id;
			else if (field === 'event') event = value;
			else if (field === 'data') data.push(value);
		}
		return out;
	};
}

// ------------------------------------------------------------------ markdown subset

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');

/** §6.19 / appendix D V23: links longer than 200 or with a query over 100 characters stay plain text. */
export function linkTarget(url: string, origin: string): { href: string; external: string | null } | null {
	let u: URL;
	try {
		u = new URL(url, origin);
	} catch {
		return null;
	}
	if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
	if (url.length > 200 || u.search.length - 1 > 100) return null;
	const same = u.origin === origin;
	return { href: u.href, external: same ? null : u.host };
}

const ISSUE_KEY = /\b([A-Z][A-Z0-9_]{0,9}-\d+)\b/g;

/** Inline: code, links (images are never drawn), bold, issue keys; everything else escaped. */
function inline(s: string, origin: string): string {
	const out: string[] = [];
	// Split out code spans first: nothing inside them is formatted.
	const parts = s.split(/(`[^`\n]+`)/);
	for (const p of parts) {
		if (/^`[^`\n]+`$/.test(p)) {
			out.push(`<code>${esc(p.slice(1, -1))}</code>`);
			continue;
		}
		let rest = p;
		let html = '';
		const re = /(!?)\[([^\]\n]*)\]\(([^)\s]+)\)/;
		let m: RegExpExecArray | null;
		while ((m = re.exec(rest))) {
			html += text(rest.slice(0, m.index));
			const [, bang, label, url] = m;
			if (bang) html += text(label ? `〔圖片：${label}〕` : '〔圖片〕');
			else {
				const t = linkTarget(url, origin);
				if (!t) html += text(`${label} (${url})`);
				// The real address stays out of href: a middle click or "open in
				// new tab" must not skip the confirmation (V23).
				else if (t.external) html += `<a href="#" data-ext="${esc(t.external)}" data-href="${esc(t.href)}" rel="noopener noreferrer">${plain(label || t.external)}</a>〔${esc(t.external)}〕`;
				else html += `<a href="${esc(new URL(t.href).pathname + new URL(t.href).search + new URL(t.href).hash)}">${plain(label || t.href)}</a>`;
			}
			rest = rest.slice(m.index + m[0].length);
		}
		html += text(rest);
		out.push(html);
	}
	return out.join('');
}
/** Escaped with **bold**; no links (a link's own label). */
function plain(s: string): string {
	return esc(s).replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
}
/** Plain text: escaped, **bold**, issue keys as links. */
function text(s: string): string {
	return plain(s).replace(ISSUE_KEY, '<a href="/issue/$1">$1</a>');
}

/**
 * The §6.19 markdown subset as HTML: paragraphs, lists, bold, inline code,
 * tables; never raw HTML, never images; http(s) links only, external ones
 * marked with their host and opened only after a confirmation (`data-ext`).
 */
export function renderMd(src: string, origin: string): string {
	const lines = src.replace(/\r\n?/g, '\n').split('\n');
	const out: string[] = [];
	let i = 0;
	const isRow = (l: string) => /^\s*\|.*\|\s*$/.test(l);
	const cells = (l: string) => l.trim().slice(1, -1).split('|').map((c) => c.trim());
	while (i < lines.length) {
		const l = lines[i];
		if (!l.trim()) {
			i++;
			continue;
		}
		if (/^```/.test(l)) {
			const body: string[] = [];
			i++;
			while (i < lines.length && !/^```/.test(lines[i])) body.push(lines[i++]);
			i++;
			out.push(`<pre><code>${esc(body.join('\n'))}</code></pre>`);
			continue;
		}
		if (isRow(l) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
			const head = cells(l);
			i += 2;
			const rows: string[][] = [];
			while (i < lines.length && isRow(lines[i])) rows.push(cells(lines[i++]));
			out.push(`<table><thead><tr>${head.map((h) => `<th>${inline(h, origin)}</th>`).join('')}</tr></thead><tbody>${rows.map((r) => `<tr>${r.map((c) => `<td>${inline(c, origin)}</td>`).join('')}</tr>`).join('')}</tbody></table>`);
			continue;
		}
		const li = /^\s*(?:[-*]|(\d+)\.)\s+(.*)$/;
		if (li.test(l)) {
			const ordered = !!li.exec(l)![1];
			const items: string[] = [];
			while (i < lines.length && li.test(lines[i]) && !!li.exec(lines[i])![1] === ordered) items.push(li.exec(lines[i++])![2]);
			const tag = ordered ? 'ol' : 'ul';
			out.push(`<${tag}>${items.map((x) => `<li>${inline(x, origin)}</li>`).join('')}</${tag}>`);
			continue;
		}
		const para: string[] = [];
		while (i < lines.length && lines[i].trim() && !li.test(lines[i]) && !/^```/.test(lines[i]) && !(isRow(lines[i]) && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1] ?? ''))) {
			para.push(lines[i++].replace(/^#{1,6}\s+/, ''));
		}
		out.push(`<p>${para.map((x) => inline(x, origin)).join('<br>')}</p>`);
	}
	return out.join('');
}

// ------------------------------------------------------------------ page context

/** The page in the middle column as a reference (§3.13.3: never its text). */
export function pageRef(pathname: string): PageRef | null {
	const dec = (s: string) => {
		try {
			return decodeURIComponent(s);
		} catch {
			return s;
		}
	};
	let m = /^\/issue\/([^/]+)/.exec(pathname);
	if (m) return { kind: 'issue', key: dec(m[1]).toUpperCase() };
	m = /^\/views\/([^/]+)/.exec(pathname);
	if (m) return { kind: 'view', id: dec(m[1]) };
	m = /^\/project\/([^/]+)/.exec(pathname);
	if (m) return { kind: 'project', key: dec(m[1]).toUpperCase() };
	return null;
}
export function pageRefLabel(r: PageRef): string {
	return r.kind === 'view' ? '視圖' : r.key;
}

// ------------------------------------------------------------------ parts and texts

export type Part =
	| { type: 'text'; text: string }
	| { type: 'tool_progress'; tool: string; summary: string; ok?: boolean | null }
	| { type: 'write'; outcome: string; execution_id?: string; changeset_id?: string; reason?: string; effects?: { key: string; kind: string }[]; review_url?: string; undo_url?: string }
	| { type: 'unavailable'; reason: string }
	| { type: 'notice'; kind: string; text: string };

const EFFECT: Record<string, string> = {
	note_added: '新增留言',
	fields_changed: '修改欄位',
	status_changed: '轉狀態',
	issue_created: '建立議題',
	created: '建立議題',
	// The backend sends only created / changed for now (backend gap G38).
	changed: '修改',
	duplicate_added: '標為重複',
	triage_decided: '做了 Triage 決定',
	time_logged: '記工時',
	time_updated: '修改工時',
	version_created: '建立里程碑',
	version_updated: '修改里程碑'
};
/** The write card's first line (§6.19 ③). */
export function writeLine(w: Extract<Part, { type: 'write' }>): string {
	if (w.outcome === 'applied') {
		const fx = w.effects ?? [];
		if (!fx.length) return '已寫入';
		return fx.map((e) => `已在 ${e.key} ${EFFECT[e.kind] ?? '寫入'}`).join('；');
	}
	if (w.outcome === 'review_required') {
		const keys = [...new Set((w.effects ?? []).map((e) => e.key))];
		return `已送審${keys.length ? `：${keys.join('、')}` : ''}（需要你核准）`;
	}
	return '寫入沒有完成';
}
export const WRITE_REASON: Record<string, string> = {
	agent_chain: '這張單上 agent 連續寫入太多次，需要你確認',
	time_taint: 'AI 在這次回覆中讀過你的工時，之後的寫入都要你確認，避免工時說明被寫到別人看得到的地方',
	require_review: 'AI 要求由你確認'
};
export const UNAVAILABLE = '（這段內容已無法讀取）';

/** The grey tail of an unfinished reply (§6.19 狀態). */
export function stopTail(stop: string | null | undefined): string | null {
	switch (stop) {
		case 'budget_exhausted':
			return '（回覆未完成：額度上限）';
		case 'deadline':
			return '（回覆未完成：時間上限）';
		case 'tool_limit':
			return '（回覆未完成：步驟上限）';
		case 'max_tokens':
			return '（回覆未完成：長度上限）';
		case 'interrupted':
			return '（回覆中斷，請再送一次）';
		case 'canceled':
			return '（已停止）';
		default:
			return null;
	}
}

/** `quota_exceeded` per scope; `retry` is `details.retry_at` (shown as HH:MM in Taipei time). */
export function quotaText(scope: string | undefined, retry: string | undefined): string {
	if (scope === 'ai_user_hour') {
		const t = retry ? new Date(retry).toLocaleTimeString('zh-TW', { hour: '2-digit', minute: '2-digit', hour12: false, timeZone: 'Asia/Taipei' }) : '';
		return `這小時的使用次數已用完${t ? `，${t} 後可再用` : ''}。`;
	}
	if (scope === 'ai_user_day') return '今天的 AI 額度已用完。';
	if (scope === 'org_month') return '本月 AI 額度已用完，請洽管理員。';
	return 'AI 額度已用完。';
}

export const MAX_INPUT = 8000;
export const INPUT_HINT = 'AI 只讀得到你委派給它的專案中的公開內容；寫入照你的委派政策。不要貼上不該公開的內容。';

// ------------------------------------------------------------------ conversation list

/** Pages of `GET /ai/conversations` joined in order; an id seen twice keeps its first place. */
export function appendPage<T extends { id: string }>(have: readonly T[], more: readonly T[]): T[] {
	const seen = new Set(have.map((x) => x.id));
	return [...have, ...more.filter((x) => !seen.has(x.id) && (seen.add(x.id), true))];
}

/** The title dropdown (recent 20): an older conversation opened from the history joins it at the top. */
export function withConv<T extends { id: string }>(recent: readonly T[], c: T): T[] {
	return recent.some((x) => x.id === c.id) ? [...recent] : [c, ...recent];
}

/** §6.19 expiry banner: the projects of delegations still in force, soonest to expire
 *  first. `GET /ai-team/{id}` also lists expired, unrevoked ones; `/ai/status` does not. */
export function expiringProjects(policies: readonly { project: string; expires_at: string }[], now: number): string[] {
	return policies
		.filter((p) => Date.parse(p.expires_at) > now)
		.sort((x, y) => Date.parse(x.expires_at) - Date.parse(y.expires_at))
		.map((p) => p.project);
}

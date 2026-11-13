// SPDX-License-Identifier: GPL-2.0-or-later
// The Markdown editor's pure parts (design 06 §6.2, R-MN1, R-M9): @mentions,
// autocomplete, toolbar edits, the byte limit and drafts. Rendering is the
// server's (`*_html`, R-M5); nothing here turns text into HTML.

/** A person the editor can mention: the project's assignable users (R-RF3). */
export type Mentionable = { id: string; name: string; login?: string | null; kind?: string };

const LOGIN = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,59}$/;

/** Text with code spans and fenced/indented code blocks blanked out (same length),
 *  so mention scanning skips them (R-MN1: no mentions inside code). */
export function blankCode(text: string): string {
	const blank = (s: string) => s.replace(/[^\n]/g, ' ');
	const lines = text.split('\n');
	let fence: string | null = null;
	let prevBlankOrCode = true;
	const out = lines.map((l) => {
		const f = /^ {0,3}(`{3,}|~{3,})/.exec(l);
		if (fence) {
			// The closing fence: the same character, at least as long, nothing after it.
			if (f && f[1][0] === fence[0] && f[1].length >= fence.length && /^ {0,3}[`~]+\s*$/.test(l)) fence = null;
			prevBlankOrCode = true;
			return blank(l);
		}
		if (f) {
			fence = f[1];
			prevBlankOrCode = true;
			return blank(l);
		}
		// An indented code block starts after a blank line (or continues one).
		if (/^( {4}|\t)/.test(l) && prevBlankOrCode && l.trim()) return blank(l);
		prevBlankOrCode = !l.trim();
		return l;
	});
	// Code spans stay inside one paragraph: a blank line ends the search.
	return out
		.join('\n')
		.split(/(\n[ \t]*\n)/)
		.map((para) => para.replace(/(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)/g, blank))
		.join('');
}

/** The logins mentioned in `text` (R-MN1, R-MN2), in order, each once.
 *  `@` must follow the line start or a character other than `[A-Za-z0-9_.@]`;
 *  the login may not be followed by `@` or `[A-Za-z0-9_-]` (an agent handle
 *  `@claude-code@alice` mentions nobody). A trailing `.` ends the sentence. */
export function mentionsIn(text: string): string[] {
	// Blank code, then links (target and text) and bare URLs: none mention anyone.
	const blank = (s: string) => s.replace(/[^\n]/g, ' ');
	const t = blankCode(text)
		.replace(/!?\[[^\]\n]*\]\([^)\n]*\)/g, blank)
		.replace(/<[a-z][a-z0-9+.-]*:[^>\s]*>/gi, blank)
		.replace(/\b[a-z][a-z0-9+.-]*:\/\/[^\s<]*/gi, blank);
	const out: string[] = [];
	const re = /@([A-Za-z0-9][A-Za-z0-9_.-]{0,59})/g;
	for (let m = re.exec(t); m; m = re.exec(t)) {
		// The character before `@` in the original text: a code span or emphasis
		// marker right before it is no word boundary for the server either.
		const prev = m.index > 0 ? text[m.index - 1] : '';
		if (prev && /[A-Za-z0-9_.@`*~]/.test(prev)) continue;
		let login = m[1];
		const end = m.index + m[0].length;
		const next = t[end] ?? '';
		if (next === '@' || /[A-Za-z0-9_-]/.test(next)) continue;
		login = login.replace(/\.+$/, '');
		if (!login || !LOGIN.test(login)) continue;
		if (!out.includes(login)) out.push(login);
	}
	return out;
}

/** Who a login names among the mentionable (R-MN1): an exact match first, else a
 *  case-insensitive one only when exactly one person matches. */
export function personFor(login: string, people: readonly Mentionable[]): Mentionable | null {
	const users = people.filter((p) => p.login && p.kind !== 'group');
	const exact = users.find((p) => p.login === login);
	if (exact) return exact;
	const ci = users.filter((p) => p.login!.toLowerCase() === login.toLowerCase());
	return ci.length === 1 ? ci[0] : null;
}

/** The chips under the editor: who this text will mention (and notify, R-MN3). */
export function mentionChips(text: string, people: readonly Mentionable[]): Mentionable[] {
	const out: Mentionable[] = [];
	for (const l of mentionsIn(text)) {
		const p = personFor(l, people);
		if (p && !out.some((x) => x.id === p.id)) out.push(p);
	}
	return out;
}

/** The `@…` being typed just before the caret: from the second character on (§6.2). */
export function mentionQuery(text: string, caret: number): { start: number; q: string } | null {
	const before = text.slice(0, caret);
	const m = /(^|[^A-Za-z0-9_.@])@([^\s@`]{2,40})$/.exec(before);
	if (!m) return null;
	return { start: caret - m[2].length - 1, q: m[2] };
}

/** Assignable users matching a query by login or name; groups cannot be mentioned. */
export function filterPeople(people: readonly Mentionable[], q: string, max = 8): Mentionable[] {
	const s = q.toLowerCase();
	const users = people.filter((p) => p.kind !== 'group');
	const hit = (p: Mentionable) => (p.login ?? '').toLowerCase().startsWith(s) || p.name.toLowerCase().includes(s);
	const starts = (p: Mentionable) => ((p.login ?? '').toLowerCase().startsWith(s) || p.name.toLowerCase().startsWith(s) ? 0 : 1);
	return users.filter(hit).sort((a, b) => starts(a) - starts(b) || a.name.localeCompare(b.name)).slice(0, max);
}

/** Replaces the typed `@query` with `@login ` and puts the caret after it. */
export function insertMention(text: string, start: number, caret: number, login: string): { text: string; caret: number } {
	const ins = `@${login} `;
	return { text: text.slice(0, start) + ins + text.slice(caret), caret: start + ins.length };
}

export type Edit = { text: string; selStart: number; selEnd: number };

/** Wraps the selection (or a placeholder) in `before`/`after`: bold, italic, code, link. */
export function wrapSel(text: string, s: number, e: number, before: string, after: string, placeholder: string): Edit {
	const sel = text.slice(s, e) || placeholder;
	const t = text.slice(0, s) + before + sel + after + text.slice(e);
	return { text: t, selStart: s + before.length, selEnd: s + before.length + sel.length };
}

/** Prefixes every line the selection touches: headings, lists, quotes, task lists.
 *  `n` in the prefix numbers the lines (1. 2. 3.). */
export function prefixLines(text: string, s: number, e: number, prefix: string): Edit {
	const ls = s === 0 ? 0 : text.lastIndexOf('\n', s - 1) + 1;
	const nl = text.indexOf('\n', Math.max(e - (e > s && text[e - 1] === '\n' ? 1 : 0), s));
	const le = nl === -1 ? text.length : nl;
	const lines = text.slice(ls, le).split('\n');
	const block = lines.map((l, i) => prefix.replace('n', String(i + 1)) + l).join('\n');
	const t = text.slice(0, ls) + block + text.slice(le);
	return { text: t, selStart: ls, selEnd: ls + block.length };
}

/** A fenced code block around the selection, on its own lines. */
export function codeBlock(text: string, s: number, e: number): Edit {
	const sel = text.slice(s, e);
	const pre = s > 0 && text[s - 1] !== '\n' ? '\n' : '';
	const ins = `${pre}\`\`\`\n${sel || '程式碼'}\n\`\`\`\n`;
	const t = text.slice(0, s) + ins + text.slice(e);
	const at = s + pre.length + 4;
	return { text: t, selStart: at, selEnd: at + (sel || '程式碼').length };
}

/** A two-column table to fill in. */
export function table(text: string, s: number): Edit {
	const pre = s > 0 && text[s - 1] !== '\n' ? '\n\n' : '';
	const ins = `${pre}| 欄位 | 欄位 |\n|---|---|\n|  |  |\n`;
	return { text: text.slice(0, s) + ins + text.slice(s), selStart: s + pre.length + 2, selEnd: s + pre.length + 4 };
}

export const utf8Bytes = (s: string) => new TextEncoder().encode(s).length;

/** 「約 N 字」 for a byte count: CJK text is about three bytes a character. */
export function bytesText(used: number, limit: number): string {
	const kib = (n: number) => (n >= 1024 ? `${Math.round(n / 1024)} KiB` : `${n} B`);
	return `${kib(used)} / ${kib(limit)}`;
}

/** Limits per field (§6.2): notes 64 KiB, wiki 512 KiB; descriptions as notes. */
export const LIMIT = { note: 64 * 1024, description: 64 * 1024, wiki: 512 * 1024 } as const;

/** R-M9: a person rewriting a `plain` description converts it to `markdown`;
 *  an unchanged one keeps its format (the op omits `format`). */
export function formatFor(oldFormat: string | null | undefined, changed: boolean): 'markdown' | undefined {
	return changed && oldFormat === 'plain' ? 'markdown' : undefined;
}

/** Drafts (§6.2) live in localStorage under the user and the target; reads and
 *  writes never throw (private windows, blocked storage). */
export const draftKey = (userId: string, target: string) => `ox:draft:${userId}:${target}`;
export function readDraft(key: string): string | null {
	try {
		return localStorage.getItem(key);
	} catch {
		return null;
	}
}
export function writeDraft(key: string, text: string) {
	try {
		if (text.trim()) localStorage.setItem(key, text);
		else localStorage.removeItem(key);
	} catch {
		/* storage unavailable: the draft is only lost on reload */
	}
}
export function dropDraft(key: string) {
	try {
		localStorage.removeItem(key);
	} catch {
		/* nothing to drop */
	}
}

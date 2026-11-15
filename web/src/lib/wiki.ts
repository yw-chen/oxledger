// SPDX-License-Identifier: GPL-2.0-or-later
// Wiki helpers (design 06 §3.5, §6.3–§6.6): titles in URLs, the page tree,
// command documents, and diffs (the server's hunks and a local line diff
// for the conflict dialog, drawn the same way).

import type { DiffHunk, DiffLine, WikiCommandDoc, WikiOp, WikiTreeItem, WikiVersionItem } from '$api/types';
import { newOperationId } from './api';

// ------------------------------------------------------------------ titles

/**
 * R-W2: a title in a URL has its spaces as `_`. The server's title key folds
 * every run of spaces and `_` into one `_`, so `a b`, `a_b` and `a__b` are one
 * page, and any spelling of the title opens it.
 */
export const urlTitle = (title: string) => title.trim().replace(/ /g, '_');

/**
 * The title a URL segment names (already percent-decoded by the router), for
 * display and to prefill a new page. Only a single `_` between two other
 * characters becomes a space (`部署_與_回滾` → `部署 與 回滾`); runs and
 * `_` at either end stay as they are (`__init__`, `a__b`), which the server
 * takes for the same title while the person sees what the link said.
 */
export const titleFromParam = (p: string) => p.replace(/(?<=[^_\s])_(?=[^_\s])/gu, ' ').trim();

/** The SPA path of a page (`/project/KEY/wiki/Title`), with an optional suffix like `/edit`. */
export function wikiPath(projectKey: string, title: string, suffix = ''): string {
	return `/project/${projectKey}/wiki/${encodeURIComponent(urlTitle(title))}${suffix}`;
}

/** The server writes wiki links as `/projects/KEY/wiki/Title`; the SPA routes are `/project/…`. */
export function spaWikiHref(href: string): string | null {
	const m = /^\/projects\/([^/?#]+)\/wiki(\/[^?#]*)?([?#].*)?$/.exec(href);
	if (!m) return null;
	return `/project/${m[1]}/wiki${m[2] ?? ''}${m[3] ?? ''}`;
}

// ------------------------------------------------------------------ tree

export type TreeNode = { id: string; title: string; parent_id: string | null; updated_at: string; depth: number; children: TreeNode[] };

const byTitle = (a: { title: string }, b: { title: string }) => a.title.localeCompare(b.title, 'zh-TW');

/** The live pages as a forest, siblings by title. A page whose parent is not listed sits at the top. */
export function buildTree(items: readonly WikiTreeItem[]): TreeNode[] {
	const nodes = new Map<string, TreeNode>();
	for (const it of items) nodes.set(it.id, { ...it, depth: 0, children: [] });
	const roots: TreeNode[] = [];
	for (const n of nodes.values()) {
		const p = n.parent_id ? nodes.get(n.parent_id) : undefined;
		if (p && !isAncestor(nodes, n.id, p.id)) p.children.push(n);
		else roots.push(n);
	}
	const fix = (list: TreeNode[], depth: number) => {
		list.sort(byTitle);
		for (const n of list) {
			n.depth = depth;
			fix(n.children, depth + 1);
		}
	};
	fix(roots, 0);
	return roots;
}
/** Guards a cycle in the data (never expected): `id` above `of`. */
function isAncestor(nodes: Map<string, TreeNode>, id: string, of: string): boolean {
	let cur: string | null = of;
	for (let i = 0; cur && i < 64; i++) {
		if (cur === id) return true;
		cur = nodes.get(cur)?.parent_id ?? null;
	}
	return false;
}

/** The pages whose title contains `q` (case-insensitive), with their ancestors so the tree still reads. */
export function filterTree(roots: readonly TreeNode[], q: string): TreeNode[] {
	const needle = q.trim().toLocaleLowerCase();
	if (!needle) return [...roots];
	const walk = (list: readonly TreeNode[]): TreeNode[] => {
		const out: TreeNode[] = [];
		for (const n of list) {
			const kids = walk(n.children);
			if (kids.length || n.title.toLocaleLowerCase().includes(needle)) out.push({ ...n, children: kids });
		}
		return out;
	};
	return walk(roots);
}

/** Depth-first, for a `<select>` (indented by depth). */
export function flatTree(roots: readonly TreeNode[]): TreeNode[] {
	const out: TreeNode[] = [];
	const walk = (list: readonly TreeNode[]) => {
		for (const n of list) {
			out.push(n);
			walk(n.children);
		}
	};
	walk(roots);
	return out;
}

/** A page and everything under it (where it cannot move to, §6.5). */
export function subtreeIds(items: readonly WikiTreeItem[], id: string): Set<string> {
	const kids = new Map<string, string[]>();
	for (const it of items) if (it.parent_id) kids.set(it.parent_id, [...(kids.get(it.parent_id) ?? []), it.id]);
	const out = new Set<string>();
	const stack = [id];
	while (stack.length) {
		const x = stack.pop()!;
		if (out.has(x)) continue;
		out.add(x);
		stack.push(...(kids.get(x) ?? []));
	}
	return out;
}

/** The ids above a page, nearest first (to open the tree down to it). */
export function ancestorIds(items: readonly WikiTreeItem[], id: string): string[] {
	const parent = new Map(items.map((i) => [i.id, i.parent_id]));
	const out: string[] = [];
	let cur = parent.get(id) ?? null;
	while (cur && !out.includes(cur) && out.length < 64) {
		out.push(cur);
		cur = parent.get(cur) ?? null;
	}
	return out;
}

// ------------------------------------------------------------------ commands

/** One wiki command document: exactly one op, at most one precondition (§3.11). */
export function wikiDoc(op: WikiOp, pre?: { pageId: string; lock: number } | null): WikiCommandDoc {
	return {
		command_schema_version: 1,
		ops: [op],
		...(pre ? { preconditions: [{ wiki_page: pre.pageId, lock_version: pre.lock }] } : {})
	};
}
export const opId = () => newOperationId();

/** What a D12 redaction of the wiki covers (§3.13). */
export type RedactTarget = { kind: 'version'; n: number } | { kind: 'title'; title: string } | { kind: 'event'; id: string; text: string };

// ------------------------------------------------------------------ history

/** Bytes added or removed against the version below it (newest first); null when that one is not loaded. */
export function sizeDeltas(items: readonly WikiVersionItem[]): (number | null)[] {
	return items.map((it, i) => {
		const prev = items[i + 1];
		if (prev && prev.version === it.version - 1) return it.size - prev.size;
		return it.version === 1 ? it.size : null;
	});
}
export function deltaText(d: number | null): string {
	if (d === null) return '';
	if (d === 0) return '±0';
	return d > 0 ? `+${d}` : `−${-d}`;
}

/** A non-content change (`GET …/events`), said in words. */
export function eventText(kind: string, payload: unknown, titleOf: (id: unknown) => string): string {
	if (payload === '[已遮蔽]' || typeof payload !== 'object' || payload === null) {
		const k = EVENT_KIND[kind] ?? kind;
		return payload === '[已遮蔽]' ? `${k}（內容已遮蔽）` : k;
	}
	const p = payload as Record<string, unknown>;
	switch (kind) {
		case 'created':
			return `建立〈${String(p.title ?? '')}〉${p.parent_id ? `，上層〈${titleOf(p.parent_id)}〉` : ''}`;
		case 'renamed':
			return `改名：〈${String(p.from ?? '')}〉→〈${String(p.to ?? '')}〉`;
		case 'moved':
			return `移動：${p.from_parent ? `〈${titleOf(p.from_parent)}〉` : '最上層'} → ${p.to_parent ? `〈${titleOf(p.to_parent)}〉` : '最上層'}`;
		case 'deleted': {
			const n = Array.isArray(p.children) ? p.children.length : 0;
			return n ? `刪除（${n} 個子頁移到上層）` : '刪除';
		}
		case 'restored':
			return `還原${p.parent_id ? `到〈${titleOf(p.parent_id)}〉下` : '到最上層'}`;
		default:
			return EVENT_KIND[kind] ?? kind;
	}
}
const EVENT_KIND: Record<string, string> = { created: '建立', renamed: '改名', moved: '移動', deleted: '刪除', restored: '還原', protected: '保護', unprotected: '取消保護' };

// ------------------------------------------------------------------ diff

export type Word = { op: string; text: string };
export type Line = { op: 'equal' | 'delete' | 'insert'; text: string; words?: Word[] | null; oldNo: number | null; newNo: number | null };
export type Row = ({ kind: 'line' } & Line) | { kind: 'gap'; id: number; count: number; newFrom: number; oldFrom: number };

const asOp = (op: string): Line['op'] => (op === 'delete' || op === 'insert' ? op : 'equal');

/**
 * The hunks as rows: lines with their numbers, and the unchanged stretches
 * between them folded into gaps (§6.6「⋯ 展開 N 行」). A gap listed in
 * `open` is drawn as its lines when `newLines` (the newer text) is known.
 * The stretch after the last hunk is only known with `newLines`.
 */
export function diffRows(hunks: readonly DiffHunk[], newLines: readonly string[] | null = null, open: ReadonlySet<number> = new Set()): Row[] {
	const rows: Row[] = [];
	let oldNext = 1;
	let newNext = 1;
	let gapId = 0;
	const gap = (count: number) => {
		if (count <= 0) return;
		const id = gapId++;
		if (open.has(id) && newLines) {
			for (let i = 0; i < count; i++) rows.push({ kind: 'line', op: 'equal', text: newLines[newNext - 1 + i] ?? '', oldNo: oldNext + i, newNo: newNext + i });
		} else rows.push({ kind: 'gap', id, count, newFrom: newNext, oldFrom: oldNext });
		oldNext += count;
		newNext += count;
	};
	for (const h of hunks) {
		const newStart = Math.max(1, h.new_start);
		const oldStart = Math.max(1, h.old_start);
		gap(Math.min(newStart - newNext, oldStart - oldNext));
		oldNext = Math.max(oldNext, oldStart);
		newNext = Math.max(newNext, newStart);
		for (const l of h.lines as DiffLine[]) {
			const op = asOp(l.op);
			rows.push({ kind: 'line', op, text: l.text, words: l.words ?? null, oldNo: op === 'insert' ? null : oldNext, newNo: op === 'delete' ? null : newNext });
			if (op !== 'insert') oldNext++;
			if (op !== 'delete') newNext++;
		}
	}
	if (newLines && hunks.length) gap(newLines.length - newNext + 1);
	return rows;
}

export type SplitRow = { kind: 'pair'; left: Line | null; right: Line | null } | Extract<Row, { kind: 'gap' }>;

/** Two columns (§6.6, ≥ 1024 px): a run of deletions sits beside the insertions that follow it. */
export function splitRows(rows: readonly Row[]): SplitRow[] {
	const out: SplitRow[] = [];
	let i = 0;
	while (i < rows.length) {
		const r = rows[i];
		if (r.kind === 'gap') {
			out.push(r);
			i++;
		} else if (r.op === 'equal') {
			out.push({ kind: 'pair', left: r, right: r });
			i++;
		} else {
			const del: Line[] = [];
			const ins: Line[] = [];
			while (i < rows.length && rows[i].kind === 'line' && (rows[i] as Line).op === 'delete') del.push(rows[i++] as Line);
			while (i < rows.length && rows[i].kind === 'line' && (rows[i] as Line).op === 'insert') ins.push(rows[i++] as Line);
			for (let k = 0; k < Math.max(del.length, ins.length); k++) out.push({ kind: 'pair', left: del[k] ?? null, right: ins[k] ?? null });
		}
	}
	return out;
}

/** The text split into lines as the server does (no trailing empty line for a final line break). */
export function lines(text: string): string[] {
	if (text === '') return [];
	const ls = text.replace(/\r\n?/g, '\n').split('\n');
	if (ls[ls.length - 1] === '') ls.pop();
	return ls;
}

type Op = 'equal' | 'delete' | 'insert';
/** The edit script between two sequences (longest common subsequence). */
function script<T>(a: readonly T[], b: readonly T[], eq: (x: T, y: T) => boolean = (x, y) => x === y): [Op, number, number][] {
	let pre = 0;
	while (pre < a.length && pre < b.length && eq(a[pre], b[pre])) pre++;
	let suf = 0;
	while (suf < a.length - pre && suf < b.length - pre && eq(a[a.length - 1 - suf], b[b.length - 1 - suf])) suf++;
	const A = a.slice(pre, a.length - suf);
	const B = b.slice(pre, b.length - suf);
	const out: [Op, number, number][] = [];
	for (let i = 0; i < pre; i++) out.push(['equal', i, i]);
	const n = A.length;
	const m = B.length;
	if (n * m > 4_000_000) {
		// Too big to compare line by line here: everything in the middle changed.
		for (let i = 0; i < n; i++) out.push(['delete', pre + i, -1]);
		for (let j = 0; j < m; j++) out.push(['insert', -1, pre + j]);
	} else {
		const w = m + 1;
		const L = new Uint32Array((n + 1) * w);
		for (let i = n - 1; i >= 0; i--)
			for (let j = m - 1; j >= 0; j--) L[i * w + j] = eq(A[i], B[j]) ? L[(i + 1) * w + j + 1] + 1 : Math.max(L[(i + 1) * w + j], L[i * w + j + 1]);
		let i = 0;
		let j = 0;
		while (i < n || j < m) {
			if (i < n && j < m && eq(A[i], B[j])) out.push(['equal', pre + i++, pre + j++]);
			else if (j < m && (i === n || L[i * w + j + 1] >= L[(i + 1) * w + j])) out.push(['insert', -1, pre + j++]);
			else out.push(['delete', pre + i++, -1]);
		}
		// Deletions before insertions inside each changed run (as the server shows them).
		reorder(out, pre);
	}
	for (let k = 0; k < suf; k++) out.push(['equal', a.length - suf + k, b.length - suf + k]);
	return out;
}
function reorder(out: [Op, number, number][], from: number) {
	let k = from;
	while (k < out.length) {
		if (out[k][0] === 'equal') {
			k++;
			continue;
		}
		let e = k;
		while (e < out.length && out[e][0] !== 'equal') e++;
		const run = out.slice(k, e);
		const sorted = [...run.filter((x) => x[0] === 'delete'), ...run.filter((x) => x[0] === 'insert')];
		out.splice(k, e - k, ...sorted);
		k = e;
	}
}

/** Word pieces as the server cuts them (R-W7): an ASCII letter/digit run, a whitespace run, or one other character. */
export function tokens(s: string): string[] {
	return s.match(/[A-Za-z0-9]+|\s+|[\s\S]/gu) ?? [];
}
function merge(ws: Word[]): Word[] {
	const out: Word[] = [];
	for (const w of ws) {
		const last = out[out.length - 1];
		if (last && last.op === w.op) last.text += w.text;
		else out.push({ ...w });
	}
	return out;
}
/** Word-level pieces of a deleted line and the inserted line paired with it. */
export function wordDiff(oldLine: string, newLine: string): { old: Word[]; new: Word[] } {
	const a = tokens(oldLine);
	const b = tokens(newLine);
	const s = script(a, b);
	const o: Word[] = [];
	const n: Word[] = [];
	for (const [op, i, j] of s) {
		if (op === 'equal') {
			o.push({ op, text: a[i] });
			n.push({ op, text: b[j] });
		} else if (op === 'delete') o.push({ op, text: a[i] });
		else n.push({ op, text: b[j] });
	}
	return { old: merge(o), new: merge(n) };
}

/**
 * A local line diff in the server's shape (3 lines of context, words on
 * paired lines): the conflict dialog compares the newest version with the
 * person's unsaved text, which no version holds.
 */
export function diffText(oldText: string, newText: string, context = 3): DiffHunk[] {
	const a = lines(oldText);
	const b = lines(newText);
	const s = script(a, b);
	const changed = s.map((x) => x[0] !== 'equal');
	const hunks: DiffHunk[] = [];
	let k = 0;
	while (k < s.length) {
		if (!changed[k]) {
			k++;
			continue;
		}
		const start = Math.max(0, k - context);
		let end = k;
		// Extend while the next change is within 2 × context of this one.
		for (;;) {
			while (end < s.length && changed[end]) end++;
			let next = end;
			while (next < s.length && !changed[next]) next++;
			if (next < s.length && next - end <= 2 * context) end = next;
			else break;
		}
		const stop = Math.min(s.length, end + context);
		const part = s.slice(start, stop);
		const first = part[0];
		const oldStart = first[1] >= 0 ? first[1] + 1 : (part.find((x) => x[1] >= 0)?.[1] ?? a.length) + 1;
		const newStart = first[2] >= 0 ? first[2] + 1 : (part.find((x) => x[2] >= 0)?.[2] ?? b.length) + 1;
		const out: DiffLine[] = part.map(([op, i, j]) => ({ op, text: op === 'insert' ? b[j] : a[i] }));
		pairWords(out);
		hunks.push({ old_start: oldStart, new_start: newStart, lines: out });
		k = stop;
	}
	return hunks;
}
function pairWords(ls: DiffLine[]) {
	let i = 0;
	while (i < ls.length) {
		if (ls[i].op !== 'delete') {
			i++;
			continue;
		}
		const d: DiffLine[] = [];
		const n: DiffLine[] = [];
		while (i < ls.length && ls[i].op === 'delete') d.push(ls[i++]);
		while (i < ls.length && ls[i].op === 'insert') n.push(ls[i++]);
		for (let k = 0; k < Math.min(d.length, n.length); k++) {
			const w = wordDiff(d[k].text, n[k].text);
			d[k].words = w.old;
			n[k].words = w.new;
		}
	}
}

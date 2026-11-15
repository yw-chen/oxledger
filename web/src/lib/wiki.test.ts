// SPDX-License-Identifier: GPL-2.0-or-later
import { describe, expect, it } from 'vitest';
import type { DiffHunk, WikiTreeItem, WikiVersionItem } from '$api/types';
import {
	ancestorIds,
	buildTree,
	deltaText,
	diffRows,
	diffText,
	eventText,
	filterTree,
	flatTree,
	lines,
	sizeDeltas,
	spaWikiHref,
	splitRows,
	subtreeIds,
	titleFromParam,
	tokens,
	urlTitle,
	wikiDoc,
	wikiPath,
	wordDiff
} from './wiki';

/** The server's title key for spaces and `_` (R-W2 as changed: one `_` for every run). */
const fold = (t: string) => t.trim().replace(/[\s_]+/gu, '_').toLowerCase();
const item = (id: string, title: string, parent_id: string | null = null): WikiTreeItem => ({ id, title, parent_id, updated_at: '2026-11-15T00:00:00Z' });

describe('wiki titles in URLs (R-W2)', () => {
	it('turns spaces into underscores and percent-encodes', () => {
		expect(urlTitle('部署 與 回滾')).toBe('部署_與_回滾');
		expect(wikiPath('WEB', '部署 與 回滾')).toBe(`/project/WEB/wiki/${encodeURIComponent('部署_與_回滾')}`);
		expect(wikiPath('WEB', 'A&B 100%', '/edit')).toBe('/project/WEB/wiki/A%26B_100%25/edit');
	});
	it('reads a URL segment back as a title', () => {
		expect(titleFromParam('部署_與_回滾')).toBe('部署 與 回滾');
		expect(titleFromParam(decodeURIComponent('A%26B_100%25'))).toBe('A&B 100%');
	});
	it('keeps runs of `_` and `_` at the ends (the server folds them into one title)', () => {
		expect(titleFromParam('__init__')).toBe('__init__');
		expect(titleFromParam('a__b')).toBe('a__b');
		expect(titleFromParam('a_b_c')).toBe('a b c');
		expect(titleFromParam('_x')).toBe('_x');
		expect(titleFromParam('中文_標題')).toBe('中文 標題');
		// A link to `__init__` opens that page: the URL and the API path keep its spelling.
		expect(urlTitle('__init__')).toBe('__init__');
		expect(wikiPath('WEB', '__init__')).toBe('/project/WEB/wiki/__init__');
		expect(urlTitle(titleFromParam('__init__'))).toBe('__init__');
		// Round trips: a title read back from its URL names the same page.
		for (const t of ['部署 與 回滾', 'a  b', '__init__', 'snake_case name', ' x ']) {
			const back = titleFromParam(decodeURIComponent(encodeURIComponent(urlTitle(t))));
			expect(fold(back)).toBe(fold(t));
		}
		expect(spaWikiHref('/projects/WEB/wiki/__init__')).toBe('/project/WEB/wiki/__init__');
	});
	it('maps the server wiki links onto the SPA routes', () => {
		expect(spaWikiHref('/projects/WEB/wiki/Install_Guide')).toBe('/project/WEB/wiki/Install_Guide');
		expect(spaWikiHref('/projects/WEB/wiki/%E9%83%A8%E7%BD%B2#step')).toBe('/project/WEB/wiki/%E9%83%A8%E7%BD%B2#step');
		expect(spaWikiHref('/issue/WEB-1')).toBeNull();
	});
});

describe('wiki tree', () => {
	const items = [item('c', 'Child', 'a'), item('a', 'Deploy'), item('b', 'API'), item('d', 'Grandchild', 'c'), item('o', 'Orphan', 'gone')];
	it('nests children under parents, siblings by title, orphans at the top', () => {
		const roots = buildTree(items);
		expect(roots.map((n) => n.title)).toEqual(['API', 'Deploy', 'Orphan']);
		const dep = roots.find((n) => n.id === 'a')!;
		expect(dep.children.map((n) => n.title)).toEqual(['Child']);
		expect(dep.children[0].children[0]).toMatchObject({ id: 'd', depth: 2 });
	});
	it('does not loop on a cycle in the data', () => {
		const roots = buildTree([item('x', 'X', 'y'), item('y', 'Y', 'x')]);
		expect(flatTree(roots).map((n) => n.id).sort()).toEqual(['x', 'y']);
	});
	it('filters by title and keeps the ancestors of a match', () => {
		const shown = filterTree(buildTree(items), 'grand');
		expect(shown.map((n) => n.id)).toEqual(['a']);
		expect(shown[0].children[0].children[0].id).toBe('d');
		expect(filterTree(buildTree(items), '  ').length).toBe(3);
		expect(filterTree(buildTree(items), 'api').map((n) => n.id)).toEqual(['b']);
	});
	it('lists the subtree a page cannot move into, and the ancestors to open', () => {
		expect([...subtreeIds(items, 'a')].sort()).toEqual(['a', 'c', 'd']);
		expect(ancestorIds(items, 'd')).toEqual(['c', 'a']);
		expect(ancestorIds(items, 'b')).toEqual([]);
	});
	it('flattens depth first for a select', () => {
		expect(flatTree(buildTree(items)).map((n) => `${n.depth}:${n.title}`)).toEqual(['0:API', '0:Deploy', '1:Child', '2:Grandchild', '0:Orphan']);
	});
});

describe('wiki command documents (§3.11)', () => {
	it('has exactly one op and at most one precondition', () => {
		const op = { kind: 'wiki_protect' as const, op_id: 'o', target: { project: 'WEB', page_id: 'p' }, protected: true };
		expect(wikiDoc(op)).toEqual({ command_schema_version: 1, ops: [op] });
		expect(wikiDoc(op, { pageId: 'p', lock: 7 }).preconditions).toEqual([{ wiki_page: 'p', lock_version: 7 }]);
	});
});

describe('history', () => {
	const v = (version: number, size: number): WikiVersionItem => ({ version, size, author: null, comment: '', created_at: '', origin: 'human_direct', redacted: false });
	it('size change against the version below, null when not loaded', () => {
		expect(sizeDeltas([v(5, 120), v(4, 160), v(2, 10)])).toEqual([-40, null, null]);
		expect(sizeDeltas([v(2, 30), v(1, 10)])).toEqual([20, 10]);
		expect([deltaText(-40), deltaText(20), deltaText(0), deltaText(null)]).toEqual(['−40', '+20', '±0', '']);
	});
	it('says events in words, redacted ones without their content', () => {
		const t = (id: unknown) => (id === 'p' ? '上層' : '?');
		expect(eventText('renamed', { from: 'A', to: 'B' }, t)).toBe('改名：〈A〉→〈B〉');
		expect(eventText('moved', { from_parent: null, to_parent: 'p' }, t)).toBe('移動：最上層 → 〈上層〉');
		expect(eventText('deleted', { children: ['x', 'y'] }, t)).toBe('刪除（2 個子頁移到上層）');
		expect(eventText('renamed', '[已遮蔽]', t)).toBe('改名（內容已遮蔽）');
		expect(eventText('protected', {}, t)).toBe('保護');
	});
});

describe('diff rows (§6.6)', () => {
	// Lines 1–10 unchanged, line 11 changed, 12–14 context; 30 lines in the new text.
	const hunks: DiffHunk[] = [
		{
			old_start: 8,
			new_start: 8,
			lines: [
				{ op: 'equal', text: 'l8' },
				{ op: 'equal', text: 'l9' },
				{ op: 'equal', text: 'l10' },
				{ op: 'delete', text: 'old 11', words: [{ op: 'delete', text: 'old' }, { op: 'equal', text: ' 11' }] },
				{ op: 'insert', text: 'new 11', words: [{ op: 'insert', text: 'new' }, { op: 'equal', text: ' 11' }] },
				{ op: 'insert', text: 'added' },
				{ op: 'equal', text: 'l12' }
			]
		},
		{ old_start: 20, new_start: 21, lines: [{ op: 'equal', text: 'l20' }, { op: 'delete', text: 'gone' }] }
	];
	const newText = Array.from({ length: 30 }, (_, i) => `n${i + 1}`);

	it('numbers the lines and folds what lies between hunks', () => {
		const rows = diffRows(hunks);
		expect(rows[0]).toEqual({ kind: 'gap', id: 0, count: 7, newFrom: 1, oldFrom: 1 });
		const del = rows.find((r) => r.kind === 'line' && r.op === 'delete');
		expect(del).toMatchObject({ oldNo: 11, newNo: null, text: 'old 11' });
		const ins = rows.filter((r) => r.kind === 'line' && r.op === 'insert');
		expect(ins.map((r) => r.kind === 'line' && r.newNo)).toEqual([11, 12]);
		// Between: old 13..19 (7 lines), new 14..20.
		const gap2 = rows.find((r) => r.kind === 'gap' && r.id === 1);
		expect(gap2).toMatchObject({ count: 7, oldFrom: 13, newFrom: 14 });
		// No trailing gap without the text.
		expect(rows[rows.length - 1]).toMatchObject({ kind: 'line', op: 'delete', oldNo: 21 });
	});
	it('opens a folded stretch from the newer text, and knows the trailing one', () => {
		const rows = diffRows(hunks, newText, new Set([0]));
		expect(rows.slice(0, 7).map((r) => r.kind === 'line' && r.text)).toEqual(['n1', 'n2', 'n3', 'n4', 'n5', 'n6', 'n7']);
		const last = rows[rows.length - 1];
		expect(last).toMatchObject({ kind: 'gap', count: 9, newFrom: 22 });
	});
	it('puts deletions beside the insertions in two columns', () => {
		const pairs = splitRows(diffRows(hunks));
		const changed = pairs.filter((p) => p.kind === 'pair' && (p.left?.op !== 'equal' || p.right?.op !== 'equal'));
		expect(changed.map((p) => p.kind === 'pair' && [p.left?.text ?? null, p.right?.text ?? null])).toEqual([
			['old 11', 'new 11'],
			[null, 'added'],
			['gone', null]
		]);
	});
});

describe('local diff (conflict dialog)', () => {
	it('cuts words like the server: ASCII runs, spaces, one other character each', () => {
		expect(tokens('deploy 到 prod-1')).toEqual(['deploy', ' ', '到', ' ', 'prod', '-', '1']);
	});
	it('marks the changed words of a paired line', () => {
		const w = wordDiff('部署到 staging', '部署到 production');
		expect(w.old).toEqual([{ op: 'equal', text: '部署到 ' }, { op: 'delete', text: 'staging' }]);
		expect(w.new).toEqual([{ op: 'equal', text: '部署到 ' }, { op: 'insert', text: 'production' }]);
	});
	it('makes hunks with 3 lines of context, deletions first', () => {
		const a = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '10'].join('\n');
		const b = ['1', '2', '3', '4', '5', 'six', '7', '8', '9', '10', '11'].join('\n');
		const h = diffText(a, b);
		expect(h.length).toBe(1);
		expect(h[0].old_start).toBe(3);
		expect(h[0].lines.map((l) => `${l.op[0]}${l.text}`)).toEqual(['e3', 'e4', 'e5', 'd6', 'isix', 'e7', 'e8', 'e9', 'e10', 'i11']);
		expect(h[0].lines.find((l) => l.op === 'delete')?.words).toEqual([{ op: 'delete', text: '6' }]);
	});
	it('splits far changes into two hunks and handles empty and equal texts', () => {
		const a = Array.from({ length: 30 }, (_, i) => `x${i}`).join('\n');
		const b = a.replace('x2\n', 'y2\n').replace('x27', 'y27');
		expect(diffText(a, b).length).toBe(2);
		expect(diffText(a, a)).toEqual([]);
		expect(diffText('', 'a\nb')).toEqual([{ old_start: 1, new_start: 1, lines: [{ op: 'insert', text: 'a' }, { op: 'insert', text: 'b' }] }]);
		expect(lines('a\r\nb\n')).toEqual(['a', 'b']);
	});
});

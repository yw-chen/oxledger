// SPDX-License-Identifier: GPL-2.0-or-later
// The search page's Wiki tab (design 06 §6.7, §3.14): what a typed query
// means for `GET /wiki/search`, where a result links, and the pages this tab
// just wrote (03 Q-S10: a write receipt is not a search result, so a page
// the index has not caught up with shows as 「剛寫入・同步中」, not missing).

import type { SearchParams, WikiSearchItem } from '$api/types';

/** The SPA path of a result: the server's `url_title` is already encoded the wiki route's way (R-W2). */
export const wikiHitHref = (it: Pick<WikiSearchItem, 'project' | 'url_title'>) => `/project/${encodeURIComponent(it.project.key)}/wiki/${it.url_title}`;

/** The search box's syntax for issues, labelled the way the hint says them. */
const ISSUE_ONLY: [keyof SearchParams, string][] = [
	['status', 'status:'],
	['status_category', 'status:'],
	['tracker', 'tracker:'],
	['assignee', 'assignee:'],
	['version', 'version:']
];

/**
 * The parsed search box as the Wiki tab uses it: the free text and
 * `project:`; the issue-only filters are named so the page can say they do
 * not apply (rather than silently searching something else).
 */
export function wikiQuery(p: SearchParams): { q: string | null; project: string | null; ignored: string[] } {
	const ignored = [...new Set(ISSUE_ONLY.filter(([k]) => p[k] !== null && p[k] !== undefined && p[k] !== '').map(([, l]) => l))];
	const q = p.q?.trim() || null;
	return { q, project: p.project ?? null, ignored };
}

// ------------------------------------------------------------------ just written

export type JustWritten = { pageId: string; projectKey: string; title: string; after: string; at: number };
const KEY = 'ox.wikiJustWritten';
const TTL = 10 * 60 * 1000;

function read(): JustWritten[] {
	try {
		const all = JSON.parse(sessionStorage.getItem(KEY) ?? '[]') as JustWritten[];
		return Array.isArray(all) ? all.filter((c) => c && typeof c.pageId === 'string' && Date.now() - c.at < TTL) : [];
	} catch {
		return [];
	}
}
function write(all: JustWritten[]) {
	try {
		sessionStorage.setItem(KEY, JSON.stringify(all.slice(0, 10)));
	} catch {
		/* ignore */
	}
}

/** A page this tab created, edited, renamed or moved (newest first, one entry per page). */
export function rememberWikiWrite(c: Omit<JustWritten, 'at'>) {
	write([{ ...c, projectKey: c.projectKey.toUpperCase(), at: Date.now() }, ...read().filter((x) => x.pageId !== c.pageId)]);
}

/** A page this tab deleted: nothing to wait for any more. */
export function forgetWikiWrite(pageId: string) {
	const all = read();
	const left = all.filter((x) => x.pageId !== pageId);
	if (left.length !== all.length) write(left);
}

/** Every token after an epoch change is void: drop them all. */
export function clearWikiWrites() {
	write([]);
}

/**
 * The writes a search has not returned yet (within `project` when the search
 * is limited to one); those it did return are forgotten.
 */
export function pendingWikiWrites(seenPageIds: string[], project: string | null): JustWritten[] {
	const seen = new Set(seenPageIds);
	const all = read();
	const left = all.filter((c) => !seen.has(c.pageId));
	if (left.length !== all.length) write(left);
	return project ? left.filter((c) => c.projectKey === project.toUpperCase()) : left;
}

/** The token to wait for: the newest write's (the tokens share one epoch). */
export function latestWikiToken(): string | null {
	const all = read();
	return all.length ? all.reduce((a, b) => (b.at > a.at ? b : a)).after : null;
}

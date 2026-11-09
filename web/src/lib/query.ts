// SPDX-License-Identifier: GPL-2.0-or-later
// The text query syntax of design 03 §11, ported from
// crates/oxledger/src/query_syntax.rs (same grammar, same error positions):
// `project:`, `status:`, `tracker:`, `assignee:`, `version:` and free text.
// The search endpoint takes structured parameters, so the SPA parses here;
// saved tabs are still normalized by the server.

import type { SearchParams } from '$api/types';

export const KEYS = ['project', 'status', 'tracker', 'assignee', 'version'] as const;
const CATEGORIES = ['open', 'todo', 'doing', 'done'];

export type Query = {
	projects: string[];
	status?: string;
	tracker?: string;
	assignee?: string;
	version?: string;
	text: string[];
};

export class SyntaxError_ extends Error {
	constructor(
		readonly position: number,
		message: string
	) {
		super(message);
	}
}

const isWs = (c: string) => /\s/u.test(c);

export function parse(input: string): Query {
	const chars = Array.from(input);
	const q: Query = { projects: [], text: [] };
	let i = 0;
	while (i < chars.length) {
		if (isWs(chars[i])) {
			i++;
			continue;
		}
		const start = i;
		while (i < chars.length && !isWs(chars[i]) && chars[i] !== ':' && chars[i] !== '"') i++;
		const word = chars.slice(start, i).join('');
		const key = word.toLowerCase();
		if (i < chars.length && chars[i] === ':' && (KEYS as readonly string[]).includes(key)) {
			i++;
			const vstart = i;
			let value: string;
			if (i < chars.length && chars[i] === '"') {
				i++;
				const s = i;
				while (i < chars.length && chars[i] !== '"') i++;
				if (i === chars.length) throw new SyntaxError_(vstart, '引號沒有結束');
				value = chars.slice(s, i).join('').trim();
				i++;
				if (i < chars.length && !isWs(chars[i])) throw new SyntaxError_(i, '引號後面要接空白');
			} else {
				const s = i;
				while (i < chars.length && !isWs(chars[i])) {
					if (chars[i] === '"') throw new SyntaxError_(i, '引號只能放在值的開頭');
					i++;
				}
				value = chars.slice(s, i).join('');
			}
			if (!value) throw new SyntaxError_(vstart, '條件沒有值');
			if (key === 'project') {
				q.projects.push(value);
				continue;
			}
			const k = key as 'status' | 'tracker' | 'assignee' | 'version';
			if (q[k] !== undefined) throw new SyntaxError_(start, '每個條件只能出現一次');
			q[k] = value;
		} else {
			while (i < chars.length && !isWs(chars[i])) i++;
			const term = chars.slice(start, i).join('');
			if (term.includes('"')) throw new SyntaxError_(start + Array.from(term).indexOf('"'), '引號只能用在條件的值');
			q.text.push(term);
		}
	}
	return q;
}

const quote = (v: string) => (/\s/u.test(v) ? `"${v}"` : v);

export function normalized(q: Query): string {
	const parts = q.projects.map((p) => `project:${quote(p)}`);
	for (const k of ['status', 'tracker', 'assignee', 'version'] as const) if (q[k] !== undefined) parts.push(`${k}:${quote(q[k]!)}`);
	return [...parts, ...q.text].join(' ');
}

export function hasConditions(q: Query): boolean {
	return q.projects.length > 0 || KEYS.slice(1).some((k) => q[k as 'status'] !== undefined);
}

/** The search takes one project: a second `project:` is an error at its
 *  position (`input` is the text that was parsed). */
export function toParams(q: Query, input = ''): SearchParams {
	if (q.projects.length > 1) {
		const lower = Array.from(input.toLowerCase());
		let at = 0;
		let seen = 0;
		for (let i = 0; i + 8 <= lower.length; i++) {
			if (lower.slice(i, i + 8).join('') === 'project:' && (i === 0 || /\s/u.test(lower[i - 1])) && ++seen === 2) {
				at = i;
				break;
			}
		}
		throw new SyntaxError_(at, '一次只能查一個專案');
	}
	const s = q.status;
	const cat = s && CATEGORIES.includes(s.toLowerCase()) ? s.toLowerCase() : null;
	return {
		q: q.text.length ? q.text.join(' ') : null,
		project: q.projects[0] ?? null,
		tracker: q.tracker ?? null,
		status: cat ? null : (s ?? null),
		status_category: cat,
		assignee: q.assignee ?? null,
		version: q.version ?? null
	};
}

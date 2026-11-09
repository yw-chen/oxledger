// SPDX-License-Identifier: GPL-2.0-or-later
// Q-S10: issues this tab just created. Search may not have them yet; the
// search page shows them as a separate "剛建立・同步中" card instead of
// letting them look missing (a write receipt is not a search result).

export type JustCreated = { key: string; subject: string; after: string; at: number };
const KEY = 'ox.justCreated';
const TTL = 10 * 60 * 1000;

function read(): JustCreated[] {
	try {
		const all = JSON.parse(sessionStorage.getItem(KEY) ?? '[]') as JustCreated[];
		return all.filter((c) => Date.now() - c.at < TTL);
	} catch {
		return [];
	}
}
function write(all: JustCreated[]) {
	try {
		sessionStorage.setItem(KEY, JSON.stringify(all.slice(0, 10)));
	} catch {
		/* ignore */
	}
}

export function rememberCreated(c: Omit<JustCreated, 'at'>) {
	write([{ ...c, at: Date.now() }, ...read().filter((x) => x.key !== c.key)]);
}

/** The ones a search has not returned yet; those it did return are forgotten. */
export function pendingCreated(seenKeys: string[]): JustCreated[] {
	const seen = new Set(seenKeys);
	const all = read();
	const left = all.filter((c) => !seen.has(c.key));
	if (left.length !== all.length) write(left);
	return left;
}

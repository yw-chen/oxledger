// SPDX-License-Identifier: GPL-2.0-or-later
// Names for user ids the page only has as ids (form version history, §6.10
// "誰"). There is no site-wide user list on the wire, so this resolves from what
// the page already has: the signed-in user and a project's people
// (assignees, time users). Anyone else shows as a short id.

export type Person = { id: string; name: string; kind?: string };

/** "你" for the signed-in user, a known person's name, else "使用者 1a2b3c4d". */
export function whoName(id: string | null | undefined, me: { user_id: string } | null | undefined, people: readonly Person[] = []): string {
	if (!id) return '';
	if (me && me.user_id === id) return '你';
	const p = people.find((x) => x.id === id && (x.kind === undefined || x.kind === 'user'));
	if (p) return p.name;
	return `使用者 ${id.replace(/-/g, '').slice(0, 8)}`;
}

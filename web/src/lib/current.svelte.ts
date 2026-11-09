// SPDX-License-Identifier: GPL-2.0-or-later
// What the middle column is showing, for the navigation and the right column.
export const current = $state<{ projectKey: string | null }>({ projectKey: null });

/** §18.6: the tab a person came from (sessionStorage only, never sent). */
export type FromTab = { project: string; tab: string; title: string };
export function rememberTab(t: FromTab) {
	try {
		sessionStorage.setItem('ox.fromTab', JSON.stringify(t));
	} catch {
		/* ignore */
	}
}
export function fromTab(project: string): FromTab | null {
	try {
		const t = JSON.parse(sessionStorage.getItem('ox.fromTab') ?? 'null') as FromTab | null;
		return t && t.project === project ? t : null;
	} catch {
		return null;
	}
}

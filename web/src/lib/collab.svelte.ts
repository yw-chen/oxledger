// SPDX-License-Identifier: GPL-2.0-or-later
// The right collaboration column's shared state (contract 05 §6.19, 01 §6):
// at 1280 px and wider it is a column; below that it is a drawer the top
// bar's 「AI」 button opens. The AI status is here too, because the top bar
// needs to know whether the built-in AI is on.

import type { AiStatus } from '$api/types';
import { api } from '$lib/api';
import { prefs, savePrefs } from '$lib/prefs.svelte';

export const NARROW_QUERY = '(max-width: 1279.98px)';

export const collabUi = $state<{ narrow: boolean; drawer: boolean; ai: AiStatus | null }>({ narrow: false, drawer: false, ai: null });

export async function loadAiStatus() {
	try {
		collabUi.ai = await api('GET /ai/status');
	} catch {
		collabUi.ai = null;
	}
}

/** What had focus when the drawer opened (the top bar button), to give it back. */
let opener: HTMLElement | null = null;

/** The top bar button: the AI tab when the AI is on, else the activity. */
export function openDrawer() {
	prefs.collabTab = collabUi.ai?.enabled ? 'ai' : 'activity';
	savePrefs();
	opener = typeof document !== 'undefined' && document.activeElement instanceof HTMLElement ? document.activeElement : null;
	collabUi.drawer = true;
}
/** `restoreFocus` false when a navigation closes it: the new page takes focus. */
export function closeDrawer(restoreFocus = true) {
	if (!collabUi.drawer) return;
	collabUi.drawer = false;
	const back = opener;
	opener = null;
	if (restoreFocus && back?.isConnected) back.focus();
}

/** Follows the window width; returns the cleanup. */
export function watchNarrow(): () => void {
	if (typeof matchMedia === 'undefined') return () => {};
	const mq = matchMedia(NARROW_QUERY);
	const set = () => {
		collabUi.narrow = mq.matches;
		if (!mq.matches) closeDrawer(false);
	};
	set();
	mq.addEventListener('change', set);
	return () => mq.removeEventListener('change', set);
}

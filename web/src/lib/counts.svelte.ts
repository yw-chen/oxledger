// SPDX-License-Identifier: GPL-2.0-or-later
// Navigation badges: "等我審 N" (§15.3) and agents that need attention.
// Refreshed on navigation and after actions, never on a timer (§15.1:
// background polling must not keep the session alive).

import type { ChangesetSummary, ContextView } from '$api/types';
import { api } from './api';

export const counts = $state<{
	awaiting: ChangesetSummary[];
	awaitingMore: boolean;
	contexts: ContextView[];
	loadedAt: number;
}>({ awaiting: [], awaitingMore: false, contexts: [], loadedAt: 0 });

let inflight: Promise<void> | null = null;

export function refreshCounts(): Promise<void> {
	if (inflight) return inflight;
	inflight = (async () => {
		try {
			const [q, ctx] = await Promise.all([api('GET /changesets', { query: { bucket: 'awaiting_me' } }), api('GET /agent-contexts')]);
			counts.awaiting = q.items;
			counts.awaitingMore = q.next_cursor !== null;
			counts.contexts = ctx;
			counts.loadedAt = Date.now();
		} catch {
			/* badges are best effort */
		} finally {
			inflight = null;
		}
	})();
	return inflight;
}

export function activeContexts() {
	return counts.contexts.filter((c) => c.state === 'active');
}
export function attentionContexts() {
	return activeContexts().filter((c) => c.suspended_at || c.changed_projects.length > 0);
}
export function awaitingLabel(): string {
	const n = counts.awaiting.length;
	return counts.awaitingMore ? `${n}+` : String(n);
}

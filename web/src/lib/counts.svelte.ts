// SPDX-License-Identifier: GPL-2.0-or-later
// Navigation badges: "等我審 N" (§15.3) and agents that need attention.
// Refreshed on navigation and after actions, never on a timer (§15.1:
// background polling must not keep the session alive).

import type { ChangesetSummary, ContextView, TriageCount } from '$api/types';
import { api } from './api';

export const counts = $state<{
	awaiting: ChangesetSummary[];
	awaitingMore: boolean;
	contexts: ContextView[];
	triage: TriageCount[];
	loadedAt: number;
}>({ awaiting: [], awaitingMore: false, contexts: [], triage: [], loadedAt: 0 });

let inflight: Promise<void> | null = null;

export function refreshCounts(): Promise<void> {
	if (inflight) return inflight;
	inflight = (async () => {
		const tri = refreshTriageCounts();
		try {
			const [q, ctx] = await Promise.all([api('GET /changesets', { query: { bucket: 'awaiting_me' } }), api('GET /agent-contexts')]);
			counts.awaiting = q.items;
			counts.awaitingMore = q.next_cursor !== null;
			counts.contexts = ctx;
			counts.loadedAt = Date.now();
		} catch {
			/* badges are best effort */
		} finally {
			await tri;
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

/** Contract 05 §3.4: pending issues per project the person may triage in. */
export async function refreshTriageCounts(): Promise<void> {
	try {
		counts.triage = (await api('GET /triage/counts')).projects;
	} catch {
		/* best effort */
	}
}
export function triageLabel(): string {
	const n = counts.triage.reduce((a, c) => a + c.count, 0);
	if (!n) return '';
	return counts.triage.some((c) => c.more) || n > 99 ? '99+' : String(n);
}

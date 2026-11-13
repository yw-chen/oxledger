// SPDX-License-Identifier: GPL-2.0-or-later
// Navigation badges: "等我審 N" (§15.3) and agents that need attention.
// Refreshed on navigation and after actions, never on a timer (§15.1:
// background polling must not keep the session alive).

import type { ChangesetSummary, TeamMember, TriageCount } from '$api/types';
import { api } from './api';
import { session } from './session.svelte';

export const counts = $state<{
	awaiting: ChangesetSummary[];
	awaitingMore: boolean;
	/** Every AI role the person can see (`GET /ai-team`, contract 05 §3.12.5). */
	team: TeamMember[];
	triage: TriageCount[];
	loadedAt: number;
}>({ awaiting: [], awaitingMore: false, team: [], triage: [], loadedAt: 0 });

let inflight: Promise<void> | null = null;

export function refreshCounts(): Promise<void> {
	if (inflight) return inflight;
	inflight = (async () => {
		const tri = refreshTriageCounts();
		try {
			const [q, ctx] = await Promise.all([api('GET /changesets', { query: { bucket: 'awaiting_me' } }), api('GET /ai-team')]);
			counts.awaiting = q.items;
			counts.awaitingMore = q.next_cursor !== null;
			counts.team = ctx.items;
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

/** The roles I sponsor. */
export function myRoles(): TeamMember[] {
	const me = session.user?.user_id;
	return counts.team.filter((m) => m.sponsor.id === me);
}
/** My roles that need me: writes paused, or quarantined (a source project's permissions changed). */
export function attentionRoles(): TeamMember[] {
	return myRoles().filter((m) => m.status === 'paused' || m.status === 'quarantined');
}
/** A role's handle by principal id (for proposals and activity), or null. */
export function handleOf(id: string | null | undefined): string | null {
	return counts.team.find((m) => m.id === id)?.handle ?? null;
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

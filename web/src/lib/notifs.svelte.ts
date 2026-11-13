// SPDX-License-Identifier: GPL-2.0-or-later
// Unread notification count for the bell (contract 05 §6.15): polled every
// 60 s while the tab is visible and at once when it gains focus. M1 has no
// push channel (B-26).

import type { NotificationItem, UnreadCount } from '$api/types';
import { api, newOperationId } from './api';

/** `rev` bumps when the bell asks the notification centre to read again. */
export const unread = $state<{ count: UnreadCount | null; rev: number }>({ count: null, rev: 0 });

let seq = 0;
export async function refreshUnread() {
	const s = ++seq;
	try {
		const c = await api('GET /notifications/unread-count');
		if (s === seq) unread.count = c;
	} catch {
		/* keep the last value; the bell is a hint */
	}
}

export const POLL_MS = 60_000;

/** Starts polling; returns the stop function. */
export function pollUnread(doc: Document = document, win: Window = window): () => void {
	refreshUnread();
	const tick = () => {
		if (doc.visibilityState === 'visible') refreshUnread();
	};
	const t = win.setInterval(tick, POLL_MS);
	const onFocus = () => refreshUnread();
	const onVis = () => doc.visibilityState === 'visible' && refreshUnread();
	win.addEventListener('focus', onFocus);
	doc.addEventListener('visibilitychange', onVis);
	return () => {
		win.clearInterval(t);
		win.removeEventListener('focus', onFocus);
		doc.removeEventListener('visibilitychange', onVis);
	};
}

export async function markRead(n: Pick<NotificationItem, 'id'>) {
	await api('POST /notifications/{id}/read', { path: { id: n.id }, operationId: newOperationId(), body: {} as never });
	refreshUnread();
}

/** The issue page marks its unread notification read when it opens (§3.11.3). */
export async function markIssueRead(key: string) {
	try {
		const l = await api('GET /notifications', { query: { filter: 'unread', limit: 50 } });
		const mine = l.items.filter((n) => n.issue?.key === key);
		for (const n of mine) await api('POST /notifications/{id}/read', { path: { id: n.id }, operationId: newOperationId(), body: {} as never });
		if (mine.length) refreshUnread();
	} catch {
		/* not worth an error on the issue page */
	}
}

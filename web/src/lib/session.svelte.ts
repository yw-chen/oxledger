// SPDX-License-Identifier: GPL-2.0-or-later
// The browser session (contract 02 §3.3, §15.1). The CSRF token lives only in
// memory; a reload asks GET /auth/session for it again.

import type { ProjectItem, SessionData } from '$api/types';
import { api, ApiError, setCsrf, setCsrfFailedHandler, setUnauthenticatedHandler } from './api';

export const session = $state<{
	user: SessionData | null;
	projects: ProjectItem[];
	ready: boolean;
	/** Set when a request found the session gone: the layout sends the person to /login. */
	expired: boolean;
}>({ user: null, projects: [], ready: false, expired: false });

setUnauthenticatedHandler(() => {
	if (session.user) session.expired = true;
	session.user = null;
	setCsrf(null);
});

// The CSRF token went stale (session replaced in another tab): take the new
// one only when it is still the same person; otherwise ask them to sign in.
setCsrfFailedHandler(async () => {
	const before = session.user?.user_id;
	try {
		const s = await api('GET /auth/session');
		if (!before || s.user_id !== before) {
			if (session.user) session.expired = true;
			session.user = null;
			setCsrf(null);
			return false;
		}
		setCsrf(s.csrf_token);
		session.user = s;
		return true;
	} catch {
		return false;
	}
});

export async function loadSession(): Promise<SessionData | null> {
	try {
		const s = await api('GET /auth/session');
		setCsrf(s.csrf_token);
		session.user = s;
		session.expired = false;
		await loadProjects();
	} catch (e) {
		if (!(e instanceof ApiError) || e.code !== 'unauthenticated') {
			session.ready = true;
			throw e;
		}
		session.user = null;
		setCsrf(null);
	}
	session.ready = true;
	return session.user;
}

export async function loadProjects() {
	session.projects = await api('GET /projects');
}

/** login-context → login (§3.3: the challenge is handled for the person). */
export async function signIn(login: string, password: string) {
	const ctx = await api('GET /auth/login-context');
	setCsrf(ctx.csrf_token);
	const r = await api('POST /auth/login', { body: { login, password } });
	setCsrf(r.csrf_token);
	return loadSession();
}

export async function signOut() {
	try {
		await api('POST /auth/logout');
	} finally {
		session.user = null;
		session.projects = [];
		setCsrf(null);
	}
}

/** Step-up (§3.3): re-enter the password; valid for 15 minutes. */
export async function reauthenticate(password: string) {
	await api('POST /auth/reauthenticate', { body: { password } });
	if (session.user) session.user = { ...session.user, reauthenticated_at: new Date().toISOString() };
}

export function steppedUpRecently(): boolean {
	const t = session.user?.reauthenticated_at;
	return !!t && Date.now() - Date.parse(t) < 14 * 60 * 1000;
}

export function projectByKey(key: string | null | undefined) {
	return session.projects.find((p) => p.key === key?.toUpperCase());
}
export function projectById(id: string | null | undefined) {
	return session.projects.find((p) => p.id === id);
}

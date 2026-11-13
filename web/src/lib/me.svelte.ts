// SPDX-License-Identifier: GPL-2.0-or-later
// Personal settings kept on the server (contract 05 §3.8, §6.12): interface
// roles, onboarding, theme, density, left-nav shortcuts, project default views.

import type { PreferencesPut2, PreferencesView } from '$api/types';
import { api, ApiError, newOperationId } from './api';
import { applyTheme, asTheme, prefs, savePrefs } from './prefs.svelte';
import { toast } from './toast.svelte';
import { describeError } from './errors';

/** `homeRev` bumps when the home was replaced elsewhere (template applied). */
export const me = $state<{ prefs: PreferencesView | null; homeRev: number }>({ prefs: null, homeRev: 0 });

function adopt(p: PreferencesView) {
	me.prefs = p;
	prefs.theme = asTheme(p.theme);
	prefs.density = p.density === 'compact' ? 'compact' : 'comfortable';
	applyTheme();
	savePrefs();
}

export async function loadMe() {
	try {
		adopt(await api('GET /me/preferences'));
	} catch {
		/* the page still works with the browser's cached theme */
	}
}
export const adoptPrefs = adopt;

/**
 * Saves part of the preferences. On `conflict` (another tab saved first) the
 * latest version is loaded and shown, nothing is overwritten (§6 共通).
 */
export async function setPrefs(set: Omit<PreferencesPut2, 'lock_version'>): Promise<boolean> {
	if (!me.prefs) await loadMe();
	if (!me.prefs) return false;
	try {
		adopt(await api('PUT /me/preferences', { operationId: newOperationId(), body: { ...set, lock_version: me.prefs.lock_version } }));
		return true;
	} catch (e) {
		if (e instanceof ApiError && e.code === 'conflict') {
			await loadMe();
			toast('設定在別處被修改過，已載入最新版本', 'w');
		} else toast(`設定沒有儲存：${describeError(e).title}`, 'bad');
		return false;
	}
}

export const ROLES = ['pm', 'designer', 'engineer'] as const;
export const ROLE_LABEL: Record<string, string> = { pm: 'PM', designer: '設計師', engineer: '工程師' };

/** Where a left-nav shortcut goes (§3.8 SHORTCUT_KINDS). */
export function shortcutHref(s: { kind: string; id?: string | null }, projectKey: string | null): string | null {
	switch (s.kind) {
		case 'view':
			return s.id ? `/views/${encodeURIComponent(s.id)}` : null;
		case 'triage':
			return '/triage';
		case 'timesheet':
			return '/time';
		case 'board':
			return projectKey ? `/project/${projectKey}/board` : null;
		case 'timeline':
			return projectKey ? `/project/${projectKey}/timeline` : null;
		default:
			return null;
	}
}
export const SHORTCUT_LABEL: Record<string, string> = { triage: '分流', timesheet: '我的工時', board: '看板', timeline: '時程', view: '視圖' };
export const SHORTCUT_ICON: Record<string, string> = { triage: '⇥', timesheet: '◷', board: '▥', timeline: '⇢', view: '▤' };

// SPDX-License-Identifier: GPL-2.0-or-later
// Per-browser conveniences (collapsed nav, collaboration column). Theme,
// density, home widgets and shortcuts are personal settings on the server
// (contract 05 §3.8, lib/me.svelte.ts); the theme is also cached here so a
// reload does not flash the wrong colours before the server answers.

export type Theme = 'dark' | 'light' | 'high_contrast' | 'system';
export const THEMES: Theme[] = ['dark', 'light', 'high_contrast', 'system'];
export const THEME_LABEL: Record<Theme, string> = { dark: '深色', light: '淺色', high_contrast: '高對比', system: '跟隨系統' };
export type Density = 'comfortable' | 'compact';
export const DENSITY_LABEL: Record<Density, string> = { comfortable: '寬鬆', compact: '緊湊' };

type Prefs = {
	theme: Theme;
	density: Density;
	navMini: boolean;
	collabTab: 'ai' | 'activity';
	collabHidden: boolean;
};

const DEFAULTS: Prefs = { theme: 'dark', density: 'comfortable', navMini: false, collabTab: 'activity', collabHidden: false };

function read(key: string): Partial<Prefs> {
	try {
		return JSON.parse(localStorage.getItem(key) ?? '{}');
	} catch {
		return {};
	}
}

export const prefs = $state<Prefs>({ ...DEFAULTS });
let storeKey = 'ox.prefs.anon';

export const asTheme = (t: unknown): Theme => (THEMES.includes(t as Theme) ? (t as Theme) : 'dark');

export function loadPrefs(userId: string | null) {
	storeKey = `ox.prefs.${userId ?? 'anon'}`;
	const p = read(storeKey);
	prefs.theme = asTheme(p.theme);
	prefs.density = p.density === 'compact' ? 'compact' : 'comfortable';
	prefs.navMini = !!p.navMini;
	prefs.collabTab = p.collabTab === 'ai' ? 'ai' : 'activity';
	prefs.collabHidden = !!p.collabHidden;
	applyTheme();
}

export function savePrefs() {
	try {
		localStorage.setItem(storeKey, JSON.stringify(prefs));
		localStorage.setItem('ox.theme', prefs.theme);
	} catch {
		/* private mode: keep in memory */
	}
}

/** The colours actually shown: 「跟隨系統」 picks dark or light from the OS. */
export function resolvedTheme(t: Theme, prefersLight: boolean): 'dark' | 'light' | 'hc' {
	if (t === 'high_contrast') return 'hc';
	if (t === 'system') return prefersLight ? 'light' : 'dark';
	return t;
}

let mq: MediaQueryList | null = null;
export function applyTheme() {
	if (typeof document === 'undefined') return;
	if (!mq && typeof matchMedia === 'function') {
		mq = matchMedia('(prefers-color-scheme: light)');
		mq.addEventListener?.('change', () => prefs.theme === 'system' && applyTheme());
	}
	document.documentElement.dataset.theme = resolvedTheme(prefs.theme, !!mq?.matches);
	document.documentElement.dataset.density = prefs.density;
}

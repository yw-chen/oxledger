// SPDX-License-Identifier: GPL-2.0-or-later
// Per-person conveniences kept in this browser (decision P1-D3: there is no
// server endpoint for personal settings yet; gap G4). Nothing here is
// authority: losing it only resets the layout.

export type WidgetKind = 'decide' | 'agents' | 'mine' | 'milestones' | 'recent' | 'query';
export type Widget = { id: string; kind: WidgetKind; wide: boolean; title?: string; query?: string; project?: string };

export const WIDGET_TITLES: Record<WidgetKind, string> = {
	decide: '需要你決定',
	agents: 'Agent 今天做了什麼',
	mine: '指派給我',
	milestones: '里程碑',
	recent: '最近更新',
	query: '存起來的篩選'
};

export const DEFAULT_WIDGETS: Widget[] = [
	{ id: 'decide', kind: 'decide', wide: true },
	{ id: 'agents', kind: 'agents', wide: false },
	{ id: 'mine', kind: 'mine', wide: false },
	{ id: 'milestones', kind: 'milestones', wide: false },
	{ id: 'recent', kind: 'recent', wide: false }
];

type Prefs = {
	theme: 'dark' | 'light';
	navMini: boolean;
	collabTab: 'ai' | 'activity';
	collabHidden: boolean;
	widgets: Widget[];
};

const DEFAULTS: Prefs = { theme: 'dark', navMini: false, collabTab: 'activity', collabHidden: false, widgets: DEFAULT_WIDGETS };

function read(key: string): Partial<Prefs> {
	try {
		return JSON.parse(localStorage.getItem(key) ?? '{}');
	} catch {
		return {};
	}
}

export const prefs = $state<Prefs>({ ...DEFAULTS, widgets: [...DEFAULT_WIDGETS] });
let storeKey = 'ox.prefs.anon';

/** U22: "需要你決定" is always there and cannot be removed (only moved). */
export function normalizeWidgets(ws: Widget[] | undefined): Widget[] {
	const list = Array.isArray(ws) ? ws.filter((w) => w && w.kind in WIDGET_TITLES) : [...DEFAULT_WIDGETS];
	if (!list.some((w) => w.kind === 'decide')) list.unshift({ id: 'decide', kind: 'decide', wide: true });
	const seen = new Set<string>();
	return list.filter((w) => (seen.has(w.id) ? false : (seen.add(w.id), true)));
}

export function loadPrefs(userId: string | null) {
	storeKey = `ox.prefs.${userId ?? 'anon'}`;
	const p = read(storeKey);
	prefs.theme = p.theme === 'light' ? 'light' : 'dark';
	prefs.navMini = !!p.navMini;
	prefs.collabTab = p.collabTab === 'ai' ? 'ai' : 'activity';
	prefs.collabHidden = !!p.collabHidden;
	prefs.widgets = normalizeWidgets(p.widgets);
	applyTheme();
}

export function savePrefs() {
	prefs.widgets = normalizeWidgets(prefs.widgets);
	try {
		localStorage.setItem(storeKey, JSON.stringify(prefs));
		localStorage.setItem('ox.theme', prefs.theme);
	} catch {
		/* private mode: keep in memory */
	}
}

export function applyTheme() {
	if (typeof document !== 'undefined') document.documentElement.dataset.theme = prefs.theme;
}

export function toggleTheme() {
	prefs.theme = prefs.theme === 'dark' ? 'light' : 'dark';
	applyTheme();
	savePrefs();
}

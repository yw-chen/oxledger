// SPDX-License-Identifier: GPL-2.0-or-later
export type Toast = { id: number; text: string; kind: 'ok' | 'bad' | 'w' | '' };
export const toasts = $state<Toast[]>([]);
let n = 0;
export function toast(text: string, kind: Toast['kind'] = 'ok', ms = 4000) {
	const id = ++n;
	toasts.push({ id, text, kind });
	setTimeout(() => {
		const i = toasts.findIndex((t) => t.id === id);
		if (i >= 0) toasts.splice(i, 1);
	}, ms);
}

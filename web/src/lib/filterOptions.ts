// SPDX-License-Identifier: GPL-2.0-or-later
// Value choices for the condition editor (§3.2): small sets come in
// `options`; people and versions are looked up from `options_source`
// (`GET /api/v1/projects/<key>/assignees|versions`).

import type { FilterFieldInfo, FilterOption } from '$api/types';
import { api } from './api';

const cache = new Map<string, Promise<FilterOption[]>>();

export function sourceOptions(f: FilterFieldInfo): Promise<FilterOption[]> {
	if (f.options?.length) return Promise.resolve(f.options);
	const src = f.options_source ?? '';
	const m = /^GET \/api\/v1\/projects\/([^/]+)\/(assignees|versions)/.exec(src);
	if (!m) return Promise.resolve([]);
	const key = `${m[1]}/${m[2]}`;
	if (!cache.has(key)) {
		const p =
			m[2] === 'assignees'
				? api('GET /projects/{id}/assignees', { path: { id: m[1] } }).then((r) => r.assignees.map((a) => ({ id: a.id, label: a.kind === 'group' ? `${a.name}（群組）` : a.name })))
				: api('GET /projects/{id}/versions', { path: { id: m[1] }, query: { status: 'open,locked,closed' } }).then((r) => r.versions.map((v) => ({ id: v.id, label: v.effective_date ? `${v.name}（${v.effective_date}）` : v.name })));
		cache.set(
			key,
			p.catch(() => {
				cache.delete(key);
				return [];
			})
		);
	}
	return cache.get(key)!;
}

/** Tests and a project switch start clean. */
export function clearOptionCache() {
	cache.clear();
}

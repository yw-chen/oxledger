// SPDX-License-Identifier: GPL-2.0-or-later
// Trackers and custom field names. The server has no tracker list (gap G8):
// resolve("", tracker) matches every tracker and its aliases, deduplicated
// here. Custom field names come from the project's tracker schemas.

import type { Candidate } from '$api/types';
import type { Lookups } from './render';
import { api } from './api';

let trackers: Promise<Candidate[]> | null = null;

export function listTrackers(): Promise<Candidate[]> {
	trackers ??= api('POST /resolve', { body: { text: '', kind: 'tracker' } })
		.then((r) => {
			const seen = new Set<string>();
			return r.candidates.filter((c) => !seen.has(c.id) && seen.add(c.id));
		})
		.catch((e) => {
			trackers = null;
			throw e;
		});
	return trackers;
}

/** `cf:<id>` → name, for every custom field the caller may update in
 *  `projectId`; `delegableOnly`: only those an agent may be given (02 §3.5:
 *  never `visible = false` fields). */
export async function customFieldNames(projectId: string, delegableOnly = false): Promise<Map<string, string>> {
	const ts = await listTrackers();
	const schemas = await Promise.all(ts.map((t) => api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: projectId, tracker_id: t.id } }).catch(() => null)));
	const out = new Map<string, string>();
	for (const s of schemas) for (const f of s?.fields ?? []) if (f.key.startsWith('cf:') && f.ops.includes('update') && (!delegableOnly || f.delegable)) out.set(f.key, f.name ?? f.key);
	return out;
}

/** Field schemas, assignable people and milestones of several projects
 *  merged, to name the ids in a diff that spans them. */
export async function projectLookups(projectIds: string[]): Promise<Lookups> {
	const ts = await listTrackers().catch(() => [] as Candidate[]);
	const out: Lookups = { fields: [], assignees: [], versions: [] };
	const seen = new Set<string>();
	await Promise.all(
		projectIds.map(async (pid) => {
			const [schemas, a, v] = await Promise.all([
				Promise.all(ts.map((t) => api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: pid, tracker_id: t.id } }).catch(() => null))),
				api('GET /projects/{id}/assignees', { path: { id: pid } }).catch(() => null),
				api('GET /projects/{id}/versions', { path: { id: pid }, query: { status: 'open,locked,closed' } }).catch(() => null)
			]);
			for (const s of schemas)
				for (const f of s?.fields ?? []) {
					if (seen.has(f.key)) continue;
					seen.add(f.key);
					out.fields.push(f);
				}
			out.assignees.push(...(a?.assignees ?? []));
			out.versions.push(...(v?.versions ?? []));
		})
	);
	return out;
}

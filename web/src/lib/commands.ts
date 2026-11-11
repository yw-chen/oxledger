// SPDX-License-Identifier: GPL-2.0-or-later
// Human direct operations (human_direct, §6.8): one command document per
// action, with the lock_version the person saw as the precondition.

import type { CommandDoc, CommandResult, FieldSchema, OpInput, TargetRef } from '$api/types';
import { api } from './api';

export const issueRef = (key: string): TargetRef => ({ type: 'issue', key });

export function doc(ops: OpInput[], pre: { key: string; lock: number }[], title?: string): CommandDoc {
	return {
		command_schema_version: 1,
		ops,
		preconditions: pre.map((p) => ({ target: issueRef(p.key), lock_version: p.lock })),
		...(title ? { title } : {})
	};
}

/** Only the fields that differ from what the person started with. */
export function changedFields(before: Record<string, unknown>, after: Record<string, unknown>): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(after)) {
		const a = v === '' ? null : v;
		const b = before[k] === '' ? null : before[k];
		if (JSON.stringify(a ?? null) !== JSON.stringify(b ?? null)) out[k] = a ?? null;
	}
	return out;
}

export function submit(d: CommandDoc, operationId?: string): Promise<CommandResult> {
	return api('POST /commands', { body: d, operationId });
}

/** Custom fields go on the wire as `{"<type>": "<value>"}` (bool: a boolean),
 *  `null` clears; built-in fields go as they are. */
export function wireFields(set: Record<string, unknown>, fields: FieldSchema[]): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(set)) {
		if (!k.startsWith('cf:')) {
			out[k] = v;
			continue;
		}
		const t = fields.find((f) => f.key === k)?.type ?? 'string';
		out[k] = v === null || v === undefined || v === '' ? null : { [t]: t === 'bool' ? v : String(v) };
	}
	return out;
}

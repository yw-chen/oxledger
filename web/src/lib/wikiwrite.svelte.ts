// SPDX-License-Identifier: GPL-2.0-or-later
// Wiki writes (design 06 §3.11): one command document per action, and the
// last one the person made, so the wiki pages can offer its undo (R-W9)
// after the page they land on has changed.

import type { CommandResult, WikiEffect, WikiOp } from '$api/types';
import { api, newOperationId } from './api';
import { wikiDoc } from './wiki';

export const wikiWrite = $state<{ last: { executionId: string; projectKey: string; text: string } | null }>({ last: null });

/**
 * Sends one wiki op. `operationId` lets a retry of the same action reuse its
 * Idempotency-Key. The result's first wiki effect (the page written) comes
 * back with the execution; a write that changed something becomes the one
 * the undo bar offers.
 */
export async function runWiki(
	projectKey: string,
	op: WikiOp,
	pre: { pageId: string; lock: number } | null,
	text: string,
	operationId: string = newOperationId()
): Promise<{ result: CommandResult; effect: WikiEffect | null }> {
	const result = await api('POST /commands', { body: wikiDoc(op, pre), operationId });
	const effect = result.wiki?.[0] ?? null;
	if (!effect || effect.changed) wikiWrite.last = { executionId: result.execution_id, projectKey: projectKey.toUpperCase(), text };
	return { result, effect };
}

/**
 * R-W8 「回復到這一版」: `wiki_put` with `revert_from: n` on top of the current
 * version. The server copies version `n` (serialized with D12) and refuses a
 * redacted one (`invalid_value`, `kind = redacted_source`).
 */
export async function revertTo(projectKey: string, page: { id: string; title: string; lock_version: number }, n: number) {
	return runWiki(
		projectKey,
		{ kind: 'wiki_put', op_id: newOperationId(), target: { project: projectKey, page_id: page.id }, revert_from: n, comment: `回復到第 ${n} 版` },
		{ pageId: page.id, lock: page.lock_version },
		`回復到第 ${n} 版`
	);
}

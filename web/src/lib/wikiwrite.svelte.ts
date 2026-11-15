// SPDX-License-Identifier: GPL-2.0-or-later
// Wiki writes (design 06 §3.11): one command document per action, and the
// last one the person made, so the wiki pages can offer its undo (R-W9)
// after the page they land on has changed.

import type { CommandResult, ErrorBody, WikiEffect, WikiOp } from '$api/types';
import { api, ApiError, newOperationId } from './api';
import { urlTitle, wikiDoc } from './wiki';

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
 * R-W8 「回復到這一版」: `wiki_put` with version `n`'s content on top of the
 * current one. A redacted version is never a source (the server cannot tell
 * a revert from an edit, so this refuses before sending anything).
 */
export async function revertTo(projectKey: string, page: { id: string; title: string; lock_version: number }, n: number) {
	const v = await api('GET /projects/{id}/wiki/pages/{title}/versions/{n}', { path: { id: projectKey, title: urlTitle(page.title), n } });
	if (v.redacted) {
		const body = { code: 'invalid_value', message: 'redacted source', details: { kind: 'redacted_source' }, next_actions: [], retryable: false, request_id: '' };
		throw new ApiError(422, body as unknown as ErrorBody, null);
	}
	return runWiki(
		projectKey,
		{ kind: 'wiki_put', op_id: newOperationId(), target: { project: projectKey, page_id: page.id }, body: v.body, comment: `回復到第 ${n} 版` },
		{ pageId: page.id, lock: page.lock_version },
		`回復到第 ${n} 版`
	);
}

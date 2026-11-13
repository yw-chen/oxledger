// SPDX-License-Identifier: GPL-2.0-or-later
// Step-up (contract 02 §3.3, 05 §3.6.3/§3.7): some admin writes need the
// password again within 15 minutes. `stepped(fn, why)` asks once in a
// dialog (StepUpDialog in the layout), then runs `fn`; when the server still
// answers `reauthentication_required` it asks again and retries once.

import { ApiError } from './api';
import { steppedUpRecently } from './session.svelte';

export const stepUp = $state<{ why: string | null; resolve: ((ok: boolean) => void) | null }>({ why: null, resolve: null });

/** Opens the password dialog; true when the password was confirmed. */
export function askPassword(why: string): Promise<boolean> {
	stepUp.resolve?.(false);
	return new Promise((resolve) => {
		stepUp.why = why;
		stepUp.resolve = (ok) => {
			stepUp.why = null;
			stepUp.resolve = null;
			resolve(ok);
		};
	});
}

/** Runs `fn` after a fresh step-up; null when the person cancelled. */
export async function stepped<T>(fn: () => Promise<T>, why: string): Promise<T | null> {
	if (!steppedUpRecently() && !(await askPassword(why))) return null;
	try {
		return await fn();
	} catch (e) {
		if (!(e instanceof ApiError && e.code === 'reauthentication_required')) throw e;
		if (!(await askPassword(why))) return null;
		return fn();
	}
}

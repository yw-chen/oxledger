// SPDX-License-Identifier: GPL-2.0-or-later
// Typed client for /api/v1 (contract 02 §4). Every request and response type
// comes from the generated `api/types.ts` (D6): a route is named exactly as
// in `Endpoints`, e.g. api('GET /issues/{key}', { path: { key } }).

import type { Endpoints, ErrorBody, ErrorCode, NextAction } from '$api/types';

export type Route = keyof Endpoints;
export type Body<R extends Route> = Endpoints[R]['body'];
export type Data<R extends Route> = Endpoints[R]['data'];

/** A domain error (§4.3). Programs look at `code`, `nextActions`, `details`. */
export class ApiError extends Error {
	readonly code: ErrorCode | 'network' | 'internal_error';
	readonly status: number;
	readonly body: ErrorBody | null;
	/** The Idempotency-Key the request carried (to look the result up later). */
	readonly operationId: string | null;
	constructor(status: number, body: ErrorBody | null, operationId: string | null, fallback?: string) {
		super(body?.message ?? fallback ?? 'request failed');
		this.status = status;
		this.body = body;
		this.code = (body?.code as ErrorCode) ?? (status === 0 ? 'network' : 'internal_error');
		this.operationId = operationId;
	}
	get nextActions(): NextAction[] {
		return this.body?.next_actions ?? [];
	}
	get details(): Record<string, unknown> {
		return (this.body?.details as Record<string, unknown>) ?? {};
	}
	get retryable(): boolean {
		return this.body?.retryable ?? this.status === 0;
	}
}

type Opts<R extends Route> = {
	path?: Record<string, string | number>;
	query?: Record<string, string | number | boolean | null | undefined | string[]>;
	/** Idempotency-Key for this logical action; reuse it to retry (§8.7). */
	operationId?: string;
	signal?: AbortSignal;
} & (Body<R> extends never ? { body?: undefined } : { body: Body<R> });

let csrfToken: string | null = null;
let onUnauthenticated: (() => void) | null = null;
let onCsrfFailed: (() => Promise<boolean>) | null = null;

export function setCsrf(token: string | null) {
	csrfToken = token;
}
export function hasCsrf() {
	return csrfToken !== null;
}
/** The session's CSRF token, for requests that are not JSON (uploads). */
export function csrf(): string | null {
	return csrfToken;
}
/** Called once when any request answers `unauthenticated` (session ended). */
export function setUnauthenticatedHandler(fn: (() => void) | null) {
	onUnauthenticated = fn;
}

/**
 * Called when a write answers `csrf_failed` (the session was replaced, e.g.
 * signed out and in again in another tab). Resolves true when a fresh token
 * for the same person is in place, so the write is sent once more with the
 * same Idempotency-Key (csrf_failed changed nothing).
 */
export function setCsrfFailedHandler(fn: (() => Promise<boolean>) | null) {
	onCsrfFailed = fn;
}

/** For requests sent outside `api()` (uploads): the same session handling. */
export function sessionEnded() {
	onUnauthenticated?.();
}
/** For requests sent outside `api()`: true when a fresh CSRF token is in place. */
export async function recoverCsrf(): Promise<boolean> {
	return onCsrfFailed ? onCsrfFailed().catch(() => false) : false;
}

export function newOperationId(): string {
	return crypto.randomUUID();
}

export function buildUrl(route: string, path?: Opts<Route>['path'], query?: Opts<Route>['query']) {
	const [, tpl] = route.split(' ', 2);
	let p = tpl.replace(/\{(\w+)\}/g, (_, k: string) => {
		const v = path?.[k];
		if (v === undefined) throw new Error(`missing path parameter ${k} for ${route}`);
		return encodeURIComponent(String(v));
	});
	const qs = new URLSearchParams();
	for (const [k, v] of Object.entries(query ?? {})) {
		if (v === undefined || v === null || v === '') continue;
		qs.set(k, Array.isArray(v) ? v.join(',') : String(v));
	}
	const s = qs.toString();
	return `/api/v1${p}${s ? `?${s}` : ''}`;
}

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;
let fetcher: FetchLike = (i, init) => fetch(i, init);
/** Tests replace the transport. */
export function setFetch(f: FetchLike) {
	fetcher = f;
}

export async function api<R extends Route>(route: R, opts: Opts<R> = {} as Opts<R>): Promise<Data<R>> {
	try {
		return await send(route, opts);
	} catch (e) {
		if (!(e instanceof ApiError) || e.code !== 'csrf_failed' || !onCsrfFailed || route.includes(' /auth/')) throw e;
		if (!(await onCsrfFailed().catch(() => false))) throw e;
		return send(route, { ...opts, operationId: e.operationId ?? opts.operationId } as Opts<R>);
	}
}

async function send<R extends Route>(route: R, opts: Opts<R>): Promise<Data<R>> {
	const method = route.split(' ', 1)[0];
	const url = buildUrl(route, opts.path, opts.query);
	const headers: Record<string, string> = { Accept: 'application/json' };
	let operationId: string | null = null;
	let body: string | undefined;
	if (method !== 'GET') {
		headers['Content-Type'] = 'application/json';
		if (csrfToken) headers['X-CSRF-Token'] = csrfToken;
		operationId = opts.operationId ?? newOperationId();
		headers['Idempotency-Key'] = operationId;
		body = JSON.stringify(opts.body ?? {});
	}
	let resp: Response;
	try {
		resp = await fetcher(url, { method, headers, body, credentials: 'same-origin', signal: opts.signal });
	} catch (e) {
		if ((e as Error)?.name === 'AbortError') throw e;
		throw new ApiError(0, null, operationId, '無法連線');
	}
	let json: unknown = null;
	try {
		json = await resp.json();
	} catch {
		/* empty or not JSON */
	}
	if (resp.ok && json && typeof json === 'object' && 'data' in json) {
		return (json as { data: Data<R> }).data;
	}
	const err = (json as { error?: ErrorBody } | null)?.error ?? null;
	const e = new ApiError(resp.status, err, operationId, `HTTP ${resp.status}`);
	if (e.code === 'unauthenticated' && route !== 'GET /auth/session') onUnauthenticated?.();
	throw e;
}

/** Runs `fn` over `items` with at most `limit` in flight, keeping order (§15.3: 4). */
export async function pool<T, U>(items: T[], limit: number, fn: (t: T, i: number) => Promise<U>, stop?: () => boolean) {
	const out: (U | undefined)[] = new Array(items.length);
	let next = 0;
	const worker = async () => {
		while (next < items.length) {
			if (stop?.()) return;
			const i = next++;
			out[i] = await fn(items[i], i);
		}
	};
	await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
	return out;
}

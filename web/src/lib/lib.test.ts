import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, ApiError, buildUrl, pool, setCsrf, setFetch, setUnauthenticatedHandler } from './api';
import { changedFields, doc, wireFields } from './commands';
import { pendingCreated, rememberCreated } from './created.svelte';
import { describeError } from './errors';
import { splitHighlights } from './highlight';
import { route } from './intent';
import { normalizeWidgets } from './prefs.svelte';
import type { FieldSchema } from '$api/types';

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

describe('api client', () => {
	afterEach(() => {
		setCsrf(null);
		setUnauthenticatedHandler(null);
		setFetch((i, init) => fetch(i, init));
	});

	it('builds paths and drops empty query values', () => {
		expect(buildUrl('GET /issues/{key}', { key: 'AUTH 1' })).toBe('/api/v1/issues/AUTH%201');
		expect(buildUrl('GET /x', undefined, { a: 'b', c: null, d: '', e: ['x', 'y'], f: false })).toBe('/api/v1/x?a=b&e=x%2Cy&f=false');
		expect(() => buildUrl('GET /issues/{key}', {})).toThrow(/missing path parameter key/);
	});

	it('GET carries no CSRF or Idempotency-Key; writes carry both', async () => {
		const seen: RequestInit[] = [];
		setFetch(async (_u, init) => {
			seen.push(init);
			return json(200, { data: { ok: 1 }, meta: {} });
		});
		setCsrf('tok');
		await api('GET /auth/session');
		await api('POST /commands', { body: doc([], []), operationId: 'op-1' });
		const g = seen[0].headers as Record<string, string>;
		const p = seen[1].headers as Record<string, string>;
		expect(g['X-CSRF-Token']).toBeUndefined();
		expect(g['Idempotency-Key']).toBeUndefined();
		expect(p['X-CSRF-Token']).toBe('tok');
		expect(p['Idempotency-Key']).toBe('op-1');
		expect(p['Content-Type']).toBe('application/json');
	});

	it('turns error bodies into ApiError and reports session end once', async () => {
		const ended = vi.fn();
		setUnauthenticatedHandler(ended);
		setFetch(async () => json(401, { error: { code: 'unauthenticated', message: 'no', retryable: false, request_id: 'r' } }));
		const e = await api('GET /projects').catch((x) => x);
		expect(e).toBeInstanceOf(ApiError);
		expect(e.code).toBe('unauthenticated');
		expect(ended).toHaveBeenCalledTimes(1);
		// The session probe itself does not count as "the session ended".
		await api('GET /auth/session').catch(() => {});
		expect(ended).toHaveBeenCalledTimes(1);
	});

	it('a network failure is retryable and keeps the operation id', async () => {
		setFetch(async () => {
			throw new TypeError('fetch failed');
		});
		const e = (await api('POST /commands', { body: doc([], []), operationId: 'op-9' }).catch((x) => x)) as ApiError;
		expect(e.code).toBe('network');
		expect(e.retryable).toBe(true);
		expect(e.operationId).toBe('op-9');
	});

	it('pool keeps order and never exceeds the limit', async () => {
		let live = 0;
		let peak = 0;
		const out = await pool([1, 2, 3, 4, 5, 6, 7], 4, async (n) => {
			live++;
			peak = Math.max(peak, live);
			await new Promise((r) => setTimeout(r, 5));
			live--;
			return n * 2;
		});
		expect(out).toEqual([2, 4, 6, 8, 10, 12, 14]);
		expect(peak).toBe(4);
	});

	it('pool stops starting new work when asked', async () => {
		let started = 0;
		let stop = false;
		await pool([1, 2, 3, 4, 5, 6], 1, async () => {
			started++;
			if (started === 2) stop = true;
		}, () => stop);
		expect(started).toBe(2);
	});
});

describe('commands', () => {
	const fields = [
		{ key: 'cf:1', type: 'list', ops: [], required: false },
		{ key: 'cf:2', type: 'float', ops: [], required: false },
		{ key: 'cf:3', type: 'bool', ops: [], required: false },
		{ key: 'cf:4', type: 'date', ops: [], required: false }
	] as FieldSchema[];

	it('only sends what changed; empty means cleared', () => {
		expect(changedFields({ a: 1, b: 'x', c: null }, { a: 1, b: '', c: null })).toEqual({ b: null });
		expect(changedFields({ a: null }, { a: '' })).toEqual({});
	});

	it('custom fields are typed on the wire, built-ins are not', () => {
		expect(wireFields({ subject: 's', 'cf:1': 'id-1', 'cf:2': 3.5, 'cf:3': false, 'cf:4': '2026-12-31', 'cf:9': 'x' }, fields)).toEqual({
			subject: 's',
			'cf:1': { list: 'id-1' },
			'cf:2': { float: '3.5' },
			'cf:3': { bool: false },
			'cf:4': { date: '2026-12-31' },
			'cf:9': { string: 'x' }
		});
		expect(wireFields({ 'cf:1': null, 'cf:2': '' }, fields)).toEqual({ 'cf:1': null, 'cf:2': null });
	});

	it('one precondition per existing issue', () => {
		const d = doc([], [{ key: 'AUTH-1', lock: 3 }], 't');
		expect(d).toEqual({ command_schema_version: 1, ops: [], preconditions: [{ target: { type: 'issue', key: 'AUTH-1' }, lock_version: 3 }], title: 't' });
	});
});

describe('just created (Q-S10)', () => {
	it('stays until a search returns it', () => {
		sessionStorage.clear();
		rememberCreated({ key: 'AUTH-9', subject: 'x', after: 'c1.1.1' });
		rememberCreated({ key: 'AUTH-10', subject: 'y', after: 'c1.1.2' });
		expect(pendingCreated(['AUTH-1']).map((c) => c.key)).toEqual(['AUTH-10', 'AUTH-9']);
		expect(pendingCreated(['AUTH-9']).map((c) => c.key)).toEqual(['AUTH-10']);
		expect(pendingCreated([]).map((c) => c.key)).toEqual(['AUTH-10']);
	});
	it('expires after ten minutes', () => {
		sessionStorage.clear();
		vi.useFakeTimers();
		rememberCreated({ key: 'AUTH-9', subject: 'x', after: 'c' });
		vi.advanceTimersByTime(11 * 60 * 1000);
		expect(pendingCreated([])).toEqual([]);
		vi.useRealTimers();
	});
});

describe('highlights', () => {
	it('cuts by code points and never splits a surrogate pair', () => {
		const parts = splitHighlights('a𠮷b登入c', [{ start: 1, end: 2, strength: 'primary' } as never, { start: 3, end: 5, strength: 'fallback' } as never]);
		expect(parts).toEqual([
			{ text: 'a', mark: null },
			{ text: '𠮷', mark: 'primary' },
			{ text: 'b', mark: null },
			{ text: '登入', mark: 'fallback' },
			{ text: 'c', mark: null }
		]);
	});
	it('ignores overlaps and out-of-range ends', () => {
		const parts = splitHighlights('abcdef', [{ start: 0, end: 3, strength: 'primary' } as never, { start: 2, end: 99, strength: 'primary' } as never]);
		expect(parts.map((p) => p.text).join('')).toBe('abcdef');
		expect(parts).toEqual([
			{ text: 'abc', mark: 'primary' },
			{ text: 'def', mark: 'primary' }
		]);
	});
});

describe('⌘K intent (01 §15.11)', () => {
	it('every input gets a route kind', () => {
		expect(route('')).toBeNull();
		expect(route('auth-12')).toMatchObject({ kind: 'issue_open', href: '/issue/AUTH-12' });
		expect(route('project:AUTH status:open')).toMatchObject({ kind: 'structured_search' });
		expect(route('/new auth')).toMatchObject({ kind: 'command_draft', href: '/new?project=AUTH' });
		expect(route('/delete AUTH-1')).toMatchObject({ kind: 'clarify', href: null });
		expect(route('AUTH-12 改成 bob')).toMatchObject({ kind: 'clarify' });
		expect(route('登入失敗')).toMatchObject({ kind: 'text_search', note: '內建 AI 未啟用：以文字搜尋' });
		expect(route('登入失敗', true)).toMatchObject({ kind: 'ai_task' });
		expect(route('a\nb')).toMatchObject({ kind: 'clarify' });
		expect(route('x'.repeat(501))).toMatchObject({ kind: 'clarify' });
	});
	it('a broken query is never sent anywhere', () => {
		const r = route('status:');
		expect(r).toMatchObject({ kind: 'structured_search', href: null, errorAt: 7 });
	});
	it('the built-in AI choice is shown disabled', () => {
		const r = route('AUTH-1 和 AUTH-2 合併');
		expect(r?.choices?.find((c) => c.label.includes('AI'))?.disabled).toBe('內建 AI 未啟用');
	});
});

describe('errors (02 §15.5)', () => {
	const err = (code: string, details: unknown = {}, retryable = false) =>
		new ApiError(409, { code, details, message: 'm', retryable, request_id: 'r' } as never, null);
	it('says whether anything changed and the next step', () => {
		expect(describeError(err('conflict'))).toMatchObject({ changed: '沒有改到任何資料。', action: 'conflict' });
		expect(describeError(new ApiError(0, null, 'op'))).toMatchObject({ changed: '尚未確認是否完成。', action: 'retry' });
		expect(describeError(new Error('x'))).toMatchObject({ action: 'refresh' });
		expect(describeError(err('context_unavailable', { kind: 'closed' })).title).toContain('已關閉');
	});
	it('every error code the server can send has its own wording', async () => {
		const src = await import('../../../api/types.ts?raw').catch(() => null);
		const m = (src?.default as string | undefined)?.match(/export type ErrorCode =([^;]+);/);
		expect(m).toBeTruthy();
		const codes = [...m![1].matchAll(/"([a-z_]+)"/g)].map((x) => x[1]);
		expect(codes.length).toBeGreaterThan(10);
		const generic = describeError(err('__nope__')).title;
		const missing = codes.filter((c) => describeError(err(c)).title === generic);
		expect(missing).toEqual([]);
	});
});

describe('home widgets (U22)', () => {
	it('"需要你決定" cannot be removed', () => {
		expect(normalizeWidgets([{ id: 'm', kind: 'mine', wide: false }])[0].kind).toBe('decide');
		expect(normalizeWidgets(undefined).some((w) => w.kind === 'decide')).toBe(true);
	});
	it('drops unknown kinds and duplicate ids', () => {
		const ws = normalizeWidgets([
			{ id: 'd', kind: 'decide', wide: true },
			{ id: 'x', kind: 'bogus' as never, wide: false },
			{ id: 'd', kind: 'decide', wide: true }
		]);
		expect(ws.map((w) => w.id)).toEqual(['d']);
	});
});

import { sameValue, show, untyped } from './render';
describe('render (names, not ids)', () => {
	const look = {
		fields: [{ key: 'cf:1', name: 'Severity', type: 'list', ops: [], required: false, options: [{ id: 'o1', label: 'High', active: true }] }] as never,
		assignees: [{ id: 'u1', name: 'Bob Chen', kind: 'user' }] as never,
		versions: [{ id: 'v1', name: 'v1.0' }] as never
	};
	it('unwraps typed custom values and names ids', () => {
		expect(untyped({ list: 'o1' })).toBe('o1');
		expect(untyped({ id: 'x', name: 'y' })).toEqual({ id: 'x', name: 'y' });
		expect(show('cf:1', { list: 'o1' }, look)).toBe('High');
		expect(show('assigned_to_id', 'u1', look)).toBe('Bob Chen');
		expect(show('fixed_version_id', 'v1', look)).toBe('v1.0');
		expect(show('cf:2', { float: '3.5' }, look)).toBe('3.5');
		expect(show('due_date', null, look)).toBe('（空）');
		expect(show('cf:3', { bool: true }, look)).toBe('是');
	});
	it('compares typed and plain values', () => {
		expect(sameValue({ float: '5' }, 5)).toBe(true);
		expect(sameValue({ float: '5.0' }, 5)).toBe(true);
		expect(sameValue('', null)).toBe(true);
		expect(sameValue({ list: 'o1' }, 'o1')).toBe(true);
		expect(sameValue('a', 'b')).toBe(false);
	});
});

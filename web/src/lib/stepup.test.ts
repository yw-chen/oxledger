import { describe, expect, it, vi } from 'vitest';
import { ApiError } from './api';

const recent = vi.hoisted(() => ({ value: false }));
vi.mock('./session.svelte', () => ({ steppedUpRecently: () => recent.value }));
const { stepUp, stepped } = await import('./stepup.svelte');

const reauth = () => new ApiError(403, { code: 'reauthentication_required', message: 'x' } as never, null);
const tick = () => new Promise((r) => setTimeout(r, 0));

describe('stepped (step-up before a sensitive write)', () => {
	it('asks for the password first, then runs', async () => {
		recent.value = false;
		const fn = vi.fn(async () => 'done');
		const p = stepped(fn, '為了刪除');
		await tick();
		expect(stepUp.why).toBe('為了刪除');
		expect(fn).not.toHaveBeenCalled();
		stepUp.resolve!(true);
		expect(await p).toBe('done');
		expect(stepUp.why).toBeNull();
	});
	it('cancel runs nothing', async () => {
		recent.value = false;
		const fn = vi.fn(async () => 'done');
		const p = stepped(fn, 'x');
		await tick();
		stepUp.resolve!(false);
		expect(await p).toBeNull();
		expect(fn).not.toHaveBeenCalled();
	});
	it('within 15 minutes it does not ask, but asks again when the server still wants it', async () => {
		recent.value = true;
		let n = 0;
		const fn = vi.fn(async () => {
			if (n++ === 0) throw reauth();
			return 'ok';
		});
		const p = stepped(fn, 'x');
		await tick();
		expect(stepUp.why).toBe('x');
		stepUp.resolve!(true);
		expect(await p).toBe('ok');
		expect(fn).toHaveBeenCalledTimes(2);
	});
	it('other errors pass through', async () => {
		recent.value = true;
		await expect(stepped(async () => Promise.reject(new Error('boom')), 'x')).rejects.toThrow('boom');
	});
});

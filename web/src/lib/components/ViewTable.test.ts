import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import type { RunInput, ViewRun } from '$api/types';
import { ApiError } from '$lib/api';
import ViewTable from './ViewTable.svelte';

const cols = [
	{ key: 'key', label: '單號' },
	{ key: 'subject', label: '標題' }
];
const item = (id: string, group: unknown) => ({ id, key: id, lock_version: 1, group, values: { subject: `標題 ${id}` } });
const page = (items: ReturnType<typeof item>[], groups: { value: unknown; label: string; count: { value: number; capped: boolean } }[], next: string | null) =>
	({ columns: cols, groups, items, next_cursor: next, totals: null, coverage: null }) as unknown as ViewRun;

describe('ViewTable', () => {
	it('keeps group heads of earlier pages and shows a returning item once', async () => {
		const run = vi.fn(async (i: RunInput): Promise<ViewRun> => {
			if (!i.cursor) return page([item('A-1', 'alice'), item('A-2', 'bob')], [{ value: 'alice', label: 'Alice', count: { value: 1, capped: false } }, { value: 'bob', label: 'Bob', count: { value: 2, capped: false } }], 'c2');
			return page([item('A-2', 'bob'), item('A-3', 'bob'), item('A-4', null)], [{ value: 'bob', label: 'Bob', count: { value: 2, capped: false } }, { value: null, label: '未指派', count: { value: 1, capped: false } }], null);
		});
		render(ViewTable, { run, reloadKey: 'k' });
		await screen.findByTestId('view-more');
		await fireEvent.click(screen.getByTestId('view-more'));
		await waitFor(() => expect(screen.getAllByTestId('view-row')).toHaveLength(4));
		const heads = screen.getAllByTestId('view-group').map((g) => g.textContent?.replace(/\s+/g, ' ').trim());
		expect(heads).toEqual(['Alice · 1', 'Bob · 2', '未指派 · 1']);
	});

	it('reloads from the top once when the cursor went stale', async () => {
		let calls = 0;
		const run = vi.fn(async (i: RunInput): Promise<ViewRun> => {
			calls++;
			if (i.cursor) throw new ApiError(400, { code: 'invalid_cursor', message: 'stale', details: {} } as never, null);
			return page([item(`A-${calls}`, null)], [], 'c2');
		});
		const onerror = vi.fn();
		render(ViewTable, { run, reloadKey: 'k', onerror });
		await screen.findByTestId('view-more');
		await fireEvent.click(screen.getByTestId('view-more'));
		await waitFor(() => expect(run).toHaveBeenCalledTimes(3));
		expect(run.mock.calls[2][0].cursor).toBeNull();
		// The table shows the fresh first page, no error box.
		await waitFor(() => expect(screen.getAllByTestId('view-row')).toHaveLength(1));
		expect(onerror).not.toHaveBeenCalledWith(expect.any(ApiError));
		expect(onerror).toHaveBeenLastCalledWith(null);
	});

	it('reports an error, then clears it after a successful run', async () => {
		let fail = true;
		const run = vi.fn(async (): Promise<ViewRun> => {
			if (fail) throw new ApiError(400, { code: 'invalid_filter', message: 'x', details: { reason: 'field_unavailable' } } as never, null);
			return page([item('A-1', null)], [], null);
		});
		const onerror = vi.fn();
		const r = render(ViewTable, { run, reloadKey: 'k1', onerror });
		await waitFor(() => expect(onerror).toHaveBeenCalledWith(expect.any(ApiError)));
		fail = false;
		await r.rerender({ run, reloadKey: 'k2', onerror });
		await waitFor(() => expect(onerror).toHaveBeenLastCalledWith(null));
	});
});

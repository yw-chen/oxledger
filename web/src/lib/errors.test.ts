import { describe, expect, it } from 'vitest';
import { ApiError } from './api';
import { describeError, FORBIDDEN, isForbidden, NOT_FOUND } from './errors';

const err = (code: string, details: unknown = {}) => new ApiError(409, { code, details, message: 'm', retryable: false, request_id: 'r' } as never, null);

describe('§6 shared states', () => {
	it('says not_found and forbidden_op the §6 way', () => {
		expect(describeError(err('not_found')).title).toBe('找不到這個頁面，或你沒有權限查看。');
		expect(NOT_FOUND).toBe('找不到這個頁面，或你沒有權限查看。');
		expect(describeError(err('forbidden_op')).title).toBe('你沒有權限執行這個動作');
		expect(FORBIDDEN).toBe('你沒有權限執行這個動作');
		expect(isForbidden(err('forbidden_op'))).toBe(true);
		expect(isForbidden(err('not_found'))).toBe(false);
		expect(isForbidden(new Error('x'))).toBe(false);
	});
	it('query_timeout offers 「縮小範圍」', () => {
		expect(describeError(err('query_timeout'))).toMatchObject({ next: '縮小範圍', action: 'narrow', changed: '沒有改到任何資料。' });
	});
});

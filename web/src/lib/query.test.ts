import { describe, expect, it } from 'vitest';
import { hasConditions, normalized, parse, SyntaxError_, toParams } from './query';

// Mirrors crates/oxledger/src/query_syntax.rs tests (same grammar, same positions).
describe('query syntax', () => {
	it('conditions and text', () => {
		const q = parse('  login  Status:open project:AUTH tracker:"Feature request" crash ');
		expect(q.projects).toEqual(['AUTH']);
		expect(q.status).toBe('open');
		expect(q.tracker).toBe('Feature request');
		expect(q.text).toEqual(['login', 'crash']);
		expect(normalized(q)).toBe('project:AUTH status:open tracker:"Feature request" login crash');
		expect(hasConditions(q)).toBe(true);
	});

	it('free text keeps colons and CJK', () => {
		const q = parse('https://x.test/a 登入 失敗 foo:bar');
		expect(q.text).toEqual(['https://x.test/a', '登入', '失敗', 'foo:bar']);
		expect(q.projects).toEqual([]);
		expect(hasConditions(q)).toBe(false);
		expect(parse('')).toEqual({ projects: [], text: [] });
	});

	const pos = (s: string) => {
		try {
			parse(s);
		} catch (e) {
			expect(e).toBeInstanceOf(SyntaxError_);
			return (e as SyntaxError_).position;
		}
		throw new Error(`no error for ${s}`);
	};
	it('errors have the same positions as the server', () => {
		expect(pos('status:')).toBe(7);
		expect(pos('a tracker:"open')).toBe(10);
		expect(pos('status:a status:b')).toBe(9);
		expect(pos('say "hi"')).toBe(4);
		expect(pos('tracker:"a"b')).toBe(11);
		expect(pos('status:a"b')).toBe(8);
		expect(parse('project:A project:B').projects).toEqual(['A', 'B']);
	});

	it('positions count code points, not UTF-16 units', () => {
		// 𠮷 is one code point (two UTF-16 units).
		expect(pos('𠮷 status:')).toBe(9);
	});

	it('status categories go to status_category', () => {
		expect(toParams(parse('status:Open 登入'))).toMatchObject({ status_category: 'open', status: null, q: '登入' });
		expect(toParams(parse('status:Resolved'))).toMatchObject({ status_category: null, status: 'Resolved', q: null });
		expect(toParams(parse('project:AUTH assignee:me version:v1'))).toMatchObject({ project: 'AUTH', assignee: 'me', version: 'v1' });
	});

	it('a second project is an error at its position', () => {
		const input = 'login project:A  project:B';
		try {
			toParams(parse(input), input);
			throw new Error('expected an error');
		} catch (e) {
			expect(e).toBeInstanceOf(SyntaxError_);
			expect((e as SyntaxError_).position).toBe(17);
		}
	});
});

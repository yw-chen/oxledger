import { describe, expect, it } from 'vitest';
import { blankCode, bytesText, codeBlock, filterPeople, formatFor, insertMention, mentionChips, mentionQuery, mentionsIn, personFor, prefixLines, table, utf8Bytes, wrapSel } from './markdown';

const people = [
	{ id: 'u1', name: 'Alice Wang', login: 'alice', kind: 'user' },
	{ id: 'u2', name: 'Bob Chen', login: 'bob', kind: 'user' },
	{ id: 'u3', name: 'Bobby Lin', login: 'Bobby', kind: 'user' },
	{ id: 'u4', name: 'bobby lee', login: 'bobby', kind: 'user' },
	{ id: 'g1', name: 'Backend', login: 'backend', kind: 'group' },
	{ id: 'u5', name: '陳小明', login: 'ming.chen', kind: 'user' }
];

describe('mentions (R-MN1, R-MN2)', () => {
	it('finds logins after a line start or a separator, once each', () => {
		expect(mentionsIn('@alice 請看，也請 @bob 和@alice 確認')).toEqual(['alice', 'bob']);
		expect(mentionsIn('（@bob）')).toEqual(['bob']);
	});
	it('skips e-mail addresses, agent handles and words glued to the login', () => {
		expect(mentionsIn('寫信到 a@bob.example')).toEqual([]);
		expect(mentionsIn('@claude-code@alice 做的')).toEqual([]);
		expect(mentionsIn('x.@bob')).toEqual([]);
	});
	it('a sentence-ending dot is not part of the login; inner dots are', () => {
		expect(mentionsIn('謝謝 @bob.')).toEqual(['bob']);
		expect(mentionsIn('問 @ming.chen 吧')).toEqual(['ming.chen']);
	});
	it('nothing inside code spans or code blocks', () => {
		expect(mentionsIn('用 `@alice` 這個字')).toEqual([]);
		expect(mentionsIn('```\n@alice\n```\n@bob')).toEqual(['bob']);
		expect(mentionsIn('~~~~\n@alice\n~~~\n@carol\n~~~~\n@bob')).toEqual(['bob']);
		expect(mentionsIn('段落\n\n    @alice 縮排程式碼\n\n@bob')).toEqual(['bob']);
	});
	it('an indented line right after a paragraph is not code', () => {
		expect(mentionsIn('段落\n    @alice 續行')).toEqual(['alice']);
	});
	it('a code span does not reach across a blank line', () => {
		expect(mentionsIn('一個 ` 反引號\n\n@alice 看 `x`')).toEqual(['alice']);
	});
	it('links, URLs and markers glued before @ mention nobody', () => {
		expect(mentionsIn('見 https://x.example/@alice 與 <https://y.example/@bob>')).toEqual([]);
		expect(mentionsIn('[ping @alice](https://x.example) 給 @bob')).toEqual(['bob']);
		expect(mentionsIn('**x**@alice 與 `x`@bob')).toEqual([]);
	});
	it('blankCode keeps the length', () => {
		const t = 'a `b` c\n```\nx\n```';
		expect(blankCode(t)).toHaveLength(t.length);
	});
});

describe('who a login names', () => {
	it('exact first, else one case-insensitive match; groups never', () => {
		expect(personFor('bob', people)?.id).toBe('u2');
		expect(personFor('Bobby', people)?.id).toBe('u3');
		expect(personFor('BOBBY', people)).toBeNull();
		expect(personFor('ALICE', people)?.id).toBe('u1');
		expect(personFor('Backend', people)).toBeNull();
		expect(personFor('backend', people)).toBeNull();
	});
	it('people without a login (G48) are never matched', () => {
		expect(personFor('carol', [{ id: 'x', name: 'carol', kind: 'user' }])).toBeNull();
	});
	it('chips: each person once, unknown logins dropped', () => {
		expect(mentionChips('@alice @nobody @ALICE @bob', people).map((p) => p.id)).toEqual(['u1', 'u2']);
	});
});

describe('autocomplete', () => {
	it('opens from the second character after @', () => {
		expect(mentionQuery('請 @b', 4)).toBeNull();
		expect(mentionQuery('請 @bo', 5)).toEqual({ start: 2, q: 'bo' });
		expect(mentionQuery('@陳小', 3)).toEqual({ start: 0, q: '陳小' });
		expect(mentionQuery('a@bob', 5)).toBeNull();
		expect(mentionQuery('@bo b', 5)).toBeNull();
	});
	it('matches login prefixes and names, prefix matches first, no groups', () => {
		expect(filterPeople(people, 'bo').map((p) => p.id)).toEqual(['u2', 'u4', 'u3']);
		expect(filterPeople(people, '小明').map((p) => p.id)).toEqual(['u5']);
		expect(filterPeople(people, 'back')).toEqual([]);
	});
	it('replaces the query with the login and a space', () => {
		expect(insertMention('請 @bo 看', 2, 5, 'bob')).toEqual({ text: '請 @bob  看', caret: 7 });
	});
});

describe('toolbar edits', () => {
	it('wraps the selection or a placeholder', () => {
		expect(wrapSel('a b c', 2, 3, '**', '**', '粗體')).toEqual({ text: 'a **b** c', selStart: 4, selEnd: 5 });
		expect(wrapSel('ab', 1, 1, '[', '](https://)', '連結文字')).toEqual({ text: 'a[連結文字](https://)b', selStart: 2, selEnd: 6 });
	});
	it('prefixes every touched line, numbering ordered lists', () => {
		expect(prefixLines('x\none\ntwo\ny', 3, 8, '- ').text).toBe('x\n- one\n- two\ny');
		expect(prefixLines('one\ntwo', 0, 7, 'n. ').text).toBe('1. one\n2. two');
		expect(prefixLines('one\ntwo\n', 0, 8, '> ').text).toBe('> one\n> two\n');
		expect(prefixLines('\nab', 0, 0, '# ').text).toBe('# \nab');
	});
	it('code blocks and tables start on their own line', () => {
		expect(codeBlock('ab', 1, 2).text).toBe('a\n```\nb\n```\n');
		expect(table('ab', 2).text).toBe('ab\n\n| 欄位 | 欄位 |\n|---|---|\n|  |  |\n');
	});
});

describe('limits and formats', () => {
	it('counts UTF-8 bytes', () => {
		expect(utf8Bytes('ab')).toBe(2);
		expect(utf8Bytes('中文')).toBe(6);
		expect(bytesText(2048, 65536)).toBe('2 KiB / 64 KiB');
	});
	it('only a changed plain description converts to markdown (R-M9)', () => {
		expect(formatFor('plain', true)).toBe('markdown');
		expect(formatFor('plain', false)).toBeUndefined();
		expect(formatFor('markdown', true)).toBeUndefined();
	});
});

import { describe, expect, it } from 'vitest';
import { appendPage, expiringProjects, linkTarget, pageRef, quotaText, renderMd, sseParser, stopTail, withConv, writeLine } from './aichat';

const O = 'http://localhost:5174';

describe('SSE', () => {
	it('parses events across chunks, keeps ids, skips pings', () => {
		const feed = sseParser();
		expect(feed('id: 1\nevent: run.started\ndata: {"run_id":"r"}\n\n: ping\n\nid: 2\nevent: text.del')).toEqual([{ id: 1, event: 'run.started', data: '{"run_id":"r"}' }]);
		expect(feed('ta\ndata: {"text":"AUTH-1 "}\r\n\r\n')).toEqual([{ id: 2, event: 'text.delta', data: '{"text":"AUTH-1 "}' }]);
	});
	it('joins multi-line data and defaults the event name', () => {
		const feed = sseParser();
		expect(feed('data: a\ndata: b\n\n')).toEqual([{ id: null, event: 'message', data: 'a\nb' }]);
	});
	it('a \\r\\n split across chunks is one line break', () => {
		const feed = sseParser();
		expect(feed('event: x\r\ndata: 1\r')).toEqual([]);
		expect(feed('\n\r\n')).toEqual([{ id: null, event: 'x', data: '1' }]);
	});
	it('skips comments; an event without data still counts', () => {
		const feed = sseParser();
		expect(feed(': ping\n\n')).toEqual([]);
		expect(feed(':data: x\nevent: resync\n\n')).toEqual([{ id: null, event: 'resync', data: '' }]);
	});
});

describe('markdown subset', () => {
	it('never renders HTML', () => {
		const h = renderMd('<img src=x onerror=alert(1)> <b>x</b>', O);
		expect(h).not.toContain('<img');
		expect(h).not.toContain('<b>');
		expect(h).toContain('&lt;img');
	});
	it('paragraphs, lists, bold, code, tables', () => {
		expect(renderMd('一\n二\n\n三', O)).toBe('<p>一<br>二</p><p>三</p>');
		expect(renderMd('- a\n- **b**', O)).toBe('<ul><li>a</li><li><strong>b</strong></li></ul>');
		expect(renderMd('1. a\n2. b', O)).toBe('<ol><li>a</li><li>b</li></ol>');
		expect(renderMd('用 `**x**` 吧', O)).toBe('<p>用 <code>**x**</code> 吧</p>');
		expect(renderMd('| a | b |\n|---|---|\n| 1 | 2 |', O)).toBe('<table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table>');
	});
	it('issue keys become links, not inside code', () => {
		expect(renderMd('看 AUTH-12', O)).toBe('<p>看 <a href="/issue/AUTH-12">AUTH-12</a></p>');
		expect(renderMd('`AUTH-12`', O)).toBe('<p><code>AUTH-12</code></p>');
	});
	it('external links carry their host and a confirmation marker; internal ones stay in the app', () => {
		const h = renderMd('[說明文件](https://docs.example.com/a?b=1)', O);
		expect(h).toContain('data-ext="docs.example.com"');
		expect(h).toContain('rel="noopener noreferrer"');
		expect(h).toContain('>說明文件</a>〔docs.example.com〕');
		// The address is never in href: a middle click must not skip the confirmation.
		expect(h).toContain('href="#"');
		expect(h).toContain('data-href="https://docs.example.com/a?b=1"');
		expect(h).not.toContain(' href="https://');
		// A key in a link's label is not a second link inside it.
		expect(renderMd('[AUTH-1 文件](https://x.example/)', O)).toBe('<p><a href="#" data-ext="x.example" data-href="https://x.example/" rel="noopener noreferrer">AUTH-1 文件</a>〔x.example〕</p>');
		expect(renderMd('[單](http://localhost:5174/issue/AUTH-1)', O)).toBe('<p><a href="/issue/AUTH-1">單</a></p>');
	});
	it('drops images, javascript: and over-long links to plain text (V23)', () => {
		expect(renderMd('![圖](https://x.example/p.png)', O)).toBe('<p>〔圖片：圖〕</p>');
		expect(renderMd('[x](javascript:alert(1))', O)).not.toContain('<a');
		const long = 'https://evil.example/?d=' + 'a'.repeat(120);
		expect(renderMd(`[x](${long})`, O)).not.toContain('<a');
		expect(linkTarget('https://e.example/' + 'p'.repeat(200), O)).toBeNull();
		expect(linkTarget('https://e.example/?' + 'q'.repeat(100), O)).not.toBeNull();
		expect(linkTarget('https://e.example/?' + 'q'.repeat(101), O)).toBeNull();
	});
});

describe('markdown escapes every path', () => {
	const bad = `<i x="1">&'`;
	const safe = '&lt;i x=&quot;1&quot;&gt;&amp;&#39;';
	it('code spans, code blocks, table cells, link labels', () => {
		expect(renderMd('`' + bad + '`', O)).toBe(`<p><code>${safe}</code></p>`);
		expect(renderMd('```\n' + bad + '\n```', O)).toBe(`<pre><code>${safe}</code></pre>`);
		expect(renderMd(`| ${bad} |\n|---|\n| ${bad} |`, O)).toBe(`<table><thead><tr><th>${safe}</th></tr></thead><tbody><tr><td>${safe}</td></tr></tbody></table>`);
		expect(renderMd(`[${bad}](https://x.example/)`, O)).toContain(`>${safe}</a>`);
		expect(renderMd(`- ${bad}`, O)).toBe(`<ul><li>${safe}</li></ul>`);
	});
	it('link length limits at the boundary', () => {
		const at = (n: number) => 'https://e.example/' + 'p'.repeat(n - 'https://e.example/'.length);
		expect(linkTarget(at(200), O)).not.toBeNull();
		expect(linkTarget(at(201), O)).toBeNull();
	});
	it('issue keys up to ten letters', () => {
		expect(renderMd('ABCDEFGHIJ-1', O)).toBe('<p><a href="/issue/ABCDEFGHIJ-1">ABCDEFGHIJ-1</a></p>');
	});
});

describe('page context and texts', () => {
	it('only matches from the start of the path; a bad escape does not throw', () => {
		expect(pageRef('/x/issue/AUTH-1')).toBeNull();
		expect(pageRef('/issue/AUTH-%E0')).toEqual({ kind: 'issue', key: 'AUTH-%E0' });
	});
	it('every unfinished reason has its tail; write cards fold keys and show the backend kinds', () => {
		expect(stopTail('deadline')).toBe('（回覆未完成：時間上限）');
		expect(stopTail('budget_exhausted')).toBe('（回覆未完成：額度上限）');
		expect(stopTail('max_tokens')).toBe('（回覆未完成：長度上限）');
		expect(stopTail('canceled')).toBe('（已停止）');
		expect(writeLine({ type: 'write', outcome: 'review_required', effects: [{ key: 'AUTH-2', kind: 'fields_changed' }, { key: 'AUTH-2', kind: 'note_added' }] })).toBe('已送審：AUTH-2（需要你核准）');
		expect(writeLine({ type: 'write', outcome: 'applied', effects: [{ key: 'AUTH-1', kind: 'changed' }, { key: 'AUTH-9', kind: 'created' }] })).toBe('已在 AUTH-1 修改；已在 AUTH-9 建立議題');
	});
	it('sends references, never text', () => {
		expect(pageRef('/issue/auth-3')).toEqual({ kind: 'issue', key: 'AUTH-3' });
		expect(pageRef('/project/web/board')).toEqual({ kind: 'project', key: 'WEB' });
		expect(pageRef('/views/v%3A1')).toEqual({ kind: 'view', id: 'v:1' });
		expect(pageRef('/time')).toBeNull();
	});
	it('write cards and unfinished replies', () => {
		expect(writeLine({ type: 'write', outcome: 'applied', effects: [{ key: 'AUTH-1', kind: 'note_added' }] })).toBe('已在 AUTH-1 新增留言');
		expect(writeLine({ type: 'write', outcome: 'review_required', reason: 'policy', effects: [{ key: 'AUTH-2', kind: 'fields_changed' }] })).toBe('已送審：AUTH-2（需要你核准）');
		expect(stopTail('tool_limit')).toBe('（回覆未完成：步驟上限）');
		expect(stopTail('interrupted')).toBe('（回覆中斷，請再送一次）');
		expect(stopTail('end_turn')).toBeNull();
	});
	it('quota texts per scope', () => {
		expect(quotaText('ai_user_hour', '2026-11-13T06:30:00Z')).toBe('這小時的使用次數已用完，14:30 後可再用。');
		expect(quotaText('ai_user_day', undefined)).toBe('今天的 AI 額度已用完。');
		expect(quotaText('org_month', undefined)).toBe('本月 AI 額度已用完，請洽管理員。');
	});
});

describe('conversation list', () => {
	const c = (id: string) => ({ id });
	it('appends pages without repeating a conversation', () => {
		expect(appendPage([c('a'), c('b')], [c('b'), c('c'), c('c')])).toEqual([c('a'), c('b'), c('c')]);
		expect(appendPage([], [c('x')])).toEqual([c('x')]);
	});
	it('puts an older opened conversation into the dropdown once', () => {
		expect(withConv([c('a'), c('b')], c('z'))).toEqual([c('z'), c('a'), c('b')]);
		expect(withConv([c('a'), c('b')], c('b'))).toEqual([c('a'), c('b')]);
	});
});

describe('expiry banner', () => {
	it('skips delegations that already expired, soonest first', () => {
		const now = Date.parse('2026-11-13T00:00:00Z');
		const ps = [
			{ project: 'OPS', expires_at: '2026-11-16T00:00:00Z' },
			{ project: 'AUTH', expires_at: '2026-11-12T00:00:00Z' },
			{ project: 'WEB', expires_at: '2026-11-15T00:00:00+08:00' }
		];
		expect(expiringProjects(ps, now)).toEqual(['WEB', 'OPS']);
		expect(expiringProjects([{ project: 'AUTH', expires_at: '2026-11-13T00:00:00Z' }], now)).toEqual([]);
	});
});

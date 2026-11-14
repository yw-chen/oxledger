import { describe, expect, it } from 'vitest';
import { ago, badge, bellLabel, budgetLine, eventLine, moreCount, reasonLabels } from './notify';

const user = { id: 'u', kind: 'user', name: 'Carol Hsu' };
const agent = { id: 'a', kind: 'agent', name: 'claude-code@bob', on_behalf_of: { id: 'b', name: 'Bob Lin' } };
const ev = (kind: string, summary: unknown, actor = user) => ({ id: 'e', at: '2026-11-09T03:12:00Z', kind, actor, summary });

describe('notification lines (§6.15)', () => {
	it('quotes a public note and marks agents', () => {
		expect(eventLine(ev('note_added', { note_excerpt: '已在 Safari 17 重現…', private: false }))).toBe('Carol Hsu 留言：已在 Safari 17 重現…');
		expect(eventLine(ev('status_changed', { old: '新建', new: '處理中' }, agent))).toBe('claude-code@bob〔agent〕把狀態從 新建 改成 處理中');
	});
	it('says private, retracted and redacted notes without their text', () => {
		expect(eventLine(ev('note_added', { note_excerpt: 'x', private: true }))).toBe('Carol Hsu 留了私有 note：x');
		expect(eventLine(ev('note_added', { retracted: true }))).toContain('已撤回');
		expect(eventLine({ ...ev('fields_changed', null), redacted: true })).toContain('已遮蔽');
	});
	it('names changed fields; quotes a single short value', () => {
		expect(eventLine(ev('fields_changed', { changes: { estimated_hours: { old: null, new: '3' } } }))).toBe('Carol Hsu 把預估工時從 （空） 改成 3');
		expect(eventLine(ev('fields_changed', { changes: { assigned_to_id: { old: null, new: '01a1d20c-2c83-7e52-8280-d53b6c81492b' } } }))).toBe('Carol Hsu 修改了指派');
		expect(eventLine(ev('fields_changed', { changes: { due_date: {}, 'cf:3': {} } }))).toBe('Carol Hsu 修改了到期日、自訂欄位');
	});
	it('counts the further updates', () => {
		const n = { events: [ev('created', {}), ev('created', {})], more_events: 1 } as never;
		expect(moreCount(n)).toBe(2);
	});
	it('keeps only the five reason labels', () => {
		expect(reasonLabels(['assigned', 'watching', 'ai_budget'])).toEqual(['指派給你', '關注中']);
		expect(reasonLabels(['mentioned'])).toEqual(['提及你']);
	});
});

describe('bell badge', () => {
	it('hides 0 and unknown counts, caps at 99+', () => {
		expect(badge({ count: 0, capped: false, stale: false })).toBe('');
		expect(badge({ count: null, capped: false, stale: true })).toBe('');
		expect(badge({ count: 7, capped: false, stale: true })).toBe('7');
		expect(badge({ count: 99, capped: true, stale: false })).toBe('99+');
		expect(bellLabel({ count: 7, capped: false, stale: false })).toBe('通知，7 則未讀');
		expect(bellLabel(null)).toBe('通知');
	});
	it('words the AI budget row', () => {
		expect(budgetLine({ ai_budget: { month: '2026-11', threshold: 80, spent_usd: '80.12', budget_usd: '100.00' } } as never)).toBe('AI 本月用量已達 80%（$80.12／$100.00）');
	});
	it('relative times', () => {
		const now = Date.parse('2026-11-13T12:00:00Z');
		expect(ago('2026-11-13T11:59:30Z', now)).toBe('剛剛');
		expect(ago('2026-11-13T11:57:00Z', now)).toBe('3 分鐘前');
		expect(ago('2026-11-13T09:00:00Z', now)).toBe('3 小時前');
	});
});

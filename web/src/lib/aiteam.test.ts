import { describe, expect, it } from 'vitest';
import { anyAllowed, byMode, changesFrom, commandLabel, fullIssueKey, isStricter, ROLE_KEY, setAll, sharedRuntimeText, WIZARD_TEMPLATES } from './aiteam';

const tpl = { commands: { add_note: 'direct', update_fields: 'review', create_issue: 'off' }, fields: { subject: 'direct', due_date: 'off' } } as const;

describe('AI 團隊', () => {
	it('全部需要核准 keeps what is not allowed out', () => {
		const p = setAll({ commands: { ...tpl.commands }, fields: { ...tpl.fields } }, 'review');
		expect(p.commands).toEqual({ add_note: 'review', update_fields: 'review', create_issue: 'off' });
		expect(p.fields).toEqual({ subject: 'review', due_date: 'off' });
		expect(setAll({ commands: { ...tpl.commands }, fields: {} }, 'review', true).commands.create_issue).toBe('review');
	});
	it('sends only the changes from the template', () => {
		const p = { commands: { ...tpl.commands, add_note: 'review' as const }, fields: { ...tpl.fields, due_date: 'direct' as const } };
		expect(changesFrom({ commands: { ...tpl.commands }, fields: { ...tpl.fields } }, p)).toEqual({ commands: { add_note: 'review' }, fields: { due_date: 'direct' } });
	});
	it('groups commands by mode and tells when nothing may write', () => {
		expect(byMode({ commands: { ...tpl.commands }, fields: {} })).toEqual({ direct: ['add_note'], review: ['update_fields'], off: ['create_issue'] });
		expect(anyAllowed({ commands: { a: 'off' }, fields: {} })).toBe(false);
		expect(anyAllowed({ commands: { a: 'off', b: 'review' }, fields: {} })).toBe(true);
	});
	it('a project manager may only make it stricter', () => {
		expect(isStricter('direct', 'review')).toBe(true);
		expect(isStricter('review', 'off')).toBe(true);
		expect(isStricter('review', 'review')).toBe(true);
		expect(isStricter('review', 'direct')).toBe(false);
		expect(isStricter('off', 'review')).toBe(false);
	});
	it('the shared-runtime warning names roles and sources, and time readers when there are some', () => {
		const t = sharedRuntimeText({ roles: ['claude-code@alice'], sources: ['WEB', 'AUTH'], time_readers: [] });
		expect(t).toBe('這個角色會和 claude-code@alice 共用來源限制：它只能寫到讀者不多於 WEB、AUTH 的地方；任一專案權限改變時這些角色會一起停。');
		expect(sharedRuntimeText({ roles: ['a@x'], sources: [], time_readers: ['a@x'] })).toContain('a@x 可讀你的工時');
	});
	it('labels commands and checks role keys like the server', () => {
		expect(commandLabel('triage_decide')).toBe('Triage 決定');
		expect(commandLabel('add_note')).toBe('新增 note');
		expect(ROLE_KEY.test('triage')).toBe(true);
		expect(ROLE_KEY.test('qa_2')).toBe(true);
		expect(ROLE_KEY.test('QA')).toBe(false);
		expect(ROLE_KEY.test('qa-2')).toBe(false);
		expect(ROLE_KEY.test('a' + 'b'.repeat(29))).toBe(true);
		expect(ROLE_KEY.test('a' + 'b'.repeat(30))).toBe(false);
	});
});

describe('edge cases', () => {
	it('an unknown mode counts as off', () => {
		expect(isStricter('direct', 'weird' as never)).toBe(true);
		expect(isStricter('weird' as never, 'review')).toBe(false);
		expect(isStricter('weird' as never, 'off')).toBe(true);
	});
	it('the wizard never offers the personal assistant template', () => {
		expect(WIZARD_TEMPLATES).not.toContain('personal');
		expect(WIZARD_TEMPLATES).toContain('custom');
		// Design 06 §4.4: 「文件維護」 holds the wiki ops.
		expect(WIZARD_TEMPLATES).toContain('docs');
		expect(commandLabel('wiki_put')).toBe('寫 wiki 頁');
		expect(commandLabel('wiki_move')).toBe('改名或移動 wiki 頁');
	});
	it('only a full issue key goes to the server', () => {
		expect(fullIssueKey(' auth-12 ')).toBe('AUTH-12');
		expect(fullIssueKey('AUTH-')).toBe('');
		expect(fullIssueKey('AUTH-ddd')).toBe('');
		expect(fullIssueKey('AUTH')).toBe('');
		expect(fullIssueKey('1AUTH-2')).toBe('');
	});
});

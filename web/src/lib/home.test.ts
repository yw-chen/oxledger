import { describe, expect, it } from 'vitest';
import { homeProblem, moveTo, newWidget, toInput, widgetTitle } from './home';
import { resolvedTheme } from './prefs.svelte';
import { shortcutHref } from './me.svelte';

const decide = { id: 'w_decide', type: 'need_decision', width: 2, available: true, protected: true };

describe('home widgets (§3.8)', () => {
	it('sends only the properties each type has', () => {
		expect(toInput(decide)).toEqual({ id: 'w_decide', type: 'need_decision', width: 2 });
		expect(toInput({ ...decide, id: 'v', type: 'view', view_id: 'builtin:assigned_to_me', view_name: '指派給我', protected: false })).toEqual({ id: 'v', type: 'view', width: 2, view_id: 'builtin:assigned_to_me', limit: 8 });
		expect(toInput({ ...decide, id: 'm', type: 'milestones', project_id: 'p1', project_key: 'AUTH' })).toMatchObject({ project_id: 'p1' });
		expect(toInput({ ...decide, id: 'm', type: 'milestones' })).not.toHaveProperty('project_id');
	});
	it('needs exactly one 需要你決定 and at most 12', () => {
		expect(homeProblem([decide])).toBe('');
		expect(homeProblem([])).toContain('需要你決定');
		expect(homeProblem([decide, { ...decide, id: 'x' }])).toContain('需要你決定');
		expect(homeProblem([decide, ...Array.from({ length: 12 }, () => newWidget('agent_summary'))])).toContain('12');
		expect(homeProblem([decide, newWidget('view')])).toContain('選一個視圖');
		expect(homeProblem([decide, { ...newWidget('view', { view_id: 'v' }), limit: 21 }])).toContain('20');
	});
	it('moves', () => {
		expect(moveTo(['a', 'b', 'c'], 2, 0)).toEqual(['c', 'a', 'b']);
		expect(moveTo(['a', 'b', 'c'], 0, 5)).toEqual(['a', 'b', 'c']);
	});
	it('titles', () => {
		expect(widgetTitle({ ...decide, type: 'view', view_name: '與我相關' })).toBe('與我相關');
		expect(widgetTitle({ ...decide, type: 'milestones', project_key: 'AUTH' })).toBe('里程碑・AUTH');
	});
});

describe('theme and shortcuts', () => {
	it('跟隨系統 follows the OS; 高對比 is its own palette', () => {
		expect(resolvedTheme('system', true)).toBe('light');
		expect(resolvedTheme('system', false)).toBe('dark');
		expect(resolvedTheme('high_contrast', true)).toBe('hc');
	});
	it('shortcut targets', () => {
		expect(shortcutHref({ kind: 'view', id: 'builtin:assigned_to_me' }, null)).toBe('/views/builtin%3Aassigned_to_me');
		expect(shortcutHref({ kind: 'board' }, 'AUTH')).toBe('/project/AUTH/board');
		expect(shortcutHref({ kind: 'board' }, null)).toBeNull();
		expect(shortcutHref({ kind: 'timesheet' }, null)).toBe('/time');
	});
});

import { personalTab } from './home';
describe('personal default view (§3.8 default_views)', () => {
	const tabs = [{ id: 'timeline' }, { id: 'issues' }, { id: 'board' }, { id: 'x', view_id: 'v1' }, { id: 'h', view_id: 'v2', hidden: true }];
	it('the project entry wins over *', () => {
		expect(personalTab({ '*': { view_id: 'builtin:board' } }, 'p', tabs)).toBe('board');
		expect(personalTab({ '*': { view_id: 'builtin:board' }, p: { view_id: 'builtin:list' } }, 'p', tabs)).toBe('issues');
		expect(personalTab({ p: { view_id: 'v1' } }, 'p', tabs)).toBe('x');
	});
	it('falls back when nothing shows it', () => {
		expect(personalTab({ p: { view_id: 'v2' } }, 'p', tabs)).toBeNull();
		expect(personalTab({ p: { view_id: 'v1', available: false } }, 'p', tabs)).toBeNull();
		expect(personalTab({}, 'p', tabs)).toBeNull();
	});
});

import { myDefaults } from './home';
describe('which default views count', () => {
	it('ignores * until an interface role is chosen; per-project entries always count', () => {
		const dv = { '*': { view_id: 'builtin:list' }, p: { view_id: 'builtin:board' } };
		expect(myDefaults({ default_views: dv, interface_roles: [] })).toEqual({ p: { view_id: 'builtin:board' } });
		expect(myDefaults({ default_views: dv, interface_roles: ['pm'] })).toEqual(dv);
		expect(myDefaults(null)).toEqual({});
	});
});

import { describe, expect, it } from 'vitest';
import { describeEvents, type AuditEvent } from './aievents';

const names: Record<string, string> = { p1: '本機假模型', p2: 'Claude' };
const nm = (id: string) => names[id] ?? '';
let n = 0;
const ev = (kind: string, payload: unknown, provider_id: string | null = null): AuditEvent => ({ id: `e${++n}`, kind, payload, provider_id });
const settings = (o: Record<string, unknown>) => ({ enabled: false, active_provider_id: null, monthly_budget_usd: null, user_daily_budget_usd: '2', user_runs_per_hour: 20, conversation_retention_days: 90, ...o });

describe('describeEvents', () => {
	it('shows what changed in the settings against the previous settings', () => {
		const list = [ev('settings_updated', settings({ enabled: true, active_provider_id: 'p1', monthly_budget_usd: '80.00' }), 'p1'), ev('settings_updated', settings({ active_provider_id: 'p1', monthly_budget_usd: '50' }), 'p1')];
		expect(describeEvents(list, nm)[0]).toBe('啟用・月預算 50 → 80');
	});
	it('names the provider switched to', () => {
		const list = [ev('settings_updated', settings({ active_provider_id: 'p2' }), 'p2'), ev('settings_updated', settings({ active_provider_id: 'p1' }), 'p1')];
		expect(describeEvents(list, nm)[0]).toBe('使用中：本機假模型 → Claude');
	});
	it('lists the values when there is nothing older', () => {
		expect(describeEvents([ev('settings_updated', settings({ monthly_budget_usd: '50' }))], nm)[0]).toBe('停用・使用中：無・月預算 50・每人每日預算 2・每人每小時次數 20・對話保存天數 90');
	});
	it('says nothing changed when saving the same values', () => {
		const s = settings({ monthly_budget_usd: '50' });
		expect(describeEvents([ev('settings_updated', s), ev('settings_updated', s)], nm)[0]).toBe('沒有變更');
	});
	it('explains the switch-off after a provider change, and skips it as a baseline', () => {
		const list = [ev('settings_updated', settings({ monthly_budget_usd: '60' })), ev('settings_updated', { enabled: false, reason: 'provider_config_changed' }, 'p1'), ev('settings_updated', settings({ monthly_budget_usd: '50' }))];
		const out = describeEvents(list, nm);
		expect(out[0]).toBe('月預算 50 → 60');
		expect(out[1]).toBe('停用（使用中的服務設定改了，要重新測試）');
	});
	it('turning the AI back on after the system turned it off is a change', () => {
		const on = settings({ enabled: true, active_provider_id: 'p1' });
		const list = [ev('settings_updated', on), ev('settings_updated', { enabled: false, reason: 'provider_config_changed' }, 'p1'), ev('settings_updated', on)];
		expect(describeEvents(list, nm)[0]).toBe('啟用');
	});
	it('retiring the active provider turned the AI off and cleared it', () => {
		const list = [
			ev('settings_updated', settings({ enabled: true, active_provider_id: 'p2' })),
			ev('activated', { previous: null }, 'p2'),
			ev('retired', { was_active: true }, 'p1'),
			ev('settings_updated', settings({ enabled: true, active_provider_id: 'p1' }))
		];
		expect(describeEvents(list, nm)[0]).toBe('啟用・使用中：無 → Claude');
	});
	it('a provider saved with the same values says so', () => {
		const p = { name: 'Fake', endpoint_name: 'local', model: 'x', max_output_tokens: 1, input_usd_per_mtok: '0', output_usd_per_mtok: '0' };
		expect(describeEvents([ev('updated', p, 'p1'), ev('created', p, 'p1')], nm)[0]).toBe('沒有變更');
	});
	it('activated: the provider and the one before', () => {
		expect(describeEvents([ev('activated', { previous: 'p2' }, 'p1')], nm)[0]).toBe('本機假模型（原本：Claude）');
		expect(describeEvents([ev('activated', { previous: null }, 'p1')], nm)[0]).toBe('本機假模型');
		expect(describeEvents([ev('activated', { previous: 'gone' }, 'p1')], nm)[0]).toBe('本機假模型（原本：（已刪除的服務））');
	});
	it('provider updates compare with the same provider only', () => {
		const list = [
			ev('updated', { name: 'Claude', endpoint_name: 'anthropic', model: 'b', max_output_tokens: 4096, input_usd_per_mtok: '3', output_usd_per_mtok: '15', config_changed: true }, 'p2'),
			ev('created', { name: 'Fake', endpoint_name: 'local', model: 'x', max_output_tokens: 1, input_usd_per_mtok: '0', output_usd_per_mtok: '0' }, 'p1'),
			ev('created', { name: 'Claude', kind: 'anthropic', endpoint_name: 'anthropic', model: 'a', max_output_tokens: 4096, input_usd_per_mtok: '3.00', output_usd_per_mtok: '15.00' }, 'p2')
		];
		const out = describeEvents(list, nm);
		expect(out[0]).toBe('模型 a → b');
		expect(out[2]).toBe('名稱 Claude・端點 anthropic・模型 a・最大輸出 token 4096・輸入單價 3・輸出單價 15');
	});
	it('test runs and retirement', () => {
		const out = describeEvents(
			[ev('test_run', { ok: true, supports_tools: true }), ev('test_run', { ok: false, kind: 'http_status', status: 401, supports_tools: false }), ev('test_run', { ok: false, kind: 'timeout', supports_tools: false }), ev('retired', { was_active: true }), ev('retired', { was_active: false })],
			nm
		);
		expect(out).toEqual(['成功', '失敗：HTTP 401', '失敗：逾時', '原本是使用中的服務，AI 已停用', '']);
	});
	it('survives an odd payload', () => {
		expect(describeEvents([ev('settings_updated', null), ev('mystery', 'x')], nm)).toEqual(['沒有變更', '']);
	});
});

// SPDX-License-Identifier: GPL-2.0-or-later
// AI 設定的稽核 (contract 05 §6.20 ⑦「誰、何時、改了什麼」). The server
// stores each change's new values only (crates/oxledger/src/ai/admin.rs):
//   created / updated → the provider's fields, retired → {was_active},
//   activated → {previous}, settings_updated → the whole settings (or
//   {enabled:false, reason} when a provider change turned the AI off),
//   test_run → {ok, kind?, status?, supports_tools, at}.
// "What changed" compares with the next older event of the same thing in the
// list (newest first); without one the new values are listed.

export type AuditEvent = { id: string; kind: string; payload: unknown; provider_id: string | null };

const SETTINGS: [string, string][] = [
	['monthly_budget_usd', '月預算'],
	['user_daily_budget_usd', '每人每日預算'],
	['user_runs_per_hour', '每人每小時次數'],
	['conversation_retention_days', '對話保存天數']
];
const PROVIDER: [string, string][] = [
	['name', '名稱'],
	['endpoint_name', '端點'],
	['model', '模型'],
	['max_output_tokens', '最大輸出 token'],
	['input_usd_per_mtok', '輸入單價'],
	['output_usd_per_mtok', '輸出單價']
];
const TEST_KIND: Record<string, string> = { timeout: '逾時', http_status: 'HTTP 錯誤', malformed: '回應格式不對', endpoint_blocked: '端點位址不允許', not_configured: '這個版本沒有 AI 連線功能', connect: '連不上' };

const obj = (p: unknown): Record<string, unknown> => (p && typeof p === 'object' && !Array.isArray(p) ? (p as Record<string, unknown>) : {});
/** "50.00" → "50", "0.5" → "0.5"; null → "未設定". */
function val(v: unknown): string {
	if (v === null || v === undefined || v === '') return '未設定';
	if (typeof v === 'number' || (typeof v === 'string' && /^-?\d+(\.\d+)?$/.test(v))) return String(Number(v));
	return String(v);
}

/** Field-by-field change against the previous values ("月預算 50 → 80"); all of `now` when there is no previous. */
function diff(fields: [string, string][], now: Record<string, unknown>, before: Record<string, unknown> | null): string[] {
	const out: string[] = [];
	for (const [k, label] of fields) {
		if (!(k in now)) continue;
		if (!before || !(k in before)) out.push(`${label} ${val(now[k])}`);
		else if (val(before[k]) !== val(now[k])) out.push(`${label} ${val(before[k])} → ${val(now[k])}`);
	}
	return out;
}

/** The settings as they stood before event `i` (newest first), replayed from the
 *  oldest one listed: the AI is also turned off without a full settings event —
 *  `{enabled:false, reason}` when the active provider's config changed, and
 *  retiring the active provider clears it and turns the AI off (admin.rs). */
function settingsBefore(events: readonly AuditEvent[], i: number): Record<string, unknown> | null {
	let s: Record<string, unknown> | null = null;
	for (let j = events.length - 1; j > i; j--) {
		const e = events[j];
		const p = obj(e.payload);
		if (e.kind === 'settings_updated') s = p.reason === undefined ? { ...p } : { ...(s ?? {}), enabled: false };
		else if (e.kind === 'retired' && p.was_active) s = { ...(s ?? {}), enabled: false, active_provider_id: null };
	}
	return s;
}

/** One readable line per event, in the order given (newest first). */
export function describeEvents(events: readonly AuditEvent[], providerName: (id: string) => string): string[] {
	const name = (id: unknown) => (typeof id === 'string' && id ? providerName(id) || '（已刪除的服務）' : '無');
	const older = (i: number, ok: (e: AuditEvent) => boolean) => events.slice(i + 1).find(ok) ?? null;
	return events.map((ev, i) => {
		const p = obj(ev.payload);
		switch (ev.kind) {
			case 'created':
				return diff(PROVIDER, p, null).join('・');
			case 'updated': {
				const prev = older(i, (e) => e.provider_id === ev.provider_id && (e.kind === 'created' || e.kind === 'updated'));
				const parts = diff(PROVIDER, p, prev ? obj(prev.payload) : null);
				if (prev && !parts.length) return '沒有變更';
				return parts.join('・');
			}
			case 'retired':
				return p.was_active ? '原本是使用中的服務，AI 已停用' : '';
			case 'activated':
				return `${name(ev.provider_id)}${p.previous ? `（原本：${name(p.previous)}）` : ''}`;
			case 'test_run':
				if (p.ok) return `成功${p.supports_tools === false ? '（不支援工具呼叫）' : ''}`;
				return `失敗：${p.kind === 'http_status' ? `HTTP ${p.status ?? ''}` : (TEST_KIND[String(p.kind ?? '')] ?? String(p.kind ?? '原因不明'))}`;
			case 'settings_updated': {
				if (p.reason === 'provider_config_changed') return '停用（使用中的服務設定改了，要重新測試）';
				const b = settingsBefore(events, i);
				const parts: string[] = [];
				if ('enabled' in p && (!b || !!b.enabled !== !!p.enabled)) parts.push(p.enabled ? '啟用' : '停用');
				if ('active_provider_id' in p && (!b || (b.active_provider_id ?? null) !== (p.active_provider_id ?? null)))
					parts.push(b ? `使用中：${name(b.active_provider_id)} → ${name(p.active_provider_id)}` : `使用中：${name(p.active_provider_id)}`);
				parts.push(...diff(SETTINGS, p, b));
				return parts.length ? parts.join('・') : '沒有變更';
			}
			default:
				return '';
		}
	});
}

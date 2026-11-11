// SPDX-License-Identifier: GPL-2.0-or-later
// Milestone health (§17.12): computed by the server with dates only, never AI.

export const HEALTH: Record<string, { label: string; cls: string }> = {
	on_track: { label: '正常', cls: 'c-ok' },
	at_risk: { label: '有風險', cls: 'c-warn' },
	overdue: { label: '已逾期', cls: 'c-bad' },
	no_date: { label: '未定日期', cls: 'c-dim' },
	closed: { label: '已關閉', cls: 'c-dim' }
};

export const HEALTH_REASON: Record<string, string> = {
	version_overdue: '里程碑日期已過且還有未完成的單',
	issue_overdue: '有單已逾期',
	issue_late_for_version: '有單的到期日晚於里程碑',
	deadline_near_incomplete: '接近日期且未完成的比例高'
};

export const FLAG: Record<string, { label: string; cls: string }> = {
	overdue: { label: '逾期', cls: 'c-bad' },
	late_for_version: { label: '晚於里程碑', cls: 'c-warn' },
	in_closed_version: { label: '里程碑已關閉', cls: 'c-dim' },
	no_dates: { label: '沒有日期', cls: 'c-dim' }
};

export const VERSION_STATUS: Record<string, string> = { open: '進行中', locked: '鎖定', closed: '關閉' };

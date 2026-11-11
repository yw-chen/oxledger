// SPDX-License-Identifier: GPL-2.0-or-later
// Display helpers (Taiwan conventions, org time zone Asia/Taipei for dates).

const TZ = 'Asia/Taipei';

export function fmtTime(iso: string | null | undefined): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	return d.toLocaleString('zh-TW', { timeZone: TZ, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false });
}

export function fmtShort(iso: string | null | undefined): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	return d.toLocaleString('zh-TW', { timeZone: TZ, month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false });
}

/** A field value as the queue and activity show it (§15.3: names, not ids). */
export function fmtValue(v: unknown): string {
	if (v === null || v === undefined || v === '') return '（空）';
	if (typeof v === 'string') return v;
	if (typeof v === 'boolean') return v ? '是' : '否';
	if (typeof v === 'number') return String(v);
	if (Array.isArray(v)) return v.map(fmtValue).join('、');
	const o = v as Record<string, unknown>;
	for (const k of ['name', 'label', 'title', 'key', 'value']) if (typeof o[k] === 'string') return o[k] as string;
	return JSON.stringify(v);
}

export const FIELD_LABELS: Record<string, string> = {
	subject: '標題',
	description: '描述',
	assigned_to_id: '指派',
	priority_id: '優先度',
	fixed_version_id: '里程碑',
	start_date: '開始日',
	due_date: '到期日',
	status: '狀態',
	status_id: '狀態',
	is_private: '私有',
	note: 'note',
	name: '名稱',
	effective_date: '日期',
	description_version: '說明'
};

export function fieldLabel(k: string): string {
	return FIELD_LABELS[k] ?? k;
}

export const COMMAND_LABELS: Record<string, string> = {
	create_issue: '建立 issue',
	update_fields: '修改欄位',
	transition: '轉狀態',
	add_note: '新增 note',
	duplicate_of: '標為重複',
	create_version: '建立里程碑',
	update_version: '修改里程碑'
};

export const MODE_LABELS: Record<string, string> = { direct: '直接寫入', review: '需要核准', off: '不允許' };

export const ISSUE_KEY = /^[A-Za-z][A-Za-z0-9]{0,9}-\d+$/;

/** Only http(s) links leave the app (a PR URL comes from outside data). */
export function safeHref(url: string | null | undefined): string | undefined {
	if (!url) return undefined;
	try {
		const u = new URL(url);
		return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : undefined;
	} catch {
		return undefined;
	}
}

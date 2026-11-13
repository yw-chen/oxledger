// SPDX-License-Identifier: GPL-2.0-or-later
// Custom field administration (contract 05 §3.7, §6.11): labels and the
// conversion pairs the server supports (field_admin.rs `rule_for`).

import type { FieldAdminView } from '$api/types';

export const FTYPES: [string, string][] = [
	['string', '單行文字'],
	['text', '多行文字'],
	['int', '整數'],
	['float', '小數'],
	['date', '日期'],
	['bool', '是／否'],
	['list', '清單'],
	['user', '人員'],
	['link', '連結']
];
export const ftypeLabel = (t: string) => FTYPES.find(([k]) => k === t)?.[1] ?? t;

/** State shown in the list (§6.11): 正常、停用、索引建立中、刪除中. */
export function fieldState(f: FieldAdminView): { label: string; cls: string } {
	if (f.index_state === 'dropping') return { label: '刪除中', cls: 'c-bad' };
	if (f.deactivated) return { label: '停用', cls: 'c-dim' };
	if (f.index_state === 'pending') return { label: '索引建立中', cls: 'c-warn' };
	return { label: '正常', cls: 'c-ok' };
}

const TEXTUAL = ['string', 'text', 'link'];
/** Target types a field of type `from` can convert to. */
export function convertTargets(from: string): string[] {
	const out: string[] = [];
	for (const [to] of FTYPES) {
		if (to === from) {
			if (from === 'list') out.push(to);
			continue;
		}
		if (from === 'list' && to === 'list') out.push(to);
		else if (TEXTUAL.includes(from) && to === 'list') out.push(to);
		else if (TEXTUAL.includes(from) && ['int', 'float', 'date', 'bool'].includes(to)) out.push(to);
		else if (from === 'int' && to === 'float') out.push(to);
		else if (['int', 'float', 'date', 'list', 'string', 'link'].includes(from) && TEXTUAL.includes(to)) out.push(to);
		else if (from === 'text' && (to === 'string' || to === 'link')) out.push(to);
	}
	return out;
}

/** What kind of mapping the pair needs. */
export function mappingKind(from: string, to: string): 'options' | 'values' | 'none' {
	if (from === 'list' && to === 'list') return 'options';
	if (TEXTUAL.includes(from) && to === 'list') return 'values';
	return 'none';
}

/** How the server parses text into `to` (shown in step ②). */
export const PARSE_HINT: Record<string, string> = {
	int: '去掉前後空白後要是整數（例如 12、-3）；其他的值會列為失敗。',
	float: '去掉前後空白後要是數字（例如 1.5）；其他的值會列為失敗。',
	date: '要是 YYYY-MM-DD 格式的日期；其他的值會列為失敗。',
	bool: "只認得 true／1／yes 與 false／0／no（不分大小寫）；其他的值會列為失敗。"
};

export const REASON: Record<string, string> = { unmapped: '沒有對照', unparseable: '無法解析', too_long: '太長' };

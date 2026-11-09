// SPDX-License-Identifier: GPL-2.0-or-later
// ⌘K intent routing (design 01 §15.11). Every input gets a testable
// `route_kind`. The built-in AI is off (frontend Q2), so natural language
// goes to text search and says so; nothing is sent to a model.

import { ISSUE_KEY } from './format';
import { hasConditions, parse, SyntaxError_ } from './query';

export type RouteKind = 'issue_open' | 'structured_search' | 'command_draft' | 'ai_task' | 'text_search' | 'clarify';

export type Choice = { label: string; href: string | null; disabled?: string };

export type Intent = {
	kind: RouteKind;
	/** Where Enter goes (null: pick a choice). */
	href: string | null;
	/** Shown under the input. */
	note?: string;
	/** Syntax error position (code points) in the input. */
	errorAt?: number;
	choices?: Choice[];
};

const search = (q: string) => `/search?q=${encodeURIComponent(q)}`;

export function route(raw: string, aiEnabled = false): Intent | null {
	const text = raw.trim();
	if (!text) return null;
	if (raw.length > 500 || raw.includes('\n')) {
		return {
			kind: 'clarify',
			href: null,
			note: '內容很長，要怎麼用？',
			choices: [
				{ label: '搜尋這段內容', href: search(text.slice(0, 500)) },
				{ label: '用它來回報問題', href: `/new?description=${encodeURIComponent(raw.slice(0, 8000))}` },
				{ label: '交給 AI 整理', href: null, disabled: '內建 AI 未啟用' }
			]
		};
	}
	if (ISSUE_KEY.test(text)) return { kind: 'issue_open', href: `/issue/${text.toUpperCase()}` };
	if (text.startsWith('/')) {
		const [cmd, ...rest] = text.split(/\s+/);
		if (cmd === '/new') {
			const p = rest[0] && /^[A-Za-z0-9]{1,10}$/.test(rest[0]) ? `?project=${rest[0].toUpperCase()}` : '';
			return { kind: 'command_draft', href: `/new${p}`, note: '開啟回報表單（不會直接寫入）' };
		}
		return {
			kind: 'clarify',
			href: null,
			note: `第一版只支援 /new；${cmd} 不是可用的指令`,
			choices: [
				{ label: '回報問題（/new）', href: '/new' },
				{ label: '只搜尋這段文字', href: search(text) }
			]
		};
	}
	// A key mixed with other words ("AUTH-123 改成 bob") is natural language:
	// confirm the target and meaning first.
	const words = text.split(/\s+/);
	const key = words.find((w) => ISSUE_KEY.test(w));
	if (key && words.length > 1 && !words.some((w) => w.includes(':'))) {
		return {
			kind: 'clarify',
			href: null,
			note: '要對這張單做什麼？',
			choices: [
				{ label: `開啟 ${key.toUpperCase()}`, href: `/issue/${key.toUpperCase()}` },
				{ label: '只搜尋這段文字', href: search(text) },
				{ label: '交給 AI 處理', href: null, disabled: '內建 AI 未啟用' }
			]
		};
	}
	try {
		const q = parse(text);
		if (hasConditions(q)) return { kind: 'structured_search', href: search(text) };
	} catch (e) {
		if (e instanceof SyntaxError_) {
			// Never silently send a broken query anywhere else (§15.11).
			return { kind: 'structured_search', href: null, note: `語法錯誤（第 ${e.position + 1} 個字）：${e.message}`, errorAt: e.position };
		}
		throw e;
	}
	if (aiEnabled) return { kind: 'ai_task', href: null };
	return { kind: 'text_search', href: search(text), note: '內建 AI 未啟用：以文字搜尋' };
}

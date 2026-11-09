// SPDX-License-Identifier: GPL-2.0-or-later
// What a person sees for an error (contract 02 §15.5): what happened, whether
// any data changed, and the next step. Programs decide on `code` only.

import { ApiError } from './api';

export type Shown = {
	title: string;
	/** Did anything change? */
	changed: string;
	/** The suggested next step, as button text (the page wires it up). */
	next?: string;
	/** Kind of next step the page can act on. */
	action?: 'login' | 'retry' | 'refresh' | 'conflict' | 'latest' | 'delegations' | 'queue' | 'reauth' | 'wait' | 'transitions';
};

const NOTHING = '沒有改到任何資料。';
const UNKNOWN = '尚未確認是否完成。';

function names(v: unknown): string {
	if (!Array.isArray(v)) return '';
	return v
		.map((x) => (typeof x === 'string' ? x : (x as { name?: string; label?: string })?.name ?? (x as { label?: string })?.label ?? ''))
		.filter(Boolean)
		.join('、');
}

/** Project tab settings (§18.6 R-T1–R-T5): `details.kind` → what to fix. */
const INVALID_KIND: Record<string, string> = {
	tab_title_invalid: '標題不能空白，也不能太長',
	tab_title_taken: '標題和其他分頁重複',
	tab_url_invalid: '連結要是完整的 https 網址',
	tab_query_scope: '查詢只能查這個專案（條件要是 project:這個專案）',
	tab_query_syntax: '查詢條件的語法有誤',
	tab_default_invalid: '預設分頁要是沒有隱藏的時程、里程碑、議題、開發或查詢分頁',
	tab_limit: '分頁數量超過上限',
	builtin_missing: '內建分頁不能移除（可以隱藏）',
	duplicate_id: '同一個分頁出現兩次',
	unknown_id: '有分頁已經不存在，請重新載入',
	unknown_kind: '分頁種類不正確'
};
/** `tabs[5].url` → 「第 6 列：」. */
function rowOf(path: unknown): string {
	const m = typeof path === 'string' ? /^tabs\[(\d+)\]/.exec(path) : null;
	return m ? `第 ${Number(m[1]) + 1} 列：` : '';
}

export function describeError(e: unknown): Shown {
	if (!(e instanceof ApiError)) return { title: '發生未預期的錯誤', changed: UNKNOWN, next: '重新整理', action: 'refresh' };
	const d = e.details;
	switch (e.code) {
		case 'network':
			return { title: '暫時無法連線', changed: UNKNOWN, next: '重試', action: 'retry' };
		case 'temporarily_unavailable':
			return { title: '暫時無法連線，尚未確認是否完成', changed: UNKNOWN, next: '重試', action: 'retry' };
		case 'unauthenticated':
			return { title: '登入已過期，請重新登入', changed: NOTHING, next: '登入', action: 'login' };
		case 'invalid_credentials':
			return { title: '帳號或密碼不正確', changed: '忘記密碼請聯絡管理員。' };
		case 'reauthentication_required':
			return { title: '為了這個操作，請再輸入一次密碼', changed: NOTHING, next: '確認密碼', action: 'reauth' };
		case 'csrf_failed':
			return { title: '安全檢查沒有通過，請重新整理頁面', changed: NOTHING, next: '重新整理', action: 'refresh' };
		case 'conflict':
			return { title: '這筆資料在你開啟之後被別人改過，尚未套用任何變更', changed: NOTHING, next: '檢視差異', action: 'conflict' };
		case 'precondition_failed':
			return { title: '資料已經和預期不同，尚未套用任何變更', changed: NOTHING, next: '重新載入', action: 'refresh' };
		case 'version_superseded':
			return { title: 'agent 已更新這份提案，你看的是舊版本', changed: NOTHING, next: '看最新版本', action: 'latest' };
		case 'delegation_unavailable':
			return { title: '這份提案的委派已失效（到期、已撤銷或專案不再符合條件），無法套用', changed: NOTHING, next: '管理委派', action: 'delegations' };
		case 'context_unavailable':
			return {
				title:
					d.kind === 'closed'
						? '這個 agent 的工作脈絡已停用（已關閉或已建立新工作脈絡），它的提案都不能套用'
						: '這個 agent 的工作脈絡已停用（曾授予的專案權限設定變更），它的提案都不能套用',
				changed: NOTHING,
				next: '建立新工作脈絡',
				action: 'delegations'
			};
		case 'proposal_unavailable':
			return { title: '提案內容目前無法完整存取，不能套用', changed: NOTHING, next: '回待審清單', action: 'queue' };
		case 'changeset_expired':
			return { title: '提案已到期', changed: NOTHING, next: '請 agent 重新提案' };
		case 'changeset_not_executable':
			return { title: '這份提案已經處理過（已拒絕、已取消或已成功）', changed: NOTHING, next: '回待審清單', action: 'queue' };
		case 'already_applied':
			return { title: '這個版本已經套用過', changed: '先前的套用已生效。', next: '重新載入', action: 'refresh' };
		case 'approval_invalidated':
			return { title: '提案內容已變更或被遮蔽，原本的核准無效', changed: NOTHING, next: '重新載入', action: 'refresh' };
		case 'invalid_transition': {
			const allowed = names(d.allowed);
			return {
				title: allowed ? `目前不能轉到這個狀態；可以轉到：${allowed}` : '目前不能轉到這個狀態',
				changed: NOTHING,
				next: '選擇狀態',
				action: 'transitions'
			};
		}
		case 'review_outdated':
			return { title: '預覽之後資料又變了，請再確認一次', changed: NOTHING, next: '重新預覽', action: 'refresh' };
		case 'execution_in_progress':
			return { title: '結果確認中，請勿重複按；會自動更新', changed: UNKNOWN, action: 'wait' };
		case 'forbidden_op':
			return { title: '你沒有這個操作的權限', changed: NOTHING };
		case 'not_found':
			return { title: '找不到，或你沒有權限查看', changed: NOTHING };
		case 'invalid_value': {
			const why = typeof d.kind === 'string' ? INVALID_KIND[d.kind] : undefined;
			if (why) return { title: `${rowOf(d.path)}${why}`, changed: NOTHING };
			const allowed = names(d.allowed);
			return { title: allowed ? `值不合法；可以用：${allowed}` : '值不合法', changed: NOTHING };
		}
		case 'required_field_missing':
			return { title: `缺少必填欄位${d.field ? `：${String(d.field)}` : ''}`, changed: NOTHING };
		case 'readonly_field':
			return { title: '這個欄位目前不能修改', changed: NOTHING };
		case 'relation_not_allowed':
			return { title: '不能建立這個關係（不能連到自己，也不能跨專案）', changed: NOTHING };
		case 'audience_expansion_forbidden':
			return { title: '寫入目的地的讀者比來源多，已擋下', changed: NOTHING };
		case 'agent_suspended':
			return { title: '這個 agent 的寫入已暫停', changed: NOTHING, next: '管理委派', action: 'delegations' };
		case 'quota_exceeded':
			return { title: '配額已用完', changed: NOTHING };
		case 'rate_limited':
			return { title: '操作太頻繁，請稍後再試', changed: NOTHING, next: '稍後重試', action: 'retry' };
		case 'version_not_open':
			return { title: '這個里程碑已鎖定或關閉，不能指定', changed: NOTHING };
		case 'version_in_use':
			return { title: `里程碑仍有議題使用${names(d.keys) ? `：${names(d.keys)}` : ''}，不能刪除`, changed: NOTHING };
		case 'version_name_taken':
			return { title: '同一個專案已經有這個名稱的里程碑', changed: NOTHING };
		case 'undo_not_applicable':
			return { title: '這筆變更目前不能回復（之後已被修改、值已不合法、權限或資料已到期）', changed: NOTHING };
		case 'repo_not_linkable':
			return { title: '這個 repo 不能連動（不存在、未開放或 App 無法存取）', changed: NOTHING };
		case 'repo_already_linked':
			return { title: '這個 repo 已經連動了', changed: NOTHING };
		case 'repo_link_limit':
			return { title: '這個專案已連動 20 個 repo，已達上限', changed: NOTHING };
		case 'invalid_query':
		case 'invalid_filter':
			return { title: '查詢語法有誤', changed: NOTHING };
		case 'search_unavailable':
			return { title: '搜尋暫時無法使用；議題本身仍可開啟', changed: NOTHING, next: '重試', action: 'retry' };
		case 'idempotency_key_reused':
			return { title: '同一個操作編號被用在不同內容上，已拒絕', changed: NOTHING, next: '重新整理', action: 'refresh' };
		case 'operation_result_expired':
			return { title: '這次操作的結果紀錄已清除（不會重新執行）', changed: '請到議題頁確認結果。' };
		case 'unsupported_operation':
			return { title: '第一版不支援這個操作', changed: NOTHING };
		case 'invalid_request':
		case 'invalid_argument': {
			const where = typeof d.path === 'string' ? `（${d.path}）` : '';
			return { title: `送出的內容有誤${where}`, changed: NOTHING, next: '重新整理', action: 'refresh' };
		}
		case 'human_session_required':
			return { title: '這個操作只能由人登入後在網頁上做', changed: NOTHING };
		case 'invalid_cursor':
			return { title: '分頁位置已失效，請重新搜尋', changed: NOTHING, next: '重新搜尋', action: 'refresh' };
		case 'ambiguous_reference': {
			const c = names(d.candidates);
			return { title: c ? `有不只一個符合：${c}；請說得更明確` : '有不只一個符合，請說得更明確', changed: NOTHING };
		}
		case 'unsupported_execution_mode':
			return { title: '這個操作不支援這種執行方式', changed: NOTHING };
		case 'source_unverifiable':
		case 'invalid_citation':
		case 'citation_mismatch':
			return { title: '引用的來源無法核對（可能已修改或看不到）', changed: NOTHING };
		case 'invalid_consistency_token':
		case 'consistency_epoch_changed':
			return { title: '搜尋的同步標記已失效，請重新搜尋', changed: NOTHING, next: '重新搜尋', action: 'refresh' };
		case 'search_not_caught_up':
			return { title: '搜尋還沒同步到你剛寫入的內容；單本身已經可以開啟', changed: '寫入已完成。', next: '再找一次', action: 'retry' };
		default:
			return { title: e.message || '操作沒有完成', changed: e.status >= 500 ? UNKNOWN : NOTHING };
	}
}

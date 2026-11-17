// SPDX-License-Identifier: GPL-2.0-or-later
// What a person sees for an error (contract 02 §15.5): what happened, whether
// any data changed, and the next step. Programs decide on `code` only.

import { ApiError } from './api';
import type { ErrorBody } from '$api/types';
import { fieldLabel } from './format';

export type Shown = {
	title: string;
	/** Did anything change? */
	changed: string;
	/** The suggested next step, as button text (the page wires it up). */
	next?: string;
	/** Kind of next step the page can act on. */
	action?: 'login' | 'retry' | 'refresh' | 'conflict' | 'latest' | 'delegations' | 'queue' | 'reauth' | 'wait' | 'transitions' | 'narrow';
};

const NOTHING = '沒有改到任何資料。';
/** §6 無權限 wording: a read that finds nothing never says which of the two it was. */
export const NOT_FOUND = '找不到這個頁面，或你沒有權限查看。';
/** §6: shown next to a write the person may not do (the button is then disabled). */
export const FORBIDDEN = '你沒有權限執行這個動作';

/** A write refused for lack of permission (§6: disable the button, explain on hover). */
export function isForbidden(e: unknown): boolean {
	return e instanceof ApiError && e.code === 'forbidden_op';
}
const UNKNOWN = '尚未確認是否完成。';

/** `forbidden_op.details.reason` with its own wording (others: FORBIDDEN). */
const FORBIDDEN_REASON: Record<string, string> = {
	wiki_protected: '這一頁受保護，只有有保護權限的人能修改',
	project_not_writable: '專案已關閉，現在是唯讀'
};

/** R-W2: what is wrong with a wiki title (`wiki_title_invalid.details`), for the title field. */
export function wikiTitleProblem(d: Record<string, unknown>): string {
	switch (d.reason) {
		case 'length':
			return '標題要有 1～100 個字';
		case 'leading_dot':
			return '標題不能以「.」開頭';
		case 'char': {
			const ch = typeof d.char === 'string' ? d.char : '';
			const cp = typeof d.codepoint === 'string' ? d.codepoint : '';
			// Control and format characters cannot be seen: name the code point.
			const visible = ch !== '' && !/[\p{Cc}\p{Cf}\s]/u.test(ch);
			if (visible) return `標題不能有「${ch}」（不能用 [ ] | # ? / \\ : < > " 這些字元）`;
			return `標題有看不見的控制字元${cp ? ` ${cp}` : ''}，請刪掉後再試`;
		}
		default:
			return '標題不合法';
	}
}

/** R-F2: what is wrong with a fix rule (`fix_rule_invalid.details`), shown on that rule. */
export function fixRuleProblem(d: Record<string, unknown>): string {
	switch (d.reason) {
		case 'keywords_empty':
			return '至少要有一個關鍵字';
		case 'keyword_invalid':
			return '關鍵字要是 1～30 個字，不能有空白、「:」「,」「#」';
		case 'keyword_duplicate':
			return '同一個專案裡，關鍵字不能重複（不分大小寫）';
		case 'too_many_keywords':
			return '一條規則最多 10 個關鍵字';
		case 'status_unknown':
			return '目標狀態不存在，請重新選擇';
		case 'tracker_not_in_project':
			return '有追蹤類型不屬於這個專案，請重新選擇';
		case 'too_many_rules':
			return '一個專案最多 5 條修正規則';
		default:
			return '規則不合法';
	}
}

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
	unknown_kind: '分頁種類不正確',
	// Wiki (design 06 §3.5, §5).
	wiki_cycle: '不能把頁面移到它自己或它的子頁下面',
	wiki_depth: '頁面最多只能有 10 層',
	wiki_version_limit: '這一頁的版本數已達上限（10,000 版）',
	wiki_page_limit: '這個 wiki 的頁數已達上限（5,000 頁）',
	wiki_comment: '修改說明最多 255 字，也不能有控制字元',
	wiki_body: '內容有不允許的字元（NUL）',
	start_page_invalid: '起始頁要是這個 wiki 裡現有的頁面',
	redacted_source: '被遮蔽的版本不能拿來回復'
};
/** `tabs[5].url` → 「第 6 列：」. */
function rowOf(path: unknown): string {
	const m = typeof path === 'string' ? /^tabs\[(\d+)\]/.exec(path) : null;
	return m ? `第 ${Number(m[1]) + 1} 列：` : '';
}

const FILTER_REASON: Record<string, string> = {
	project_scope_required: '用名稱或 next 指定里程碑時，要再加上一個專案（例如 project:AUTH）',
	no_next_version: '這個專案沒有「下一個里程碑」（要是開啟、有日期、還沒過期的里程碑）',
	unknown_version: '這個專案沒有這個名稱的里程碑'
};
const LOOKUP: Record<string, string> = { project: '專案', tracker: '類型', status: '狀態', assignee: '指派對象', priority: '優先度', author: '作者' };

const VIEW_REASON: Record<string, string> = {
	unknown_field: '有不認得的欄位',
	invalid_shape: '格式不正確',
	invalid_value: '有不合法的值',
	kind_mismatch: '設定和視圖種類不符',
	lanes_required: '看板依狀態分欄時要列出狀態',
	restricted_field: '專案視圖不能用受限欄位',
	project_scope: '專案視圖只能限定本專案',
	too_large: '設定太大',
	duplicate_value: '有重複的值'
};
const FORM_KIND: Record<string, string> = {
	unknown_field: '有不認得的欄位',
	restricted_condition_field: '顯示條件不能用受限欄位',
	condition_too_complex: '顯示條件太複雜',
	cycle: '顯示條件互相依賴',
	hides_required: '這個欄位在某些情況必填，不能被顯示條件藏起來',
	field_not_applicable: '有欄位不適用於這個類型'
};

const TIME_FIELD: Record<string, string> = { issue: '議題', comments: '說明', hours: '時數', spent_on: '日期', activity_id: '活動' };
const showH = (h: unknown) => String(Math.round(Number(h) * 100) / 100);
/** Time entry rules (contract 05 §3.5.5 T1–T7, wording §6.6). */
function timeKind(d: Record<string, unknown>): string | null {
	switch (d.kind) {
		case 'max_hours_per_day': {
			const max = showH(d.max_hours_per_day);
			if (d.remaining === undefined || d.remaining === null) return `超過每天可記的上限（${max} 小時）`;
			const used = showH(Number(d.max_hours_per_day) - Number(d.remaining));
			return `這天已經記了 ${used} 小時，上限是 ${max} 小時（還能記 ${showH(d.remaining)}）`;
		}
		case 'future_date':
			return '不能記未來的日期';
		case 'closed_issue':
			return '這張單已結案，不接受記工時';
		case 'hours_invalid':
			return '時數要是 0～1000、最多兩位小數（記 0 小時要管理員開放）';
		case 'activity_inactive':
			return '這個活動已停用，請選別的活動';
		case 'snooze_range':
			return '稍後處理的時間要在 1 小時到 90 天之後';
		case 'activity_in_use':
			return '已有工時的活動不能刪除，只能停用';
		default:
			return null;
	}
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
			return { title: (typeof d.reason === 'string' && FORBIDDEN_REASON[d.reason]) || FORBIDDEN, changed: NOTHING };
		case 'not_found':
			return { title: NOT_FOUND, changed: NOTHING };
		case 'invalid_value': {
			const time = timeKind(d);
			if (time) return { title: time, changed: NOTHING };
			const why = typeof d.kind === 'string' ? INVALID_KIND[d.kind] : undefined;
			if (why) return { title: `${rowOf(d.path)}${why}`, changed: NOTHING };
			const allowed = names(d.allowed);
			return { title: allowed ? `值不合法；可以用：${allowed}` : '值不合法', changed: NOTHING };
		}
		case 'required_field_missing':
		{
			const list = d.field ? [String(d.field)] : Array.isArray(d.fields) ? d.fields.map(String) : [];
			const shown = list.map((f) => TIME_FIELD[f] ?? fieldLabel(f)).join('、');
			return { title: `缺少必填欄位${shown ? `：${shown}` : ''}`, changed: NOTHING };
		}
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
		case 'payload_too_large':
			return { title: '內容太大，超過上限', changed: NOTHING };
		case 'insufficient_storage':
			return { title: '伺服器的儲存空間不足，暫時不能上傳', changed: NOTHING, next: '稍後重試', action: 'retry' };
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
			return { title: '查詢語法有誤', changed: NOTHING };
		case 'invalid_filter': {
			const why = typeof d.reason === 'string' ? FILTER_REASON[d.reason] : d.path === 'version' ? FILTER_REASON.unknown_version : undefined;
			if (why) return { title: why, changed: NOTHING };
			return { title: typeof d.path === 'string' ? `查詢條件有誤（${fieldLabel(d.path)}）` : '查詢條件有誤', changed: NOTHING };
		}
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
			// A name the person typed (search, ⌘K) that matches nothing they can see.
			if (typeof d.path === 'string' && d.path in LOOKUP) {
				const allowed = names(d.allowed);
				return { title: `找不到這個${LOOKUP[d.path]}，或你沒有權限查看${allowed ? `；可以用：${allowed}` : ''}`, changed: NOTHING };
			}
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
		// ---- M1 (contract 05 §7) ----
		case 'triage_state_invalid':
			return { title: '這張單已經不在待分流（可能別人剛處理過），尚未套用任何變更', changed: NOTHING, next: '重新載入', action: 'refresh' };
		case 'field_definition_changed':
			return { title: '條件用到的自訂欄位設定剛被改過，請重新選擇條件', changed: NOTHING, next: '重新載入', action: 'refresh' };
		case 'query_timeout':
			// §6: 「縮小範圍」 opens the condition editor (pages without one retry).
			return { title: '查詢太久沒有完成；請縮小範圍（例如加上專案、狀態或日期）再試', changed: NOTHING, next: '縮小範圍', action: 'narrow' };
		case 'invalid_view': {
			const why = typeof d.reason === 'string' ? VIEW_REASON[d.reason] : undefined;
			const where = typeof d.path === 'string' ? `（${d.path}）` : '';
			return { title: `視圖設定有誤${where}${why ? `：${why}` : ''}`, changed: NOTHING };
		}
		case 'view_name_taken':
			return { title: '已經有同名的視圖，請換一個名稱', changed: NOTHING };
		case 'view_in_use': {
			const tabs = names(d.tabs);
			return { title: `這個視圖還被專案分頁使用${tabs ? `（${tabs}）` : ''}，請先移除分頁再刪除`, changed: NOTHING };
		}
		case 'form_rules_changed':
			return { title: '表單規則剛更新過，請重新載入表單後再送出', changed: NOTHING, next: '重新載入', action: 'refresh' };
		case 'form_invalid': {
			const why = typeof d.kind === 'string' ? FORM_KIND[d.kind] : undefined;
			return { title: why ? `表單規則有誤：${why}` : '表單規則有誤', changed: NOTHING };
		}
		case 'form_publish_blocked':
			return { title: '有既有的單不符合新規則，選了「有違規就不發布」所以沒有發布', changed: NOTHING };
		case 'concept_label_taken':
			return typeof d.label === 'string' && typeof d.pref_label === 'string'
				? { title: `「${d.label}」已經是「${d.pref_label}」的標籤`, changed: NOTHING }
				: { title: '這個標籤已經屬於另一個概念，請換一個或到那個概念修改', changed: NOTHING };
		case 'runtime_context_closed':
			return { title: '這個執行環境的工作脈絡已關閉，請建立新的工作脈絡', changed: NOTHING, next: '管理委派', action: 'delegations' };
		case 'runtime_client_taken': {
			const label = typeof d.label === 'string' ? `「${d.label}」` : '另一個';
			return { title: `同一個 agent 軟體已在${label}執行環境裡；請放進那個執行環境，或先退役它`, changed: NOTHING, next: '管理委派', action: 'delegations' };
		}
		case 'ai_unavailable':
			return d.kind === 'circuit_open'
				? { title: 'AI 暫時無法使用；資料與操作正常', changed: NOTHING, next: '稍後重試', action: 'retry' }
				: { title: 'AI 助理目前沒有啟用或尚未設定完成；資料與操作正常', changed: NOTHING };
		case 'ai_provider_error':
			return { title: d.kind === 'timeout' ? 'AI 服務回應逾時' : 'AI 服務回應錯誤', changed: NOTHING, next: '重試', action: 'retry' };
		case 'ai_model_disallowed':
			return { title: '這個模型或端點在 D17 禁用名單內（不得使用中國公司的模型或服務）', changed: NOTHING };
		case 'ai_endpoint_invalid':
			return {
				title:
					d.kind === 'no_key'
						? 'Anthropic 需要 key：請在伺服器設定 OXLEDGER_AI_KEY_〈名稱〉'
						: d.kind === 'endpoint_blocked'
							? '端點解析到不允許的位址（私有網段或本機需要伺服器另外允許）'
							: '這個端點已不在伺服器設定中',
				changed: NOTHING
			};
		case 'ai_conversation_read_only':
			return { title: '這個對話已封存（AI 的工作脈絡已更換），只能查看；請開新對話', changed: NOTHING };
		case 'ai_provider_changed': {
			const p = (d.provider ?? {}) as { name?: string; model?: string };
			const who = p.name ? `〈${p.name}${p.model ? `（${p.model}）` : ''}〉` : '新的服務';
			return { title: `AI 服務已換成${who}；繼續這個對話時，先前的對話內容會送到新服務`, changed: NOTHING };
		}
		// ---- M2.2 wiki (design 06 §5) ----
		case 'wiki_page_exists':
			return { title: '已經有同名的頁面', changed: NOTHING, next: '換一個標題' };
		case 'wiki_has_children': {
			const n = typeof d.children === 'number' ? d.children : null;
			return { title: `這一頁還有${n !== null ? ` ${n} 個` : ''}子頁，刪除時要一併決定子頁移到哪裡`, changed: NOTHING };
		}
		case 'wiki_title_invalid':
			return { title: wikiTitleProblem(d), changed: NOTHING };
		case 'fix_rule_invalid':
			return { title: fixRuleProblem(d), changed: NOTHING };
		case 'ai_run_in_progress':
			return { title: '你還有一個 AI 回覆在進行中；等它完成或取消後再送', changed: NOTHING, action: 'wait' };
		default:
			return { title: e.message || '操作沒有完成', changed: e.status >= 500 ? UNKNOWN : NOTHING };
	}
}

/**
 * An execution's stored error (proposal applied by the queue or the proposal
 * page), said the §15.5 way instead of the server's English message.
 */
export function describeFailure(err: { code: string; message: string; details?: unknown } | null | undefined): string {
	if (!err) return '套用失敗，尚未套用任何變更';
	if (err.code === 'conflict') return '這張單在提案後被改過，尚未套用任何變更';
	const body = { ...err, next_actions: [], request_id: '', retry_after_ms: null } as unknown as ErrorBody;
	return describeError(new ApiError(409, body, null)).title;
}

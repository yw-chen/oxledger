// Fix keywords (design 06 §3.8, §6.8): the words the development card and
// the fix-rules page show for the server's reason codes.
import type { DevFix } from '$api/types';

/** Why a `fixes AUTH-1` did not move the issue by itself (R-F6). */
export const FIX_REASON: Record<string, string> = {
	no_rule: '本專案沒有啟用對應的規則',
	already_closed: '單已經關閉',
	in_triage: '單還在分流',
	// Also when the rule's sponsor cannot see the issue: the same words on purpose.
	workflow_disallowed: '規則設定者無法對這張單做這個狀態轉換',
	direct_push: '直接推到預設分支，依設定只提示',
	form_rule: '目標狀態有必填欄位沒填',
	sponsor_inactive: '規則設定者已失去權限，規則已停用',
	sponsor_lost_permission: '規則設定者已失去權限，規則已停用',
	too_many: '一次修正的單太多（超過 50 張）',
	not_on_default_branch: '沒有合併進預設分支',
	unconfirmed: '無法向 GitHub 確認',
	project_inactive: '專案已關閉或封存',
	pr_hidden: 'PR 在這個專案被隱藏'
};

/** Why a rule is off (`disabled_reason`), for the settings page. */
export const DISABLED_REASON: Record<string, string> = {
	sponsor_inactive: '已停用：規則設定者的帳號已停用。重新啟用後會改以你的權限執行。',
	sponsor_lost_permission: '已停用：規則設定者已不能管理這個專案。重新啟用後會改以你的權限執行。',
	status_removed: '已停用：目標狀態已不存在。',
	manual: '已手動停用'
};

/** `acme/app#12` or `acme/app@1a2b3c4`. */
export function fixSource(f: Pick<DevFix, 'repo' | 'pr_number' | 'sha'>): string {
	if (f.pr_number != null) return `${f.repo}#${f.pr_number}`;
	return `${f.repo}@${(f.sha ?? '').slice(0, 7)}`;
}

/** What the card says about one fix link; null when there is nothing to say yet (`pending`). */
export function fixText(f: DevFix): { kind: 'applied' | 'hint'; text: string } | null {
	if (f.state === 'applied') return { kind: 'applied', text: `已自動轉為「${f.target_status ?? '目標狀態'}」` };
	if (f.state === 'suggested' || f.state === 'skipped') {
		const why = (f.reason && FIX_REASON[f.reason]) || '沒有符合的條件';
		return { kind: 'hint', text: `寫了 ${f.keyword}，沒有自動轉換：${why}` };
	}
	return null;
}

/** The history line of a `fixed_by` transition's source (`detail.fix`). */
export function fixEventSource(fix: unknown): string {
	if (!fix || typeof fix !== 'object') return '';
	const f = fix as Record<string, unknown>;
	if (typeof f.pr_number === 'number') return `PR #${f.pr_number}`;
	if (typeof f.sha7 === 'string') return f.sha7;
	return '';
}

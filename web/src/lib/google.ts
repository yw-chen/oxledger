// SPDX-License-Identifier: GPL-2.0-or-later
// Google sign-in (server: crates/oxledger/src/google.rs). The round trip is a
// page navigation, not an API call: the browser goes to
// /api/v1/auth/google/start, on to Google, and comes back into the SPA — or to
// /login?google_error=<code> when it did not work.

/** What the sign-in page says for each `google_error` code. */
const GOOGLE_ERROR: Record<string, string> = {
	not_configured: '這個站沒有開啟 Google 登入。',
	cancelled: '已取消 Google 登入。',
	state: '登入逾時或不是從這個瀏覽器開始的，請再試一次。',
	exchange: '無法向 Google 確認身分，請稍後再試。',
	unverified: '這個 Google 帳號的 email 還沒有通過 Google 驗證。',
	domain: '這個 Google 帳號的網域不能登入這個站。',
	no_account: '這個 Google 帳號沒有對應的帳號，請聯絡管理員先建立帳號。',
	locked: '帳號已被鎖定，請聯絡管理員。',
	other_account: '這不是這個帳號連結的 Google 帳號。',
	setup_not_allowed: '這個站還沒有設定，而這個 Google 帳號不能建立機構。',
	signed_out: '登入已失效，請重新登入後再試。'
};

/** The text for a `google_error` code; null when there is none. */
export function googleErrorText(code: string | null | undefined): string | null {
	if (!code) return null;
	return GOOGLE_ERROR[code] ?? 'Google 登入沒有成功，請再試一次。';
}

/**
 * Where to send the browser to sign in with Google and come back to `next`
 * (a path on this origin; the server checks it again). `reauth` asks for a
 * step-up of the current session instead of a sign-in.
 */
export function googleStartUrl(next: string, reauth = false): string {
	const q = new URLSearchParams();
	if (reauth) q.set('reauth', '1');
	if (next && next !== '/') q.set('next', next);
	const s = q.toString();
	return `/api/v1/auth/google/start${s ? `?${s}` : ''}`;
}

/** The browser's time zone when the list has it, else `fallback`. */
export function defaultTimeZone(zones: string[], fallback: string): string {
	try {
		const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
		if (tz && (zones.length === 0 || zones.includes(tz))) return tz;
	} catch {
		// Fall through.
	}
	return fallback;
}

/** IANA time zones this browser knows; empty when it cannot list them. */
export function timeZones(): string[] {
	try {
		const f = (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf;
		return f ? f('timeZone') : [];
	} catch {
		return [];
	}
}

/** What is wrong with the setup form, field by field; empty when it may be sent. */
export function setupProblems(f: { org_name: string; login: string; name: string; timezone: string }): Record<string, string> {
	const p: Record<string, string> = {};
	const org = f.org_name.trim();
	if (!org) p.org_name = '請填機構名稱。';
	else if ([...org].length > 100) p.org_name = '機構名稱最多 100 個字。';
	const login = f.login.trim();
	if (!login) p.login = '請填帳號。';
	else if (/\s/.test(login) || [...login].length > 64) p.login = '帳號最多 64 個字，不能有空白。';
	const name = f.name.trim();
	if (!name) p.name = '請填姓名。';
	else if ([...name].length > 100) p.name = '姓名最多 100 個字。';
	if (!f.timezone.trim()) p.timezone = '請選時區。';
	return p;
}

// SPDX-License-Identifier: GPL-2.0-or-later
// M2.2 wiki (design 06 §6.3–§6.6) against the real backend: create pages
// (the first one from the empty wiki, one from a link to a missing page),
// edit, a conflict that keeps the text, rename with a redirect, protect and
// undo, history / version / diff and revert, D12 on a version, delete with
// children and restore, the title errors, keys, and a person who cannot
// read the wiki seeing the one 「找不到」. On AUTH: alice is Manager (every
// wiki permission), bob Developer (read and edit), carol Reporter (no
// view_wiki_pages). Every wait has a limit (playwright.config.ts).
import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test';

const PASSWORD = 'oxledger-demo-pass'; // test value for the oxledger_fe_* databases only
const NOT_FOUND = '找不到這個頁面，或你沒有權限查看。';
test.describe.configure({ mode: 'serial' });

const contexts: BrowserContext[] = [];
test.afterEach(async () => {
	for (const c of contexts.splice(0)) await c.close();
});

async function as(browser: Browser, user: string, next: string): Promise<Page> {
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const page = await ctx.newPage();
	// Confirmations (revert, save over a newer version) are accepted.
	page.on('dialog', (d) => d.accept());
	await page.goto(`/login?next=${encodeURIComponent(next)}`);
	await page.fill('input[name=username]', user);
	await page.fill('input[name=password]', PASSWORD);
	await page.click('button[type=submit]');
	await page.waitForURL((u) => u.pathname === new URL(next, 'http://x').pathname);
	await page.waitForLoadState('networkidle');
	const ob = page.getByTestId('onboarding');
	if (await ob.isVisible()) {
		await ob.getByTestId('ob-skip').click();
		await expect(ob).toHaveCount(0);
	}
	return page;
}

/** A REST call inside the page with the session's CSRF token. */
async function papi(page: Page, method: string, path: string, body?: unknown): Promise<{ status: number; json: any }> {
	return page.evaluate(
		async ({ method, path, body }) => {
			const s = await (await fetch('/api/v1/auth/session')).json();
			const headers: Record<string, string> = { 'Content-Type': 'application/json' };
			if (method !== 'GET') {
				headers['X-CSRF-Token'] = s.data?.csrf_token;
				headers['Idempotency-Key'] = crypto.randomUUID();
			}
			const r = await fetch('/api/v1' + path, { method, headers, body: method === 'GET' ? undefined : JSON.stringify(body ?? {}) });
			return { status: r.status, json: await r.json().catch(() => null) };
		},
		{ method, path, body }
	);
}
const wpath = (title: string, suffix = '') => `/project/AUTH/wiki/${encodeURIComponent(title.replace(/ /g, '_'))}${suffix}`;
async function getPage(page: Page, title: string) {
	const r = await papi(page, 'GET', `/projects/AUTH/wiki/pages/${encodeURIComponent(title.replace(/ /g, '_'))}`);
	expect(r.status).toBe(200);
	return r.json.data as { id: string; lock_version: number; version: number; title: string };
}
async function put(page: Page, op: Record<string, unknown>, pre?: { id: string; lock: number }) {
	const r = await papi(page, 'POST', '/commands', {
		command_schema_version: 1,
		ops: [{ op_id: crypto.randomUUID(), ...op }],
		...(pre ? { preconditions: [{ wiki_page: pre.id, lock_version: pre.lock }] } : {})
	});
	expect(r.status, JSON.stringify(r.json)).toBe(200);
	return r.json.data;
}

test('wiki：空的 wiki 建立第一頁，連到不存在的頁再建立它', async ({ browser }) => {
	const page = await as(browser, 'alice', '/project/AUTH/wiki');
	await expect(page.getByTestId('ptabs').getByRole('link', { name: 'Wiki' })).toHaveAttribute('aria-current', 'page');
	await expect(page.getByTestId('wiki-empty')).toContainText('這個專案還沒有 wiki 頁面。');
	await page.getByTestId('wiki-create-first').click();
	await page.waitForURL('**/wiki/Wiki/edit');
	await expect(page.getByTestId('wiki-title-input')).toHaveValue('Wiki');
	await page.getByTestId('wiki-md-input').fill('# 首頁\n\n見 [[部署 與 回滾]]。');
	await page.getByTestId('wiki-comment').fill('第一版');
	await page.getByTestId('wiki-save').click();
	await page.waitForURL((u) => u.pathname === '/project/AUTH/wiki/Wiki');
	await expect(page.getByTestId('wiki-title')).toHaveText('Wiki');
	await expect(page.getByTestId('wiki-status')).toContainText('第 1 版');
	await expect(page.getByTestId('wiki-undo-bar')).toContainText('已建立〈Wiki〉');

	// The tab now opens on the start page (R-W13: the page titled "Wiki").
	await page.goto('/project/AUTH/wiki');
	await expect(page.getByTestId('wiki-title')).toHaveText('Wiki');
	// A link to a page that does not exist yet: alice may create it.
	const link = page.getByTestId('wiki-body').locator('a.ox-wiki');
	await expect(link).toHaveClass(/ox-wiki-new/);
	await expect(link).toHaveAttribute('href', /^\/project\/AUTH\/wiki\//);
	await link.click();
	await expect(page.getByTestId('wiki-missing')).toContainText('這一頁還不存在。');
	await page.getByTestId('wiki-create').click();
	await expect(page.getByTestId('wiki-title-input')).toHaveValue('部署 與 回滾');
	await page.getByTestId('wiki-parent').selectOption({ label: 'Wiki' });
	await page.getByTestId('wiki-md-input').fill(['步驟一', '步驟二', '步驟三', '步驟四', '步驟五'].join('\n'));
	await page.getByTestId('wiki-save').click();
	await page.waitForURL((u) => decodeURIComponent(u.pathname) === '/project/AUTH/wiki/部署_與_回滾');
	await expect(page.getByTestId('wiki-crumbs')).toContainText(/Wiki\s*›\s*Wiki\s*›\s*部署 與 回滾/);
	await expect(page.getByTestId('wiki-tree')).toContainText('部署 與 回滾');
});

test('wiki：標題錯誤顯示在標題欄下（同名、不能用的字元、看不見的字元）', async ({ browser }) => {
	const page = await as(browser, 'alice', '/project/AUTH/wiki-new');
	const title = page.getByTestId('wiki-title-input');
	await page.getByTestId('wiki-md-input').fill('內容');
	// Case and width do not make another title (R-W2).
	await title.fill('ＷＩＫＩ');
	await page.getByTestId('wiki-save').click();
	await expect(page.getByTestId('wiki-title-error')).toContainText('已經有同名的頁面');
	await expect(page.getByTestId('wiki-title-error').getByRole('link')).toHaveText('開啟〈Wiki〉');
	await title.fill('a#b');
	await page.getByTestId('wiki-save').click();
	await expect(page.getByTestId('wiki-title-error')).toContainText('標題不能有「#」');
	await title.fill('evil‮txt');
	await page.getByTestId('wiki-save').click();
	await expect(page.getByTestId('wiki-title-error')).toContainText('U+202E');
	await expect(page).toHaveURL(/wiki-new/);
});

test('wiki：編輯時別人先存了 → 紅條、比較差異、以我的內容重新儲存', async ({ browser }) => {
	const page = await as(browser, 'alice', wpath('部署 與 回滾', '/edit'));
	const bob = await as(browser, 'bob', '/project/AUTH/wiki');
	await expect(page.getByTestId('wiki-md-input')).toHaveValue(/步驟一/);
	await page.getByTestId('wiki-md-input').fill(['步驟一', '步驟二（alice 改）', '步驟三', '步驟四', '步驟五'].join('\n'));
	// bob saves first, with the lock_version alice opened.
	const p = await getPage(bob, '部署 與 回滾');
	await put(bob, { kind: 'wiki_put', target: { project: 'AUTH', page_id: p.id }, body: ['步驟一', '步驟二', '步驟三', '步驟四', '步驟五（bob 改）'].join('\n') }, { id: p.id, lock: p.lock_version });

	await page.getByTestId('wiki-save').click();
	const bar = page.getByTestId('wiki-conflict');
	await expect(bar).toContainText('這一頁在你編輯時被 Bob Chen 更新了（第 2 版）');
	// Nothing typed is lost, and saving stays off until the person decides.
	await expect(page.getByTestId('wiki-md-input')).toHaveValue(/alice 改/);
	await expect(page.getByTestId('wiki-save')).toBeDisabled();
	await page.getByTestId('wiki-conflict-compare').click();
	const dlg = page.getByTestId('wiki-conflict-diff');
	await dlg.getByTestId('wiki-conflict-diffview-unified').click();
	await expect(dlg.locator('tr[data-op=delete]')).toHaveCount(2);
	await expect(dlg.locator('tr[data-op=insert]')).toHaveCount(2);
	await expect(dlg.locator('tr[data-op=insert]').first()).toContainText('+');
	await expect(dlg.locator('tr[data-op=insert]').first()).toContainText('步驟二（alice 改）');
	await dlg.getByRole('button', { name: '關閉' }).click();
	await page.getByTestId('wiki-conflict-mine').click();
	await page.waitForURL((u) => decodeURIComponent(u.pathname) === '/project/AUTH/wiki/部署_與_回滾');
	await expect(page.getByTestId('wiki-status')).toContainText('第 3 版');
	await expect(page.getByTestId('wiki-body')).toContainText('步驟二（alice 改）');
});

test('wiki：歷史、單版、差異與回復；被遮蔽的版本不能回復', async ({ browser }) => {
	const page = await as(browser, 'alice', wpath('部署 與 回滾'));
	// H opens the history (§6.3 keys).
	await page.locator('body').press('h');
	await page.waitForURL(/\/history$/);
	const rows = page.getByTestId('wiki-version-row');
	await expect(rows).toHaveCount(3);
	await expect(rows.first()).toContainText('目前');
	await expect(rows.last()).toContainText('第 1 版');

	// Compare 1 → 3: the diff page, with `−`／`+` and the folded stretch.
	await page.getByRole('radio', { name: '從第 1 版比較' }).check();
	await page.getByRole('radio', { name: '比較到第 3 版' }).check();
	await page.getByTestId('wiki-compare').click();
	await page.waitForURL(/\/diff\?from=1&to=3$/);
	const diff = page.getByTestId('wiki-diff');
	await diff.getByTestId('wiki-diff-unified').click();
	await expect(diff.locator('tr[data-op=delete]')).toHaveCount(1);
	await expect(diff.locator('tr[data-op=delete]')).toContainText('−');
	await expect(diff.locator('tr[data-op=insert]')).toContainText('步驟二（alice 改）');
	await expect(diff.getByTestId('wiki-diff-truncated')).toHaveCount(0);
	await diff.getByTestId('wiki-diff-split').click();
	await expect(diff.locator('span.wins')).toContainText(['（alice 改）']);

	// D12: redact version 2 from the history; its revert is off afterwards.
	await page.goto(wpath('部署 與 回滾', '/history'));
	const v2 = page.locator('[data-testid=wiki-version-row][data-version="2"]');
	await v2.getByRole('button', { name: '第 2 版的更多動作' }).click();
	await page.getByTestId('wiki-version-redact').click();
	const red = page.getByTestId('wiki-redact');
	await red.getByTestId('wiki-redact-code').selectOption('confidential');
	await red.getByTestId('wiki-redact-reason').fill('E2E：含不該公開的步驟');
	await red.getByTestId('wiki-redact-confirm').click();
	const step = page.getByTestId('step-up');
	if (await step.isVisible().catch(() => false)) {
		await step.getByTestId('step-up-password').fill(PASSWORD);
		await step.getByTestId('step-up-confirm').click();
	}
	await expect(red).toHaveCount(0);
	await expect(v2.getByTestId('wiki-version-redacted')).toBeVisible();
	await expect(v2.getByTestId('wiki-version-revert')).toBeDisabled();
	await expect(v2.getByTestId('wiki-version-revert')).toHaveAttribute('title', '這一版已遮蔽');
	await v2.getByTestId('wiki-version-view').click();
	await expect(page.getByTestId('wiki-version-bar')).toContainText('你在看第 2 版（目前是第 3 版）');
	await expect(page.getByTestId('wiki-version-revert')).toBeDisabled();

	// Version 1: 「回復到這一版」 makes version 4 with its content.
	await page.goto(wpath('部署 與 回滾', '/versions/1'));
	await expect(page.getByTestId('wiki-version-bar')).toContainText('你在看第 1 版（目前是第 3 版）');
	await page.getByTestId('wiki-version-revert').click();
	await page.waitForURL((u) => decodeURIComponent(u.pathname) === '/project/AUTH/wiki/部署_與_回滾');
	await expect(page.getByTestId('wiki-status')).toContainText('第 4 版');
	await expect(page.getByTestId('wiki-body')).not.toContainText('alice 改');
});

test('wiki：保護後別人的編輯停用並說明；復原保護', async ({ browser }) => {
	const page = await as(browser, 'alice', wpath('部署 與 回滾'));
	await page.getByTestId('wiki-menu').click();
	await page.getByTestId('wiki-protect').click();
	await expect(page.getByTestId('wiki-protected')).toContainText('受保護');
	await expect(page.locator('.toast')).toContainText(['已保護']);

	const bob = await as(browser, 'bob', wpath('部署 與 回滾'));
	await expect(bob.getByTestId('wiki-edit')).toBeDisabled();
	await expect(bob.getByTestId('wiki-edit')).toHaveAttribute('title', '這一頁受保護，只有有保護權限的人能編輯');
	await bob.getByTestId('wiki-menu').click();
	await expect(bob.getByTestId('wiki-delete')).toBeDisabled();
	// bob has view_wiki_edits: 歷史 is there.
	await expect(bob.getByTestId('wiki-history')).toBeVisible();

	// alice undoes her protect (R-W9).
	await page.getByTestId('wiki-undo').click();
	await expect(page.getByTestId('wiki-undo-preview')).toContainText('〈部署 與 回滾〉');
	await page.getByTestId('wiki-undo-confirm').click();
	await expect(page.getByTestId('wiki-protected')).toHaveCount(0);
	await bob.reload();
	await expect(bob.getByRole('link', { name: '編輯' })).toBeVisible();
});

test('wiki：改名保留轉址，舊網址開啟顯示轉址灰條；E 開編輯', async ({ browser }) => {
	const page = await as(browser, 'alice', wpath('部署 與 回滾'));
	await page.getByTestId('wiki-menu').click();
	await page.getByTestId('wiki-move').click();
	const dlg = page.getByTestId('wiki-move-dialog');
	await dlg.getByTestId('wiki-move-title').fill('部署手冊');
	await expect(dlg.getByTestId('wiki-move-keep')).toBeChecked();
	// A page cannot move under itself: only the other pages are offered.
	await expect(dlg.getByTestId('wiki-move-parent').locator('option')).toHaveText(['（最上層）', 'Wiki']);
	await dlg.getByTestId('wiki-move-confirm').click();
	await page.waitForURL((u) => decodeURIComponent(u.pathname) === '/project/AUTH/wiki/部署手冊');
	await expect(page.getByTestId('wiki-title')).toHaveText('部署手冊');
	await expect(page.getByTestId('wiki-redirected')).toHaveCount(0);

	await page.goto(wpath('部署 與 回滾'));
	await expect(page.getByTestId('wiki-redirected')).toHaveText('由〈部署 與 回滾〉轉址過來');
	await expect(page.getByTestId('wiki-title')).toHaveText('部署手冊');
	await page.locator('body').press('e');
	await page.waitForURL(/\/edit$/);
	await expect(page.getByTestId('wiki-md-input')).toHaveValue(/步驟一/);
});

test('wiki：刪除有子頁的頁（子頁上移），由最近刪除檢視並還原', async ({ browser }) => {
	const page = await as(browser, 'alice', wpath('部署手冊'));
	const parent = await getPage(page, '部署手冊');
	await put(page, { kind: 'wiki_put', target: { project: 'AUTH', page: '回滾細節' }, body: '子頁', parent: { page_id: parent.id } });
	await page.reload();
	await expect(page.getByTestId('wiki-children')).toContainText('回滾細節');
	await page.getByTestId('wiki-menu').click();
	await page.getByTestId('wiki-delete').click();
	const dlg = page.getByTestId('wiki-delete-dialog');
	await expect(dlg).toContainText('刪除〈部署手冊〉？');
	await expect(dlg).toContainText('30 天內可以由 wiki 管理者還原。');
	await expect(dlg.getByTestId('wiki-delete-children')).toHaveText('它的 1 個子頁會移到〈Wiki〉下面。');
	await dlg.getByTestId('wiki-delete-confirm').click();
	await page.waitForURL((u) => u.pathname === '/project/AUTH/wiki');
	// The deleted page reads like a missing one (R-W16)…
	await page.goto(wpath('部署手冊'));
	await expect(page.getByTestId('wiki-missing')).toBeVisible();
	// …and the child moved up to Wiki.
	await page.goto('/project/AUTH/wiki');
	await expect(page.getByTestId('wiki-children')).toContainText('回滾細節');
	const item = page.getByTestId('wiki-deleted-item').filter({ hasText: '部署手冊' });
	await item.getByRole('link', { name: '檢視' }).click();
	await expect(page.getByTestId('wiki-deleted-bar')).toContainText('這一頁已刪除');
	await expect(page.getByTestId('wiki-body')).toContainText('步驟一');
	await page.getByTestId('wiki-restore').click();
	await page.waitForURL((u) => decodeURIComponent(u.pathname) === '/project/AUTH/wiki/部署手冊');
	await expect(page.getByTestId('wiki-title')).toHaveText('部署手冊');
});

test('wiki：`__init__` 這類底線標題能建立、用網址開啟，底線與空白的寫法是同一頁', async ({ browser }) => {
	const page = await as(browser, 'alice', '/project/AUTH/wiki-new');
	await page.getByTestId('wiki-title-input').fill('__init__');
	await page.getByTestId('wiki-md-input').fill('模組初始化');
	await page.getByTestId('wiki-save').click();
	await page.waitForURL((u) => u.pathname === '/project/AUTH/wiki/__init__');
	await expect(page.getByTestId('wiki-title')).toHaveText('__init__');
	// Every run of `_` and spaces is one `_` in the title key: these name the same page.
	for (const path of ['/project/AUTH/wiki/__init__', '/project/AUTH/wiki/_init_', `/project/AUTH/wiki/${encodeURIComponent('_ init _')}`]) {
		await page.goto(path);
		await expect(page.getByTestId('wiki-title')).toHaveText('__init__');
		await expect(page.getByTestId('wiki-body')).toContainText('模組初始化');
	}
	// …and creating it again under another spelling is the same title.
	await page.goto('/project/AUTH/wiki-new');
	await page.getByTestId('wiki-title-input').fill('_init_');
	await page.getByTestId('wiki-md-input').fill('另一頁？');
	await page.getByTestId('wiki-save').click();
	await expect(page.getByTestId('wiki-title-error')).toContainText('已經有同名的頁面');
	// A [[__init__]] link in another page opens it.
	await put(page, { kind: 'wiki_put', target: { project: 'AUTH', page: '底線連結' }, body: '見 [[__init__]]。' });
	await page.goto(wpath('底線連結'));
	const link = page.getByTestId('wiki-body').locator('a.ox-wiki');
	await expect(link).not.toHaveClass(/ox-wiki-new/);
	await link.click();
	await page.waitForURL((u) => u.pathname.startsWith('/project/AUTH/wiki/') && !u.pathname.endsWith(encodeURIComponent('底線連結')));
	await expect(page.getByTestId('wiki-title')).toHaveText('__init__');
});

test('wiki：讀不到 wiki 的人一律看到「找不到」，也沒有 Wiki 分頁', async ({ browser }) => {
	const carol = await as(browser, 'carol', '/project/AUTH/issues');
	await expect(carol.getByTestId('ptabs')).toBeVisible();
	await carol.waitForLoadState('networkidle');
	await expect(carol.getByTestId('ptabs').getByRole('link', { name: 'Wiki' })).toHaveCount(0);
	for (const path of ['/project/AUTH/wiki', wpath('Wiki'), wpath('沒有這頁'), wpath('Wiki', '/history'), wpath('Wiki', '/edit')]) {
		await carol.goto(path);
		await expect(carol.getByTestId('wiki-not-found')).toHaveText(NOT_FOUND);
	}
	// The same answer from the API for a real page and a missing one (R-W1).
	const real = await papi(carol, 'GET', '/projects/AUTH/wiki/pages/Wiki');
	const none = await papi(carol, 'GET', '/projects/AUTH/wiki/pages/no-such-page');
	expect(real.status).toBe(404);
	expect(real.json.error.code).toBe('not_found');
	expect(real.json.error.message).toBe(none.json.error.message);
});

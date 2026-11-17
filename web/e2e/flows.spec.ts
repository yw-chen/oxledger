// SPDX-License-Identifier: GPL-2.0-or-later
// The main human flows of contract 02 §14 against the real backend, with
// e2e/agent.mjs playing the agent over MCP: delegate and get a token (§16),
// open a proposal link and approve (用法 1, 2, 8), approve a batch from the
// queue (用法 3), revise a conflicting proposal (用法 7), suspend, undo, and
// the human's own direct path (report, edit). Runs serially on one fresh DB.
import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test';
// @ts-expect-error plain ESM helper without types
import { Agent } from './agent.mjs';
import { BACKEND_LOG, BACKEND_URL, MCP_URL, METRICS_TOKEN } from '../playwright.config';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, statSync } from 'node:fs';

const PASSWORD = 'oxledger-demo-pass'; // test value for the oxledger_fe_* databases only
test.describe.configure({ mode: 'serial' });

let token = '';
let agent: { call: (name: string, args: unknown) => Promise<any> };

async function login(page: Page, user: string, next = '/') {
	await page.goto(`/login?next=${encodeURIComponent(next)}`);
	await page.fill('input[name=username]', user);
	await page.fill('input[name=password]', PASSWORD);
	await page.click('button[type=submit]');
	// Lands on `next` itself (the login page must not wander on to "/").
	await page.waitForURL((u) => u.pathname === next.split('?')[0]);
	await page.waitForLoadState('networkidle');
	// …and stays there (a reactive read of `next` once sent people on to "/").
	await page.waitForTimeout(300);
	expect(new URL(page.url()).pathname).toBe(next.split('?')[0]);
	// §6.12 first sign-in asks for interface roles on the home page; carol's own test answers it.
	if (user !== 'carol') await skipOnboarding(page);
}
async function skipOnboarding(page: Page) {
	const ob = page.getByTestId('onboarding');
	if (await ob.isVisible()) {
		await ob.getByTestId('ob-skip').click();
		await expect(ob).toHaveCount(0);
	}
}

/** A REST call inside the page with the session's CSRF token. */
async function papi(page: Page, method: string, path: string, body?: unknown): Promise<{ status: number; json: any }> {
	try {
		return await papiOnce(page, method, path, body);
	} catch (e) {
		// The page was still navigating (a redirect after login): once more.
		if (!String(e).includes('Execution context was destroyed')) throw e;
		await page.waitForLoadState('networkidle');
		return papiOnce(page, method, path, body);
	}
}
async function papiOnce(page: Page, method: string, path: string, body?: unknown) {
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

const uuid = () => crypto.randomUUID();
/** The database playwright.config.ts resets for this run. */
const E2E_DB = process.env.OX_E2E_DB ?? 'oxledger_fe_e2e';
const ref = (key: string) => ({ type: 'issue', key });
async function lockOf(key: string): Promise<number> {
	return (await agent.call('get', { key, fields: ['lock_version'] })).lock_version;
}
async function propose(title: string, ops: unknown[], keys: string[], extra: Record<string, unknown> = {}) {
	const preconditions = [];
	for (const k of keys) preconditions.push({ target: ref(k), lock_version: await lockOf(k) });
	return agent.call('changeset_propose', { operation_id: uuid(), ...extra, proposal: { command_schema_version: 1, title, ops, preconditions } });
}

const contexts: BrowserContext[] = [];
// D11: a failing test carries what the backend logged while it ran, and /metrics.
let logFrom = 0;
test.beforeEach(() => {
	logFrom = existsSync(BACKEND_LOG) ? statSync(BACKEND_LOG).size : 0;
});
test.afterEach(async ({}, testInfo) => {
	for (const c of contexts.splice(0)) await c.close();
	if (testInfo.status === testInfo.expectedStatus) return;
	if (existsSync(BACKEND_LOG)) {
		const all = readFileSync(BACKEND_LOG);
		const part = all.subarray(Math.max(logFrom, all.length - 512 * 1024)).toString('utf8');
		await testInfo.attach('backend-log', { body: part, contentType: 'text/plain' });
	}
	const m = await fetch(`${BACKEND_URL}/metrics`, { headers: { Authorization: `Bearer ${METRICS_TOKEN}` } })
		.then((r) => r.text())
		.catch((e) => `metrics unavailable: ${e}`);
	await testInfo.attach('metrics', { body: m, contentType: 'text/plain' });
});
async function alice(browser: Browser, next = '/') {
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const page = await ctx.newPage();
	await login(page, 'alice', next);
	return page;
}

test('AI 團隊：精靈建立自訂角色（新執行環境、AUTH 全部需要核准），token 只顯示一次', async ({ browser }) => {
	// The old 02 pages now lead here (`/agent-contexts*` goes away in M2.0).
	const page = await alice(browser, '/ai-team');
	await expect(page.getByTestId('team-empty')).toContainText('還沒有 AI 團隊成員。');
	await page.goto('/agents/new');
	await page.waitForURL('**/ai-team/new');
	// §6.18: the wizard opens after a step-up (signing in just now counts as one).
	await maybeStepUp(page);
	await page.getByTestId('tpl-custom').click();
	await page.getByTestId('next').click();
	await page.getByTestId('role-key').fill('e2e');
	await page.getByTestId('role-duties').fill('E2E 測試用的角色');
	await page.getByTestId('client').first().check();
	await page.getByTestId('next').click();
	await page.getByTestId('rt-new').check();
	await page.getByTestId('rt-label').fill('E2E 執行環境');
	await expect(page.getByTestId('next')).toBeDisabled();
	await page.getByTestId('rt-ack').check();
	await page.getByTestId('next').click();
	await page.getByTestId('project-AUTH').check();
	// The custom template allows nothing: next stays off until something is allowed.
	await expect(page.getByTestId('next')).toBeDisabled();
	await page.getByTestId('all-review').click();
	await page.getByTestId('next').click();
	await page.getByTestId('read-time').check();
	await expect(page.getByTestId('time-warning')).toContainText('這個 agent 可以讀你自己的工時紀錄');
	await expect(page.getByTestId('next')).toBeDisabled();
	await page.getByTestId('read-time').uncheck();
	await page.getByTestId('next').click();
	await expect(page.getByTestId('confirm-project')).toContainText('AUTH');
	await expect(page.getByTestId('confirm-project')).toContainText(/直接寫入 —；需要核准 .*建立 issue/);
	// §6.18 ⑥: the final full table, read-only.
	const full = page.locator('[data-testid=confirm-table][data-project=AUTH]');
	await expect(full).toBeVisible();
	await expect(full.getByTestId('policy-table').locator('input[type=radio]:checked').first()).toBeDisabled();
	await page.getByTestId('create-role').click();
	if (await page.getByTestId('step-up').isVisible().catch(() => false)) {
		await page.getByTestId('step-up-password').fill(PASSWORD);
		await page.getByTestId('step-up-confirm').click();
	}
	await expect(page.getByTestId('wizard-done')).toBeVisible();
	await expect(page.getByTestId('wizard-done')).toContainText('e2e@alice');
	token = (await page.getByTestId('token-value').textContent())!.trim();
	expect(token).toMatch(/^oxa_/);
	agent = new Agent(token, MCP_URL);
	// The token works for MCP right away.
	const got = await agent.call('get', { key: 'AUTH-1', fields: ['subject'] });
	expect(got.subject).toBeTruthy();
	// Leaving and coming back never shows it again; the role page lists the token (G14), never its value.
	await page.goto('/ai-team');
	await expect(page.getByTestId('team-row').filter({ hasText: 'e2e@alice' })).toBeVisible();
	await page.getByRole('link', { name: 'e2e@alice' }).click();
	await expect(page.getByTestId('role-handle')).toHaveText('e2e@alice');
	await expect(page.getByTestId('token-value')).toHaveCount(0);
	await expect(page.getByTestId('cred')).toHaveCount(1);
	await expect(page.getByTestId('cred')).toContainText('有效');
	await expect(page.getByTestId('safety-row')).toContainText('AUTH');
	// §6.18 ②: direct / review in two columns (everything needs review here).
	await expect(page.getByTestId('safety-direct')).toHaveText('—');
	await expect(page.getByTestId('safety-review')).toContainText('建立 issue');
	await expect(page.getByTestId('runtime')).toContainText('E2E 執行環境');
	// The project frame lists the roles delegated there.
	await page.goto('/project/AUTH/ai-team');
	await expect(page.getByTestId('team-row').filter({ hasText: 'e2e@alice' })).toBeVisible();
});

test('用法 1：agent 提案建單，人開連結核准並套用', async ({ browser }) => {
	const page = await alice(browser);
	const projects = await papi(page, 'GET', '/projects');
	const auth = projects.json.data.find((p: { key: string }) => p.key === 'AUTH');
	const trackers = await papi(page, 'POST', '/resolve', { text: '', kind: 'tracker' });
	const bug = trackers.json.data.candidates.find((c: { name: string }) => /bug/i.test(c.name)) ?? trackers.json.data.candidates[0];
	const subject = `Safari 登入後回到登入頁 ${Date.now()}`;
	const p = await agent.call('changeset_propose', {
		operation_id: uuid(),
		proposal: {
			command_schema_version: 1,
			title: '回報 Safari 登入問題',
			ops: [{ kind: 'create_issue', op_id: uuid(), bind: 'new1', project_id: auth.id, tracker_id: bug.id, fields: { subject, description: 'Safari 17 重現。' } }],
			preconditions: []
		}
	});
	expect(p.changeset_id).toBeTruthy();
	await page.goto(`/cs/${p.changeset_id}`);
	await expect(page.getByTestId('cs-title')).toContainText('回報 Safari 登入問題');
	await expect(page.getByTestId('op')).toContainText(subject);
	await page.getByTestId('approve').click();
	const result = page.getByTestId('exec-result');
	await expect(result).toContainText('已套用');
	await result.getByRole('link', { name: /開啟 AUTH-/ }).click();
	await expect(page.getByTestId('issue-subject')).toHaveText(subject);
});

test('用法 3：三份分流提案，在佇列一次核准並套用', async ({ browser }) => {
	const keys = ['AUTH-2', 'AUTH-3', 'AUTH-6'];
	const ids: string[] = [];
	let group: string | undefined;
	for (const [i, k] of keys.entries()) {
		const extra: Record<string, unknown> = group ? { group_id: group } : { group_label: '本週分流 E2E' };
		const p = await propose(`${k} 到期日`, [{ kind: 'update_fields', op_id: uuid(), target: ref(k), set: { due_date: `2026-12-1${i}` } }], [k], extra);
		group ??= p.group_id;
		ids.push(p.changeset_id);
	}
	const page = await alice(browser, '/review');
	for (const id of ids) await page.locator(`[data-testid=cs-row][data-id="${id}"] [data-testid=tick]`).check();
	await expect(page.getByTestId('apply-picked')).toContainText('3 份');
	await page.getByTestId('apply-picked').click();
	for (const id of ids) await expect(page.locator(`[data-testid=cs-row][data-id="${id}"] [data-testid=row-result]`)).toContainText('已套用');
	for (const [i, k] of keys.entries()) {
		const got = await agent.call('get', { key: k, fields: ['subject', 'lock_version'] });
		expect(got.lock_version).toBeGreaterThan(0);
		const issue = await papi(page, 'GET', `/issues/${k}`);
		expect(issue.json.data.due_date).toBe(`2026-12-1${i}`);
	}
});

test('用法 8 與 2：補 note、標重複，各自開連結核准', async ({ browser }) => {
	const note = await propose('補充：需要 Safari 版本', [{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-1'), body: '請補上 Safari 版本號。' }], ['AUTH-1']);
	const dup = await propose('AUTH-3 是 AUTH-1 的重複', [{ kind: 'duplicate_of', op_id: uuid(), source: ref('AUTH-3'), target: ref('AUTH-1') }], ['AUTH-3', 'AUTH-1']);
	const page = await alice(browser, `/cs/${note.changeset_id}`);
	await expect(page.getByTestId('note-body')).toContainText('Safari 版本號');
	await page.getByTestId('approve').click();
	await expect(page.getByTestId('exec-result')).toContainText('已套用');
	// The note bumped AUTH-1's lock_version, so the duplicate proposal is now
	// outdated (K8): it opens with a conflict to revise, not a silent apply.
	await page.goto(`/cs/${dup.changeset_id}`);
	const panel = page.getByTestId('conflict-panel');
	await expect(panel).toBeVisible();
	await page.getByTestId('revise-apply').click();
	await expect(page.getByTestId('exec-result')).toContainText('已套用');
	await page.goto('/issue/AUTH-1');
	await expect(page.getByTestId('note').last()).toContainText('Safari 版本號');
	await page.goto('/issue/AUTH-3');
	await expect(page.getByTestId('event').filter({ hasText: '標為 AUTH-1 的重複' })).toHaveCount(1);
});

test('用法 7：人先改了同一個欄位，提案頁三欄選擇後修訂並套用', async ({ browser }) => {
	const p = await propose('AUTH-4 改標題', [{ kind: 'update_fields', op_id: uuid(), target: ref('AUTH-4'), set: { subject: 'agent 的標題' } }], ['AUTH-4']);
	const page = await alice(browser, '/issue/AUTH-4');
	// The human edits the same field directly first.
	await page.getByTestId('edit').click();
	await page.getByLabel(/^標題/).fill('人改的標題');
	await page.getByTestId('save').click();
	await expect(page.getByTestId('issue-subject')).toHaveText('人改的標題');
	await page.goto(`/cs/${p.changeset_id}`);
	await expect(page.getByTestId('conflict-panel')).toBeVisible();
	// G13: the server recorded the value when the agent proposed; the row is marked changed.
	await expect(page.getByTestId('original').first()).not.toHaveText('（提案時沒有記錄）');
	await expect(page.getByTestId('original').first()).not.toHaveText('人改的標題');
	await expect(page.locator('[data-testid=conflict-panel] tr.changed')).toHaveCount(1);
	await page.getByTestId('use-proposed').first().check();
	await expect(page.getByTestId('preview-status')).toContainText(/通過/);
	await page.getByTestId('revise-apply').click();
	await expect(page.getByTestId('exec-result')).toContainText('已套用');
	await page.goto('/issue/AUTH-4');
	await expect(page.getByTestId('issue-subject')).toHaveText('agent 的標題');
});

test('暫停：人暫停後 agent 寫入被擋，恢復後可以再提案', async ({ browser }) => {
	const page = await alice(browser, '/ai-team/e2e%40alice');
	await page.getByTestId('pause').click();
	await expect(page.getByTestId('pause-dialog')).toContainText('暫停 e2e@alice？它在所有專案的寫入會被拒絕');
	await page.getByTestId('pause-reason').fill('E2E 暫停');
	await page.getByTestId('pause-confirm').click();
	await expect(page.getByTestId('role-paused')).toContainText('E2E 暫停');
	await expect(page.getByTestId('role-status')).toHaveText('已暫停');
	// The nav says one of my roles needs me.
	await expect(page.getByTestId('ai-attention')).toBeVisible();
	await expect(propose('暫停中', [{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-2'), body: 'x' }], ['AUTH-2'])).rejects.toThrow(/agent_paused|agent_suspended/);
	await page.getByTestId('resume').click();
	await expect(page.getByTestId('role-paused')).toHaveCount(0);
	await expect(page.getByTestId('ai-attention')).toHaveCount(0);
	const ok = await propose('恢復後', [{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-2'), body: '恢復後的提案' }], ['AUTH-2']);
	expect(ok.changeset_id).toBeTruthy();
});

test('待審與委派：工時提案的摘要可讀；舊委派提示「有 N 個新操作可以加入」並可加入', async ({ browser }) => {
	// §6.14: a time proposal reads as a sentence, never as JSON.
	// G47: the backend only turns `issue.key` into an id for target/source/duplicate_of, so a
	// time proposal sent by key has no target and no summary row. Send it by id until that is fixed.
	const page = await alice(browser, '/');
	// The human API has no issue ids, so read it from the E2E database.
	const issueId = sqlValue("SELECT issue_id FROM issue_keys WHERE key = 'AUTH-2' AND is_current;");
	expect(issueId).toMatch(/^[0-9a-f-]{36}$/);
	const r = await propose('記工時', [{ kind: 'log_time', op_id: uuid(), issue: { type: 'issue', id: issueId }, fields: { hours: '1.5', spent_on: '2026-11-12', comments: '修 SSO' } }], []);
	expect(r.changeset_id ?? r.id).toBeTruthy();
	await page.goto('/review');
	const sum = page.locator('[data-testid=diff-summary][data-field=time]').first();
	await expect(sum).toContainText('記工時 1.50 小時（2026-11-12');
	await expect(sum).toContainText('說明：修 SSO');
	await expect(page.locator('main')).not.toContainText('"hours"');

	// §6.14: a delegation made with only some operations offers the newer ones.
	const role = (await papi(page, 'GET', '/ai-team?handle=e2e%40alice')).json.data.items[0];
	const web = ((await papi(page, 'GET', '/projects')).json.data as { key: string; id: string }[]).find((p) => p.key === 'WEB')!;
	const made = await papi(page, 'POST', '/delegations', {
		agent_principal_id: role.id,
		project_id: web.id,
		proposal_ops: ['update_fields'],
		writable_fields: [],
		expires_in_days: 7,
		policies: { commands: { update_fields: 'review' }, fields: {} }
	});
	expect(made.status).toBeLessThan(300);
	await page.goto('/ai-team/e2e%40alice');
	const box = page.locator('[data-testid=policy][data-project=WEB]').getByTestId('new-ops');
	await expect(box).toContainText('個新操作可以加入');
	await expect(box.locator('[data-testid=new-op][data-op=log_time]')).toContainText('預設需要核准');
	for (const op of ['create_version', 'update_version']) {
		const cb = box.locator(`[data-testid=new-op][data-op=${op}] input[type=checkbox]`);
		if (await cb.count()) await cb.uncheck();
	}
	await box.getByTestId('new-ops-add').click();
	await maybeStepUp(page);
	const row = page.locator('[data-testid=policy][data-project=WEB] [data-testid=policy-row][data-op=log_time]');
	await expect(row).toBeVisible();
	await expect(row.getByLabel('記工時：需要核准')).toBeChecked();
});

test('回復：活動頁把 agent 套用的到期日改回去', async ({ browser }) => {
	const page = await alice(browser, '/ai-team/e2e%40alice');
	await expect(page.getByTestId('role-activity-list').getByTestId('activity').first()).toBeVisible();
	await page.getByTestId('role-activity').click();
	await expect(page.getByTestId('activity').first()).toBeVisible();
	// A full key is filtered by the server (G24), so it finds rows past the first page too.
	const asked = page.waitForRequest((r) => r.url().includes('/activity') && new URL(r.url()).searchParams.get('issue') === 'AUTH-6');
	await page.getByLabel('篩選單號').fill('auth-6');
	await asked;
	await expect(page.getByTestId('activity').first()).toContainText('AUTH-6');
	const row = page.getByTestId('activity').filter({ hasText: 'AUTH-6' }).first();
	await row.getByTestId('undo').click();
	await expect(page.getByTestId('undo-preview')).toBeVisible();
	await page.getByTestId('undo-confirm').click();
	await expect.poll(async () => (await papi(page, 'GET', '/issues/AUTH-6')).json.data.due_date).toBeNull();
});

test('人直接回報與編輯：/new 建單、議題頁改欄位、加 note', async ({ browser }) => {
	const page = await alice(browser, '/new');
	await page.getByTestId('new-project').selectOption('AUTH');
	const subject = `人回報的單 ${Date.now()}`;
	await page.getByTestId('new-form').getByLabel(/^標題/).fill(subject);
	await page.getByTestId('new-submit').click();
	await page.waitForURL(/\/issue\/AUTH-\d+/);
	await expect(page.getByTestId('issue-subject')).toHaveText(subject);
	await page.getByTestId('add-note').click();
	await page.getByTestId('note-body').fill('人自己補的 note');
	await page.getByTestId('note-submit').click();
	await expect(page.getByTestId('note').last()).toContainText('人自己補的 note');
});

test('M2.0 Markdown 編輯器：/new 的說明、純文字說明改寫後轉 Markdown、note 的提及清單／預覽／字數上限／草稿', async ({ browser }) => {
	const fmt = (k: string) => sqlValue(`SELECT i.description_format FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current WHERE k.key = '${k}';`);
	const page = await alice(browser, '/new');
	await page.getByTestId('new-project').selectOption('AUTH');
	const subject = `Markdown 單 ${Date.now()}`;
	await page.getByTestId('new-form').getByLabel(/^標題/).fill(subject);
	// The toolbar wraps the selection (06 §6.2).
	const desc = page.getByTestId('md-description-input');
	await desc.fill('重現步驟');
	await desc.selectText();
	await page.getByTestId('md-description').getByRole('button', { name: '粗體（Ctrl+B）' }).click();
	await expect(desc).toHaveValue('**重現步驟**');
	await page.getByTestId('md-description-help').click();
	await expect(page.getByTestId('md-description-cheat')).toContainText('語法小抄');
	await page.getByTestId('new-submit').click();
	await page.waitForURL(/\/issue\/AUTH-\d+/);
	const k = page.url().split('/issue/')[1];
	// A new description is Markdown (R-M9: create omits `format`).
	expect(fmt(k)).toBe('markdown');
	await expect(page.getByTestId('description')).toContainText('重現步驟');

	// A description written before M2 stays plain until a person rewrites it.
	sql(`UPDATE issues SET description_format = 'plain' WHERE id = (SELECT issue_id FROM issue_keys WHERE key = '${k}' AND is_current);`);
	await page.reload();
	await page.getByTestId('edit').click();
	await expect(page.getByTestId('md-description-plain')).toContainText('原本是純文字');
	await page.getByLabel(/^標題/).fill(`${subject} 改`);
	await page.getByTestId('save').click();
	await expect(page.getByTestId('issue-subject')).toHaveText(`${subject} 改`);
	expect(fmt(k)).toBe('plain');
	await page.getByTestId('edit').click();
	await page.getByTestId('md-description-input').fill('改寫成 **Markdown**');
	await page.getByTestId('save').click();
	await expect(page.getByTestId('description')).toContainText('改寫成');
	await expect.poll(() => fmt(k)).toBe('markdown');

	// Note: the mention list comes from the project's assignable people. Until the
	// server sends logins (G48) they show but cannot be picked.
	await page.getByTestId('add-note').click();
	const body = page.getByTestId('note-body');
	await body.fill('請 @Bo');
	await body.press('End');
	await body.pressSequentially('b');
	const ac = page.getByTestId('note-md-ac');
	await expect(ac).toBeVisible();
	await expect(ac.getByTestId('note-md-ac-item').first()).toContainText('Bob');
	await body.press('Escape');
	await expect(ac).toHaveCount(0);
	// The preview needs the server's /markdown/preview (M2.1); without it the text is kept.
	await body.fill('預覽 **這段**');
	await page.getByTestId('note-md-tab-preview').click();
	await expect(page.getByTestId('note-md-preview')).toBeVisible();
	await expect(page.getByTestId('note-md-preview-failed').or(page.getByTestId('note-md-preview').locator('strong'))).toBeVisible({ timeout: 10_000 });
	await page.getByTestId('note-md-tab-write').click();
	await expect(body).toHaveValue('預覽 **這段**');
	// Over 64 KiB: the count turns red and the button is disabled.
	await body.fill('中'.repeat(22_000));
	await expect(page.getByTestId('note-md-count')).toContainText('超過上限');
	await expect(page.getByTestId('note-submit')).toBeDisabled();
	// A draft survives a reload and comes back on request.
	await body.fill('還沒送出的 note');
	await page.waitForTimeout(600);
	page.once('dialog', (d) => d.accept());
	await page.reload();
	await page.getByTestId('add-note').click();
	await expect(page.getByTestId('note-md-draft')).toBeVisible();
	await page.getByTestId('note-md-draft-restore').click();
	await expect(page.getByTestId('note-body')).toHaveValue('還沒送出的 note');
	await page.getByTestId('note-submit').click();
	await expect(page.getByTestId('note').last()).toContainText('還沒送出的 note');
	// Sent: no draft left.
	await page.reload();
	await page.getByTestId('add-note').click();
	await expect(page.getByTestId('note-md-input').or(page.getByTestId('note-body'))).toBeVisible();
	await expect(page.getByTestId('note-md-draft')).toHaveCount(0);
});

test('帳號：另一個瀏覽器被登出後要重新登入', async ({ browser }) => {
	const a = await alice(browser);
	const b = await alice(browser, '/account');
	await expect.poll(() => b.getByTestId('session-row').count()).toBeGreaterThanOrEqual(2);
	b.on('dialog', (d) => d.accept());
	await b.getByRole('button', { name: /登出其他/ }).click();
	await expect(b.getByTestId('session-row')).toHaveCount(1);
	const r = await papi(a, 'GET', '/projects');
	expect(r.status).toBe(401);
});

test('不存在的網址顯示找不到頁面', async ({ browser }) => {
	const page = await alice(browser, '/');
	await page.goto('/no/such/page');
	await expect(page.getByTestId('error-page')).toContainText('找不到這個頁面');
	// Signing in with `next` pointing nowhere ends on the same page, not on /login.
	const other = await alice(browser, '/no/such/page');
	await expect(other.getByTestId('error-page')).toContainText('找不到這個頁面');
});

/** SQL on the E2E database only (GitHub data cannot come from GitHub here). */
function sql(text: string) {
	execFileSync('docker', ['exec', '-i', 'oxledger-pg', 'psql', '-v', 'ON_ERROR_STOP=1', '-q', '-U', 'oxledger', '-d', E2E_DB], { input: text });
}

test('里程碑：改日期前先看影響，儲存後更新', async ({ browser }) => {
	const page = await alice(browser, '/project/AUTH/milestones');
	await page.getByTestId('new-milestone').click();
	await page.locator('#ms-name').fill('E2E 里程碑');
	await page.locator('#ms-date').fill('2026-12-20');
	await page.getByTestId('ms-create').click();
	const card = page.getByTestId('milestone').filter({ hasText: 'E2E 里程碑' });
	await expect(card).toContainText('2026-12-20');
	// AUTH-2 (due 2026-12-10) goes into it.
	const versions = await papi(page, 'GET', '/projects/AUTH/versions');
	const v = versions.json.data.versions.find((x: { name: string }) => x.name === 'E2E 里程碑');
	const iss = await papi(page, 'GET', '/issues/AUTH-2');
	const r = await papi(page, 'POST', '/commands', {
		command_schema_version: 1,
		ops: [{ kind: 'update_fields', op_id: uuid(), target: ref('AUTH-2'), set: { fixed_version_id: v.id } }],
		preconditions: [{ target: ref('AUTH-2'), lock_version: iss.json.data.lock_version }]
	});
	expect(r.status).toBe(200);
	await page.reload();
	await card.getByTestId('ms-edit').click();
	await page.locator('[data-testid=milestone-edit] input[type=date]').fill('2026-12-05');
	await expect(page.getByTestId('impact')).toContainText('1 張的到期日會晚於新日期');
	await page.getByTestId('ms-save').click();
	await expect(card).toContainText('2026-12-05');
	// The timeline draws AUTH-2 in that milestone's lane.
	await page.goto('/project/AUTH/timeline');
	await expect(page.getByTestId('lane').filter({ hasText: 'E2E 里程碑' })).toContainText('AUTH-2');
});

test('權限：bob 看不到里程碑與隱藏 PR 的操作，alice 看得到', async ({ browser }) => {
	sql(`
WITH i AS (INSERT INTO github_installations (gh_installation_id, account_login, state) VALUES (9001,'acme','active') RETURNING id)
INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state)
SELECT 101, i.id, 'acme/auth-service', false, true, 'accessible' FROM i;
INSERT INTO project_repo_links (project_id, repo_id, linked_by, sync_state, synced_through)
SELECT p.id, r.id, u.id, 'ok', now() FROM projects p, github_repos r, principals u WHERE p.key='AUTH' AND r.full_name='acme/auth-service' AND u.login='alice';
WITH p AS (INSERT INTO github_pull_requests (repo_id, number, title, head_ref, head_sha, state, review_state, ci_state, html_url, gh_updated_at, content_hash)
  SELECT r.id, 12, 'AUTH-1 fix', 'feat/12', 'abc', 'merged', 'approved', 'success', 'https://github.com/acme/auth-service/pull/12', now(), '\\x00'
  FROM github_repos r WHERE r.full_name='acme/auth-service' RETURNING id)
INSERT INTO pr_issue_refs (pr_id, issue_id, project_id, link_id, sources)
SELECT p.id, k.issue_id, k.project_id, l.id, '{title}' FROM p, issue_keys k, project_repo_links l
WHERE k.key='AUTH-1' AND l.project_id=k.project_id AND l.unlinked_at IS NULL;
UPDATE github_repos SET default_branch='main' WHERE full_name='acme/auth-service';
INSERT INTO github_commits (repo_id, sha, message, author_login, sender_login, committed_at, on_default_branch, first_ref)
SELECT r.id, repeat('c', 40), E'AUTH-1 修正登入逾時\n\n細節不顯示', 'mallory', 'alice', now(), true, 'main'
FROM github_repos r WHERE r.full_name='acme/auth-service';
INSERT INTO commit_issue_refs (repo_id, sha, issue_id, project_id, link_id)
SELECT l.repo_id, repeat('c', 40), k.issue_id, k.project_id, l.id FROM issue_keys k, project_repo_links l
WHERE k.key='AUTH-1' AND l.project_id=k.project_id AND l.unlinked_at IS NULL;
`);
	const bob = await (await browser.newContext()).newPage();
	contexts.push(bob.context());
	await login(bob, 'bob', '/project/AUTH/milestones');
	await expect(bob.getByText(/組織時區/)).toBeVisible();
	await expect(bob.getByTestId('new-milestone')).toHaveCount(0);
	await expect(bob.getByTestId('ms-edit')).toHaveCount(0);
	await bob.goto('/project/AUTH/dev');
	await expect(bob.getByTestId('dev-issue').first()).toContainText('acme/auth-service#12');
	await expect(bob.getByRole('button', { name: '隱藏' })).toHaveCount(0);
	const page = await alice(browser, '/project/AUTH/dev');
	const row = page.getByTestId('dev-issue').filter({ hasText: 'AUTH-1' });
	await expect(row.getByTestId('merged-transition')).toBeVisible();
	await row.getByRole('button', { name: '隱藏' }).click();
	await expect(page.getByTestId('dev-issue')).toHaveCount(0);
	// M2.5 (design 06 §6.8): the issue's card lists the commit; only who
	// manages the project can hide it there.
	await bob.goto('/issue/AUTH-1');
	const seen = bob.getByTestId('dev-commit');
	await expect(seen).toHaveCount(1);
	await expect(seen).toContainText('ccccccc');
	await expect(seen).toContainText('AUTH-1 修正登入逾時');
	await expect(seen).not.toContainText('細節不顯示');
	await expect(seen.getByTestId('dev-commit-author')).toContainText('mallory（GitHub 未驗證）');
	await expect(seen.getByTestId('dev-commit-default')).toContainText('已在 main');
	await expect(seen.locator('a')).toHaveAttribute('href', `https://github.com/acme/auth-service/commit/${'c'.repeat(40)}`);
	await expect(bob.getByTestId('dev-commit-hide')).toHaveCount(0);
	await page.goto('/issue/AUTH-1');
	await page.getByTestId('dev-commit-hide').click();
	await expect(page.getByTestId('dev-commit')).toContainText('已隱藏');
	await expect(page.getByTestId('dev-commit-hide')).toHaveText('取消隱藏');
	await bob.reload();
	await expect(bob.getByText(/組織時區|AUTH-1/).first()).toBeVisible();
	await expect(bob.getByTestId('dev-commit')).toHaveCount(0);
	await page.getByTestId('dev-commit-hide').click();
	await expect(page.getByTestId('dev-commit')).not.toContainText('已隱藏');
});

test('登入的 next 只接受本站路徑', async ({ browser }) => {
	for (const [next, want] of [
		['//evil.example/x', '/'],
		// Normalises to the path `//evil.example/x` (a protocol-relative URL).
		['/.//evil.example/x', '/'],
		['/a/..//evil.example/x', '/'],
		['/%09/evil.example', '/'],
		['/\\evil.example', '/'],
		['https://evil.example', '/'],
		['/search?q=login', '/search']
	] as const) {
		const page = await (await browser.newContext()).newPage();
		contexts.push(page.context());
		await page.goto(`/login?next=${encodeURIComponent(next).replace('%2509', '%09')}`);
		await page.fill('input[name=username]', 'alice');
		await page.fill('input[name=password]', PASSWORD);
		await page.click('button[type=submit]');
		await page.waitForURL((u) => !u.pathname.startsWith('/login'));
		await page.waitForTimeout(300);
		const u = new URL(page.url());
		expect(u.origin, next).toBe(new URL(page.url()).origin);
		expect(u.host, next).toMatch(/^localhost:/);
		expect(u.pathname, next).toBe(want);
	}
});

function sqlValue(text: string): string {
	return execFileSync('docker', ['exec', '-i', 'oxledger-pg', 'psql', '-q', '-t', '-A', '-U', 'oxledger', '-d', E2E_DB], { input: text }).toString().trim();
}

test('議題頁衝突：載入最新後只送我改的欄位，不蓋掉別人的修改', async ({ browser }) => {
	const page = await alice(browser, '/issue/AUTH-4');
	await page.getByTestId('edit').click();
	const subject = `alice 改的標題 ${Date.now()}`;
	await page.getByLabel(/^標題/).fill(subject);
	// Someone else changes the due date meanwhile.
	const lock = (await papi(page, 'GET', '/issues/AUTH-4')).json.data.lock_version;
	const other = await papi(page, 'POST', '/commands', {
		command_schema_version: 1,
		ops: [{ kind: 'update_fields', op_id: uuid(), target: ref('AUTH-4'), set: { due_date: '2027-01-31' } }],
		preconditions: [{ target: ref('AUTH-4'), lock_version: lock }]
	});
	expect(other.status).toBe(200);
	await page.getByTestId('save').click();
	await expect(page.getByTestId('conflict')).toBeVisible();
	await page.getByTestId('conflict').getByRole('button').click();
	await expect(page.getByTestId('kept')).toContainText('標題');
	await expect(page.getByLabel(/^標題/)).toHaveValue(subject);
	await page.getByTestId('save').click();
	await expect(page.getByTestId('issue-subject')).toHaveText(subject);
	const now = (await papi(page, 'GET', '/issues/AUTH-4')).json.data;
	expect(now.due_date).toBe('2027-01-31');
	expect(now.subject).toBe(subject);
});

test('/new：回應遺失時鎖住表單，重試用同一筆，不會建兩張', async ({ browser }) => {
	const page = await alice(browser, '/new');
	await page.getByTestId('new-project').selectOption('AUTH');
	await page.waitForLoadState('networkidle'); // the form is rebuilt for the chosen project
	const subject = `回應遺失 ${Date.now()}`;
	await page.getByTestId('new-form').getByLabel(/^標題/).fill(subject);
	await expect(page.getByTestId('new-submit')).toBeEnabled();
	let dropped = false;
	await page.route('**/api/v1/commands', async (route) => {
		if (dropped) return route.continue();
		dropped = true;
		await route.fetch(); // reaches the server…
		await route.abort('connectionreset'); // …but the answer is lost
	});
	await page.getByTestId('new-submit').click();
	await expect(page.getByTestId('new-uncertain')).toBeVisible();
	await expect(page.getByTestId('new-submit')).toBeDisabled();
	await expect(page.getByTestId('new-form').getByLabel(/^標題/)).toBeDisabled();
	await page.getByTestId('error').getByRole('button', { name: '重試' }).click();
	await page.waitForURL(/\/issue\/AUTH-\d+/);
	expect(sqlValue(`SELECT count(*) FROM issues WHERE subject = '${subject}'`)).toBe('1');
});

test('CSRF：另一個分頁登出再登入後，原頁送出 note 自動換新 token', async ({ browser }) => {
	const page = await alice(browser, '/issue/AUTH-3');
	await page.getByTestId('add-note').click();
	await page.getByTestId('note-body').fill('換 token 後送出的 note');
	const other = await page.context().newPage();
	await other.goto('/account');
	await papi(other, 'POST', '/auth/logout');
	await login(other, 'alice', '/account');
	await page.getByTestId('note-submit').click();
	await expect(page.getByTestId('note').last()).toContainText('換 token 後送出的 note');
	await expect(page.getByTestId('error')).toHaveCount(0);
});

test('佇列：同一張單的兩份提案不能同時勾；出錯後剩下的不送出', async ({ browser }) => {
	const a = await propose('AUTH-1 互斥甲', [{ kind: 'update_fields', op_id: uuid(), target: ref('AUTH-1'), set: { due_date: '2027-02-01' } }], ['AUTH-1']);
	const b = await propose('AUTH-1 互斥乙', [{ kind: 'update_fields', op_id: uuid(), target: ref('AUTH-1'), set: { due_date: '2027-02-02' } }], ['AUTH-1']);
	const page = await alice(browser, '/review');
	const tick = (id: string) => page.locator(`[data-testid=cs-row][data-id="${id}"] [data-testid=tick]`);
	await tick(a.changeset_id).check();
	await expect(tick(b.changeset_id)).toBeDisabled();
	await tick(a.changeset_id).uncheck();
	await expect(tick(b.changeset_id)).toBeEnabled();

	// Five on different issues; the first answer is a server error, which
	// stops new sends: the fifth is never sent.
	const ids: string[] = [];
	for (const k of ['AUTH-1', 'AUTH-2', 'AUTH-3', 'AUTH-4', 'AUTH-6']) {
		const p = await propose(`${k} 停止測試`, [{ kind: 'update_fields', op_id: uuid(), target: ref(k), set: { due_date: '2027-03-01' } }], [k]);
		ids.push(p.changeset_id);
	}
	await page.reload();
	let n = 0;
	await page.route('**/approve-and-apply', async (route) => {
		if (n++ === 0) return route.fulfill({ status: 401, contentType: 'application/json', body: JSON.stringify({ error: { code: 'unauthenticated', message: 'x', next_actions: [], request_id: 'r', retry_after_ms: null } }) });
		await new Promise((r) => setTimeout(r, 1500));
		return route.abort('connectionreset');
	});
	for (const id of ids) await tick(id).check();
	await page.getByTestId('apply-picked').click();
	// Which row met the error depends on arrival order; at most four run at
	// once, so the fifth is never started.
	const result = (id: string) => page.locator(`[data-testid=cs-row][data-id="${id}"] [data-testid=row-result]`);
	await expect.poll(async () => (await Promise.all(ids.map((id) => result(id).textContent()))).filter((t) => t?.includes('未送出')).length).toBe(1);
	const texts = await Promise.all(ids.map((id) => result(id).textContent()));
	const unsent = ids[texts.findIndex((t) => t?.includes('未送出'))];
	expect(sqlValue(`SELECT count(*) FROM changesets WHERE id = '${unsent}' AND state = 'proposed'`)).toBe('1');
});

// ---- M1 views and boards (contract 05 §6.1–§6.3)

test('視圖：專案議題加「指派＝我」，另存成個人視圖，我的視圖列出', async ({ browser }) => {
	const page = await alice(browser, '/project/AUTH/issues');
	await expect(page.getByTestId('view-scope')).toHaveText('內建');
	await expect(page.getByTestId('view-table').or(page.getByTestId('view-empty'))).toBeVisible();
	await page.getByTestId('edit-conditions').click();
	await page.getByTestId('fe-add').click();
	const cond = page.getByTestId('fe-cond').last();
	await cond.getByTestId('fe-field').selectOption('assignee');
	await cond.getByTestId('fe-options').getByLabel('我').check();
	await expect(page.getByTestId('fe-preview')).toContainText(/有結果|沒有結果/);
	await page.getByTestId('fe-apply').click();
	await expect(page.getByTestId('chips')).toContainText('指派：我');
	await expect(page.getByTestId('view-dirty')).toContainText('尚未儲存');
	await page.getByTestId('view-save-as').click();
	const name = `E2E 我的待辦 ${Date.now()}`;
	await page.getByTestId('save-as-name').fill(name);
	await page.getByTestId('save-as-submit').click();
	await expect(page.getByTestId('view-name')).toHaveText(name);
	await expect(page.getByTestId('view-scope')).toHaveText('個人');
	await expect(page.getByTestId('view-dirty')).toHaveCount(0);
	await page.goto('/views');
	await expect(page.getByTestId('view-item').filter({ hasText: name })).toBeVisible();
	// The saved condition is the server's, not just the page's.
	const id = new URL((await page.getByTestId('view-item').filter({ hasText: name }).getByRole('link').getAttribute('href'))!, 'http://x').pathname.split('/').pop()!;
	const v = (await papi(page, 'GET', `/views/${id}`)).json.data;
	expect(JSON.stringify(v.def.query.where)).toContain('assignee');
});

test('搜尋：把查詢存成視圖，條件變成 chip', async ({ browser }) => {
	const page = await alice(browser, `/search?q=${encodeURIComponent('project:AUTH status:open')}`);
	await page.getByTestId('save-as-view').click();
	await page.getByTestId('sv-name').fill('E2E 未完成');
	await page.getByTestId('sv-submit').click();
	await page.waitForURL(/\/project\/AUTH\/views\//);
	await expect(page.getByTestId('chips')).toContainText('狀態分類：待處理、進行中');
	await expect(page.getByTestId('view-scope')).toHaveText('個人');
});

test('列表與看板：骨架、排序只套用這次、搜尋頁條件、條件組最多兩層、看板複製成個人並改卡片欄位與泳道', async ({ browser }) => {
	test.setTimeout(120_000);
	const page = await alice(browser, '/');
	// §6 共通: a slow list shows 8 skeleton rows (after 400 ms) and then 「還在載入…」.
	await page.route(/\/api\/v1\/views\/.*\/run/, async (r) => {
		await new Promise((x) => setTimeout(x, 2500));
		await r.continue();
	});
	await page.goto('/project/AUTH/issues');
	await expect(page.getByTestId('view-skeleton')).toBeVisible();
	await expect(page.getByTestId('view-skeleton').locator('tr')).toHaveCount(8);
	await expect(page.getByTestId('view-slow')).toContainText('還在載入');
	await page.unrouteAll({ behavior: 'ignoreErrors' });
	await expect(page.getByTestId('view-table').or(page.getByTestId('view-empty'))).toBeVisible();

	// §6.1: sorting is for now only — no unsaved-conditions banner, and it can be reset.
	await page.getByTestId('sort-due_date').click();
	await expect(page.getByTestId('view-sorted')).toBeVisible();
	await expect(page.getByTestId('view-dirty')).toHaveCount(0);
	await page.getByTestId('view-sort-reset').click();
	await expect(page.getByTestId('view-sorted')).toHaveCount(0);

	// §6.2: the condition editor shows its two-level limit in words.
	await page.getByTestId('edit-conditions').click();
	await page.getByTestId('fe-add-group').click();
	await expect(page.getByTestId('fe-add-subgroup')).toBeDisabled();
	await expect(page.getByTestId('fe-depth-limit')).toHaveText('最多兩層');

	// §6.14: the search page has the condition editor too; the conditions ride in the URL.
	await page.goto(`/search?q=${encodeURIComponent('project:AUTH')}`);
	await page.getByTestId('search-conditions').click();
	await page.getByTestId('fe-add').click();
	const cond = page.getByTestId('fe-cond').last();
	await cond.getByTestId('fe-field').selectOption('assignee');
	await cond.getByTestId('fe-options').getByLabel('我').check();
	await page.getByTestId('fe-apply').click();
	await expect(page).toHaveURL(/where=/);
	await expect(page.getByTestId('search-chips')).toContainText('指派：我');

	// §6.3: the built-in board copies to a personal one; card fields and swimlanes save with it.
	await page.goto('/project/AUTH/board');
	await page.getByTestId('board-settings').click();
	await expect(page.getByTestId('board-copy')).toBeVisible();
	await page.getByTestId('board-copy-personal').click();
	const name = `E2E 我的看板 ${Date.now()}`;
	await page.getByTestId('save-as-name').fill(name);
	await page.getByTestId('save-as-submit').click();
	await expect(page.getByTestId('view-name')).toHaveText(name);
	await expect(page.getByTestId('view-scope')).toHaveText('個人');
	await page.getByTestId('board-settings').click();
	await page.getByTestId('bs-card-assignee').uncheck();
	await page.getByTestId('bs-card-status').check();
	await page.getByTestId('bs-apply').click();
	await page.getByTestId('board-swim').selectOption({ label: '優先權' });
	await expect(page.getByTestId('view-dirty')).toBeVisible();
	await page.getByTestId('view-save').click();
	await expect(page.getByTestId('view-dirty')).toHaveCount(0);
	const id = new URL(page.url()).pathname.split('/').pop()!;
	const v = (await papi(page, 'GET', `/views/${id}`)).json.data;
	expect(v.def.card_fields).toContain('status');
	expect(v.def.card_fields).not.toContain('assignee');
	expect(v.def.swimlanes?.by).toBeTruthy();
	await expect(page.locator('[data-testid=card] [data-testid=card-assignee]')).toHaveCount(0);
	await expect(page.locator('[data-testid=card] [data-testid=card-status]').first()).toBeVisible();
});

/** The first card in a lane of the AUTH board. */
async function cardIn(page: Page, lane: string) {
	const c = page.locator(`[data-testid=lane][data-lane=${lane}] [data-testid=card]`).first();
	await expect(c).toBeVisible();
	return (await c.getAttribute('data-key'))!;
}
async function categoryOf(page: Page, key: string) {
	return (await papi(page, 'GET', `/issues/${key}/view`)).json.data;
}

test('看板：鍵盤 M 與拖拉都移到進行中，之後只重載兩欄', async ({ browser }) => {
	const page = await alice(browser, '/project/AUTH/board');
	await expect(page.getByTestId('lane')).toHaveCount(3);
	// Watch the runs sent after the board is on screen.
	const runs: string[] = [];
	page.on('request', (r) => {
		if (r.method() === 'POST' && /\/views\/[^/]+\/run$/.test(new URL(r.url()).pathname)) runs.push(r.postData() ?? '');
	});
	const key = await cardIn(page, 'todo');
	await page.locator(`[data-testid=card][data-key="${key}"]`).focus();
	await page.keyboard.press('m');
	await page.getByTestId('move-menu').getByTestId('move-to').filter({ hasText: '進行中' }).click();
	if (await page.getByTestId('pick-status').first().isVisible().catch(() => false)) await page.getByTestId('pick-status').first().click();
	await expect(page.locator(`[data-testid=lane][data-lane=doing] [data-testid=card][data-key="${key}"]`)).toBeVisible();
	expect(JSON.stringify(await categoryOf(page, key))).toContain('"doing"');
	// Only the two lanes were run again, not the whole board.
	expect(runs.length).toBe(2);
	expect(runs.every((b) => /"lane":"(todo|doing)"/.test(b))).toBe(true);
	// Another card by HTML5 drag and drop: it must really move.
	const key2 = await cardIn(page, 'todo');
	expect(key2).not.toBe(key);
	await page.locator(`[data-testid=card][data-key="${key2}"]`).dragTo(page.locator('[data-testid=lane][data-lane=doing]'));
	if (await page.getByTestId('pick-status').first().isVisible().catch(() => false)) await page.getByTestId('pick-status').first().click();
	await expect(page.locator(`[data-testid=lane][data-lane=doing] [data-testid=card][data-key="${key2}"]`)).toBeVisible();
	expect(JSON.stringify(await categoryOf(page, key2))).toContain('"doing"');
	await expect(page.getByTestId('done-window')).toContainText('14 天');
});

test('看板：別人剛改過的卡片，移動時說明並重新載入', async ({ browser }) => {
	const page = await alice(browser, '/project/AUTH/board');
	const key = await cardIn(page, 'todo');
	const lock = (await papi(page, 'GET', `/issues/${key}`)).json.data.lock_version;
	const r = await papi(page, 'POST', '/commands', {
		command_schema_version: 1,
		ops: [{ kind: 'update_fields', op_id: uuid(), target: ref(key), set: { due_date: '2027-02-01' } }],
		preconditions: [{ target: ref(key), lock_version: lock }]
	});
	expect(r.status).toBe(200);
	await page.locator(`[data-testid=card][data-key="${key}"]`).focus();
	await page.keyboard.press('m');
	await page.getByTestId('move-menu').getByTestId('move-to').filter({ hasText: '進行中' }).click();
	if (await page.getByTestId('pick-status').first().isVisible().catch(() => false)) await page.getByTestId('pick-status').first().click();
	await expect(page.locator('.toast').last()).toContainText('這張單剛被別人更新，已重新載入。');
	// The person's move did not go through on the old version.
	expect(JSON.stringify(await categoryOf(page, key))).not.toContain('"doing"');
});

test('專案設定：加「視圖」分頁並把看板分頁換成專案看板', async ({ browser }) => {
	const page = await alice(browser, '/project/AUTH/board');
	// A project board view to point at (alice manages AUTH).
	const pid = (await papi(page, 'GET', '/projects')).json.data.find((p: { key: string }) => p.key === 'AUTH').id;
	const copy = await papi(page, 'POST', `/views/${encodeURIComponent('builtin:project_board:' + pid)}/copy`, { name: 'E2E 團隊看板', scope: 'project', project_id: pid });
	expect(copy.status).toBe(201);
	await page.goto('/project/AUTH/settings');
	await page.getByTestId('add-view-tab').click();
	await expect(page.getByTestId('tab-view')).toHaveValue(copy.json.data.id);
	await page.getByTestId('tab-board-view').selectOption(copy.json.data.id);
	await page.getByRole('button', { name: '儲存分頁' }).click();
	await expect(page.locator('.toast').last()).toContainText('已儲存分頁');
	await page.getByTestId('ptabs').getByRole('link', { name: '看板' }).first().click();
	await expect(page.getByTestId('view-name')).toHaveText('E2E 團隊看板');
	await expect(page.getByTestId('view-scope')).toHaveText('專案');
});

test('視圖：儲存帶 lock_version；別人先改過時顯示衝突，載入最新後我的草稿不見', async ({ browser }) => {
	const page = await alice(browser, '/views');
	const made = await papi(page, 'POST', '/views', { scope: 'personal', kind: 'list', name: 'E2E 衝突視圖', def: { kind: 'list', query: { where: null, q: null }, columns: ['key', 'subject'], sort: [{ field: 'updated_at', dir: 'desc' }] } });
	expect(made.status).toBe(201);
	const id = made.json.data.id;
	await page.goto(`/views/${id}`);
	await expect(page.getByTestId('view-name')).toHaveText('E2E 衝突視圖');
	// My draft: a condition.
	await page.getByTestId('edit-conditions').click();
	await page.getByTestId('fe-add').click();
	await page.getByTestId('fe-cond').last().getByTestId('fe-field').selectOption('assignee');
	await page.getByTestId('fe-cond').last().getByTestId('fe-options').getByLabel('我').check();
	await page.getByTestId('fe-apply').click();
	await expect(page.getByTestId('view-dirty')).toBeVisible();
	// Someone (same person, another tab) renames it meanwhile.
	const lv = (await papi(page, 'GET', `/views/${id}`)).json.data.lock_version;
	expect((await papi(page, 'PATCH', `/views/${id}`, { lock_version: lv, name: 'E2E 別人改的名字' })).status).toBe(200);
	await page.getByTestId('view-save').click();
	await expect(page.getByTestId('view-conflict')).toContainText('最新名稱：「E2E 別人改的名字」');
	// Nothing was written: the server still has no condition.
	expect((await papi(page, 'GET', `/views/${id}`)).json.data.def.query.where).toBeNull();
	await page.getByTestId('view-conflict').getByRole('button', { name: '放棄我的修改，載入最新' }).click();
	await expect(page.getByTestId('view-name')).toHaveText('E2E 別人改的名字');
	await expect(page.getByTestId('view-dirty')).toHaveCount(0);
	// Now a plain save goes through with the new lock_version.
	await page.getByTestId('edit-conditions').click();
	await page.getByTestId('fe-add').click();
	await page.getByTestId('fe-cond').last().getByTestId('fe-field').selectOption('assignee');
	await page.getByTestId('fe-cond').last().getByTestId('fe-options').getByLabel('我').check();
	await page.getByTestId('fe-apply').click();
	await page.getByTestId('view-save').click();
	await expect(page.getByTestId('view-dirty')).toHaveCount(0);
	const saved = (await papi(page, 'GET', `/views/${id}`)).json.data;
	expect(JSON.stringify(saved.def.query.where)).toContain('assignee');
	expect(saved.lock_version).toBe(lv + 2);
});

// ---------------------------------------------------------------- F9: Triage and time (contract 05 §6.4–§6.9)

async function newIssue(page: Page, subject: string): Promise<string> {
	const projects = (await papi(page, 'GET', '/projects')).json.data as { id: string; key: string }[];
	const ff = (await papi(page, 'GET', '/filter-fields')).json.data;
	const bug = ff.fields.find((f: { field: string }) => f.field === 'tracker').options.find((o: { label: string }) => o.label === 'Bug').id;
	const r = await papi(page, 'POST', '/commands', {
		command_schema_version: 1,
		ops: [{ kind: 'create_issue', op_id: uuid(), bind: 'n', project_id: projects.find((p) => p.key === 'AUTH')!.id, tracker_id: bug, fields: { subject, description: `${subject} 的描述` } }],
		preconditions: []
	});
	expect(r.status).toBe(200);
	return r.json.data.effects[0].key;
}
async function issueView(page: Page, key: string) {
	return (await papi(page, 'GET', `/issues/${key}/view?include=recent_notes`)).json.data;
}

test('Triage：設定開啟後，收件匣用鍵盤與按鈕拒絕、接受、標重複，最近決定列出', async ({ browser }) => {
	const page = await alice(browser, '/project/AUTH/settings/triage');
	await page.getByTestId('ts-enabled').check();
	await page.getByTestId('ts-rule-all_new').check();
	await page.getByTestId('ts-responsible').selectOption({ label: 'Carol Wu' });
	await page.getByTestId('ts-decline').selectOption({ label: 'Closed' });
	await page.getByTestId('ts-save').click();
	await expect.poll(async () => (await papi(page, 'GET', '/projects/AUTH/triage-settings')).json.data.enabled).toBe(true);
	const a = await newIssue(page, 'E2E 分流 A：不是本專案的問題');
	const b = await newIssue(page, 'E2E 分流 B：登入頁 CSP');
	const c = await newIssue(page, 'E2E 分流 C：和 AUTH-1 一樣的 SSO 需求');
	expect((await papi(page, 'GET', '/triage/counts')).json.data.projects[0].count).toBeGreaterThanOrEqual(3);

	await page.goto('/project/AUTH/triage');
	await expect(page.getByTestId('triage-responsible')).toHaveText('本週負責：Carol Wu');
	await expect(page.getByTestId('triage-badge')).toBeVisible();
	await expect(page.locator(`[data-testid=triage-row][data-key=${a}]`)).toBeVisible();
	// Keyboard: move to A, 2 = decline (preset status Closed, preset note prefix).
	const keys = await page.getByTestId('triage-row').evaluateAll((rs) => rs.map((r) => (r as HTMLElement).dataset.key));
	for (let i = 0; i < keys.indexOf(a); i++) await page.keyboard.press('j');
	await page.keyboard.press('2');
	await expect(page.getByTestId('triage-panel')).toHaveAttribute('data-decision', 'decline');
	await expect(page.getByTestId('tp-status').locator('option:checked')).toHaveText('Closed');
	await expect(page.getByTestId('tp-reason-hint')).toBeVisible();
	await page.getByTestId('tp-note').fill('（Triage 拒絕）請改報到 WEB。');
	await page.getByTestId('tp-submit').click();
	await expect(page.locator(`[data-testid=triage-row][data-key=${a}]`)).toHaveCount(0);
	const va = await issueView(page, a);
	expect([va.status, va.triage]).toEqual(['Closed', 'declined']);
	expect(va.recent_notes.items[0].body).toBe('（Triage 拒絕）請改報到 WEB。');

	// Accept B: assign Bob and move to In Progress in the same document.
	await page.locator(`[data-testid=triage-row][data-key=${b}]`).getByTestId('act-accept').click();
	await page.getByTestId('tp-assignee').selectOption({ label: 'Bob Chen' });
	await page.getByTestId('tp-status').selectOption({ label: 'In Progress' });
	await page.getByTestId('tp-submit').click();
	await expect(page.locator(`[data-testid=triage-row][data-key=${b}]`)).toHaveCount(0);
	const vb = await issueView(page, b);
	expect([vb.status, vb.assignee, vb.triage]).toEqual(['In Progress', 'Bob Chen', 'accepted']);

	// Duplicate C of AUTH-1: search by key, pick, the note fills in.
	await page.locator(`[data-testid=triage-row][data-key=${c}]`).getByTestId('act-dup').click();
	await page.getByTestId('tp-dup-q').fill('AUTH-1');
	await page.getByTestId('tp-dup-hit').filter({ hasText: /^AUTH-1 / }).first().click();
	await expect(page.getByTestId('tp-note')).toHaveValue('和 AUTH-1 重複。');
	await page.getByTestId('tp-submit').click();
	await expect(page.locator(`[data-testid=triage-row][data-key=${c}]`)).toHaveCount(0);
	const vc = await issueView(page, c);
	expect([vc.status, vc.triage]).toEqual(['Closed', 'duplicate']);

	await page.getByTestId('tab-decided').click();
	for (const [k, d] of [
		[a, '拒絕'],
		[b, '接受'],
		[c, '重複']
	])
		await expect(page.locator(`[data-testid=triage-row][data-key=${k}]`).getByTestId('decision')).toHaveText(d);
	await page.getByTestId('self-only').check();
	await expect(page.getByTestId('triage-empty')).toHaveText('最近 30 天沒有 agent 分流自己建的單。');
});

test('Triage：議題頁橫條稍後與取消稍後；別人先處理時說明並移除該列', async ({ browser }) => {
	const page = await alice(browser, '/');
	const k = await newIssue(page, 'E2E 分流 D：稍後處理');
	await page.goto(`/issue/${k}`);
	await expect(page.getByTestId('triage-bar')).toContainText('這張單在 Triage 收件匣（AUTH）');
	await page.getByTestId('tb-snooze').click();
	await page.getByTestId('triage-panel').getByLabel('下週一 9 點').check();
	await page.getByTestId('tp-submit').click();
	await expect(page.getByTestId('triage-bar')).toContainText('稍後處理到');
	expect((await issueView(page, k)).triage).toBe('snoozed');
	// Snoozed issues leave the inbox unless asked for.
	await page.goto('/project/AUTH/triage');
	await expect(page.getByTestId('triage-empty').or(page.getByTestId('triage-row').first())).toBeVisible();
	await expect(page.locator(`[data-testid=triage-row][data-key=${k}]`)).toHaveCount(0);
	await page.getByTestId('show-snoozed').check();
	await expect(page.locator(`[data-testid=triage-row][data-key=${k}]`).getByTestId('snoozed-until')).toBeVisible();
	await page.goto(`/issue/${k}`);
	await page.getByTestId('tb-unsnooze').click();
	await page.getByTestId('tp-submit').click();
	await expect(page.getByTestId('tb-accept')).toBeVisible();
	expect((await issueView(page, k)).triage).toBe('pending');

	// Someone accepts it while my panel is open: said so, and the row leaves.
	await page.goto('/project/AUTH/triage');
	const row = page.locator(`[data-testid=triage-row][data-key=${k}]`);
	await row.getByTestId('act-accept').click();
	await expect(page.getByTestId('tp-submit')).toBeEnabled();
	const lock = (await papi(page, 'GET', `/issues/${k}`)).json.data.lock_version;
	const other = await papi(page, 'POST', '/commands', { command_schema_version: 1, ops: [{ kind: 'triage_decide', op_id: uuid(), target: ref(k), decision: 'accept' }], preconditions: [{ target: ref(k), lock_version: lock }] });
	expect(other.status).toBe(200);
	await page.getByTestId('tp-submit').click();
	await expect(page.getByText('這張單已經被處理了').first()).toBeVisible();
	await expect(row).toHaveCount(0);
});

test('工時：議題頁記工時、編輯時同時記；週表就地新增、修改與清空刪除', async ({ browser }) => {
	const page = await alice(browser, '/');
	page.on('dialog', (d) => d.accept());
	const k = await newIssue(page, 'E2E 工時：重現 Safari 問題');
	await page.goto(`/issue/${k}`);
	await expect(page.getByTestId('time-sum')).toContainText('已記 0 小時');
	await page.getByTestId('log-time').click();
	await page.getByTestId('lt-hours').fill('1:30');
	await expect(page.getByTestId('lt-hours-parsed')).toHaveText('＝1.50 小時');
	await page.getByTestId('lt-comments').fill('重現 Safari');
	await page.getByTestId('lt-submit').click();
	await expect(page.getByTestId('time-sum')).toContainText('已記 1.5 小時');
	await expect(page.getByTestId('time-event').first()).toContainText('Alice Lin 記了 1.5 小時・開發');
	// Edit + log in one document.
	await page.getByTestId('edit').click();
	await page.getByTestId('edit-with-time').locator('summary').click();
	// Hours it cannot read: saving stops and says so (never saves without the time).
	await page.getByTestId('edit-with-time').getByTestId('lt-hours').fill('abc');
	await page.getByTestId('save').click();
	await expect(page.getByTestId('time-hint')).toBeVisible();
	await expect(page.getByTestId('save')).toBeVisible();
	await page.getByTestId('edit-with-time').getByTestId('lt-hours').fill('0.5');
	await page.getByTestId('save').click();
	await expect(page.getByTestId('time-sum')).toContainText('已記 2 小時');
	expect((await issueView(page, k)).spent_hours).toBe('2.00');

	// Timesheet: this week has the issue's row; log a new day, change it, clear it.
	await page.goto('/time');
	const row = page.locator(`[data-testid=ts-row][data-issue=${k}]`);
	await expect(row).toBeVisible();
	const today = new Date().toLocaleDateString('sv-SE', { timeZone: 'Asia/Taipei' });
	const days: string[] = await row.getByTestId('cell').evaluateAll((cs) => cs.map((c) => (c as HTMLElement).dataset.date!));
	const other = days.find((d) => d < today) ?? days.find((d) => d !== today)!;
	const cell = () => row.locator(`[data-testid=cell][data-date="${other}"]`);
	await cell().click();
	await page.getByTestId('cell-input').fill('2h');
	await page.getByTestId('cell-input').press('Enter');
	await expect(cell()).toHaveText('2');
	await cell().click();
	await expect(page.getByTestId('cell-input')).toHaveValue('2');
	await page.getByTestId('cell-input').fill('3');
	await page.getByTestId('cell-input').press('Enter');
	await expect(cell()).toHaveText('3');
	// Changed elsewhere while I edit: the stale lock is said and the week reloads.
	await cell().click();
	await expect(page.getByTestId('cell-input')).toHaveValue('3');
	const entry = (await papi(page, 'GET', `/time-entries?issue=${k}&from=${other}&to=${other}`)).json.data.items[0];
	const elsewhere = await papi(page, 'POST', '/commands', { command_schema_version: 1, ops: [{ kind: 'update_time_entry', op_id: uuid(), target: { type: 'time_entry', id: entry.id }, set: { hours: '2.50' } }], preconditions: [{ target: { type: 'time_entry', id: entry.id }, lock_version: entry.lock_version }] });
	expect(elsewhere.status).toBe(200);
	await page.getByTestId('cell-input').fill('4');
	await page.getByTestId('cell-input').press('Enter');
	await expect(page.getByText('這筆工時剛被改過，已重新載入')).toBeVisible();
	await expect(cell()).toHaveText('2.5');
	// Today's cell holds two entries: a panel, not an inline edit.
	await row.locator(`[data-testid=cell][data-date="${today}"]`).click();
	await expect(page.getByTestId('cell-panel').getByTestId('panel-hours')).toHaveCount(2);
	await page.getByTestId('cell-panel').getByRole('button', { name: '關閉' }).click();
	// 0 is a real entry while the org accepts 0 hours (the default, T1).
	await cell().click();
	await expect(page.getByTestId('cell-input')).toHaveValue('2.5');
	await page.getByTestId('cell-input').fill('0');
	await page.getByTestId('cell-input').press('Enter');
	await expect(cell()).toHaveText('0');
	// Clearing asks, then deletes.
	await cell().click();
	await expect(page.getByTestId('cell-input')).toHaveValue('0');
	await page.getByTestId('cell-input').fill('');
	await page.getByTestId('cell-input').press('Enter');
	await expect(cell()).toHaveText('');
	const left = (await papi(page, 'GET', `/time-entries?issue=${k}`)).json.data.items;
	expect(left.map((t: { hours: string }) => t.hours).sort()).toEqual(['0.50', '1.50']);
});

test('工時：管理員設每日上限，超過時就地說明還能記多少；報表與模組開關', async ({ browser }) => {
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const admin = await ctx.newPage();
	await login(admin, 'admin', '/admin/timelog');
	await admin.getByTestId('tl-max').fill('8');
	await admin.getByTestId('tl-save').click();
	await expect.poll(async () => (await papi(admin, 'GET', '/org/timelog-settings')).json.data.max_hours_per_day).toBe('8.00');

	const page = await alice(browser, '/');
	page.on('dialog', (d) => d.accept());
	// Only administrators get the page (the settings themselves are readable by all).
	await page.goto('/admin/timelog');
	await expect(page.getByTestId('tl-denied')).toBeVisible();
	await expect(page.getByTestId('tl-save')).toHaveCount(0);
	const k = await newIssue(page, 'E2E 工時：每日上限');
	await page.goto(`/issue/${k}`);
	const spent = Number(sqlValue("SELECT COALESCE(sum(te.hours),0) FROM time_entries te JOIN principals u ON u.id = te.user_id WHERE u.login = 'alice' AND te.deleted_at IS NULL AND te.spent_on = (now() AT TIME ZONE 'Asia/Taipei')::date"));
	await page.getByTestId('log-time').click();
	await page.getByTestId('lt-hours').fill('9');
	await page.getByTestId('lt-submit').click();
	const show = (n: number) => String(Math.round(n * 100) / 100);
	await expect(page.getByTestId('logtime').getByTestId('error')).toContainText(`這天已經記了 ${show(spent)} 小時，上限是 8 小時（還能記 ${show(Math.max(0, 8 - spent))}）`);
	expect((await papi(page, 'GET', `/time-entries?issue=${k}`)).json.data.items).toHaveLength(0);
	await admin.getByTestId('tl-max').fill('0');
	await admin.getByTestId('tl-save').click();
	await expect.poll(async () => (await papi(admin, 'GET', '/org/timelog-settings')).json.data.max_hours_per_day).toMatch(/^0(\.00)?$/);

	// Report: grouped by person, with the only-visible footnote.
	await page.goto('/project/AUTH/time');
	await expect(page.getByTestId('tr-row').filter({ hasText: 'Alice Lin' })).toBeVisible();
	await expect(page.getByTestId('tr-note')).toHaveText('只包含你看得到的工時。');
	await expect(page.getByTestId('tr-nodrill')).toHaveCount(0);
	// By tracker there is no drill-down yet (backend gap G44), and it says so.
	await page.getByTestId('tr-group').selectOption({ label: '類型' });
	await expect(page.getByTestId('tr-nodrill')).toBeVisible();
	// Module off (confirmed), then the report and the issue card say so.
	await page.goto('/project/AUTH/settings/time');
	await page.getByTestId('tm-toggle').click();
	await expect(page.getByTestId('tm-state')).toHaveText('已關閉');
	await page.goto('/project/AUTH/time');
	await expect(page.getByTestId('time-off')).toHaveText('這個專案沒有開啟工時。');
	await page.goto(`/issue/${k}`);
	await expect(page.getByTestId('issue-subject')).toBeVisible();
	await expect(page.getByTestId('time-card')).toHaveCount(0);
	await page.goto('/project/AUTH/settings/time');
	await page.getByTestId('tm-toggle').click();
	await expect(page.getByTestId('tm-state')).toHaveText('已開啟');
});

// ---- F10: custom fields, forms, concepts (contract 05 §6.10, §6.11, §6.13) ----
async function adminPage(browser: Browser, next: string) {
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const page = await ctx.newPage();
	await login(page, 'admin', next);
	page.on('dialog', (d) => d.accept());
	return page;
}
async function trackerId(page: Page, name: string): Promise<string> {
	const ff = (await papi(page, 'GET', '/filter-fields')).json.data;
	return ff.fields.find((x: { field: string }) => x.field === 'tracker').options.find((o: { label: string }) => o.label === name).id;
}
async function setCf(page: Page, key: string, set: Record<string, unknown>) {
	const r = await papi(page, 'POST', '/commands', { command_schema_version: 1, ops: [{ kind: 'update_fields', op_id: uuid(), target: ref(key), set }], preconditions: [{ target: ref(key), lock_version: (await issueView(page, key)).lock_version }] });
	expect(r.status, JSON.stringify(r.json)).toBe(200);
}

test('自訂欄位：新增清單欄位、選項排序與停用、改必填前看影響、改可見性；非管理員進不去', async ({ browser }) => {
	const admin = await adminPage(browser, '/admin/custom-fields');
	await admin.getByTestId('cf-new').click();
	await admin.getByTestId('cf-add-name').fill('E2E 平台');
	await admin.getByTestId('cf-add-type').selectOption('list');
	await admin.getByTestId('cf-add-tracker').first().check();
	await admin.getByTestId('cf-add-options').fill('Web\niOS');
	await admin.getByTestId('cf-add-submit').click();
	await expect(admin.getByTestId('cf-title')).toHaveText('E2E 平台');
	await expect(admin.getByTestId('cf-option-label')).toHaveText(['Web', 'iOS']);
	await admin.getByTestId('cf-option-new').fill('Android');
	await admin.getByTestId('cf-option-add').click();
	await expect(admin.getByTestId('cf-option-label')).toHaveText(['Web', 'iOS', 'Android']);
	await admin.getByTestId('cf-option').nth(2).getByLabel('上移').click();
	await expect(admin.getByTestId('cf-option-label')).toHaveText(['Web', 'Android', 'iOS']);
	await admin.getByTestId('cf-option').nth(2).getByTestId('cf-option-toggle').click();
	await expect(admin.getByTestId('cf-option').nth(2)).toContainText('已停用');
	// Required: the impact shows before saving (B-11).
	await admin.getByTestId('cf-required').check();
	await expect(admin.getByTestId('cf-required-impact')).toBeVisible();
	await admin.getByTestId('cf-save').click();
	const mine = async () => (await papi(admin, 'GET', '/admin/custom-fields')).json.data.find((f: { name: string }) => f.name === 'E2E 平台');
	await expect.poll(async () => (await mine()).is_required).toBe(true);
	// Visibility asks first (the dialog is accepted) and is saved.
	await admin.getByTestId('cf-restricted').check();
	await admin.getByTestId('cf-save').click();
	await expect.poll(async () => (await mine()).visible).toBe(false);
	await admin.goto('/admin/custom-fields');
	await expect(admin.getByTestId('cf-row').filter({ hasText: 'E2E 平台' })).toContainText('受限');
	// Leave nothing that affects later tests.
	const f = await mine();
	await papi(admin, 'PATCH', `/admin/custom-fields/${f.id}`, { lock_version: f.lock_version, set: { is_required: false, visible: true, deactivated: true } });

	const page = await alice(browser, '/');
	await page.goto('/admin/custom-fields');
	await expect(page.getByTestId('cf-denied')).toBeVisible();
	await page.goto('/admin/forms');
	await expect(page.getByTestId('fd-denied')).toBeVisible();
	await page.goto('/admin/concepts');
	await expect(page.getByTestId('cc-denied')).toBeVisible();
});

test('換型別：文字欄位轉整數，預檢列出失敗，開始後完成並停用舊欄位', async ({ browser }) => {
	const admin = await adminPage(browser, '/');
	const made = (await papi(admin, 'POST', '/admin/custom-fields', { name: 'E2E 件數', ftype: 'string', is_for_all: true, tracker_ids: [await trackerId(admin, 'Bug')] })).json.data;
	const key = await newIssue(admin, 'E2E 換型別');
	await setCf(admin, key, { [made.key]: { string: '12' } });
	const key2 = await newIssue(admin, 'E2E 換型別失敗');
	await setCf(admin, key2, { [made.key]: { string: '十二' } });

	await admin.goto(`/admin/custom-fields/${made.id}/convert`);
	await admin.getByTestId('cv-type').selectOption('int');
	await admin.getByTestId('cv-name').fill('E2E 件數（整數）');
	await admin.getByTestId('cv-next1').click();
	await expect(admin.getByTestId('cv-parse')).toContainText('整數');
	await admin.getByTestId('cv-preview').click();
	await expect(admin.getByTestId('cv-ok')).toHaveText('1');
	await expect(admin.getByTestId('cv-fail')).toHaveText('1');
	await expect(admin.getByTestId('cv-step3')).toContainText(key2);
	await admin.getByTestId('cv-start').click();
	await expect(admin.getByTestId('cv-done')).toContainText('1 張已轉換', { timeout: 20_000 });
	await admin.getByTestId('cv-deactivate').click();
	await expect(admin.getByTestId('cv-step5')).toContainText('已停用');
	expect((await papi(admin, 'GET', `/admin/custom-fields/${made.id}`)).json.data.field.deactivated).toBe(true);
});

test('表單設計器：規則草稿→影響→擋下或發布並標記；版面區塊與顯示條件；試填兩邊一致；回報問題依版面即時顯示與必填', async ({ browser }) => {
	const admin = await adminPage(browser, '/admin/forms');
	await admin.getByTestId('form-row').filter({ hasText: 'Bug' }).getByTestId('form-design').click();
	await expect(admin.getByTestId('fd-title')).toContainText('Bug');
	// Rule: Root cause is required when Severity is High.
	await admin.getByTestId('fd-rule-add').click();
	await admin.getByTestId('fd-rule-field').selectOption({ label: 'Root cause' });
	await admin.getByTestId('fd-rule-kind').selectOption('required_if');
	await admin.getByTestId('cond-field').selectOption({ label: 'Severity' });
	await admin.getByTestId('cond-value').selectOption({ label: 'High' });
	await expect(admin.getByTestId('fd-rule-text')).toHaveText('當 Severity 是 High 時，Root cause 必填');
	await admin.getByTestId('fd-save').click();
	await expect(admin.getByTestId('fd-versions')).toContainText('規則 v0・有草稿');
	// An existing High issue without a root cause breaks the rule.
	const key = await newIssue(admin, 'E2E 高嚴重度沒有原因');
	const sev = (await papi(admin, 'GET', '/admin/custom-fields')).json.data.find((f: { name: string }) => f.name === 'Severity');
	await setCf(admin, key, { [sev.key]: { list: sev.options.find((o: { label: string }) => o.label === 'High').id } });
	await admin.getByTestId('fd-publish').click();
	await expect(admin.getByTestId('fd-impact')).toContainText('張現有議題不符合新規則');
	await admin.getByTestId('fd-mode-block').check();
	await admin.getByTestId('fd-publish-confirm').click();
	await expect(admin.getByTestId('fd-blocked')).toBeVisible();
	await admin.getByTestId('fd-publish').click();
	await admin.getByTestId('fd-mode-mark').check();
	await admin.getByTestId('fd-publish-confirm').click();
	await expect(admin.getByTestId('fd-versions')).toContainText('規則 v1');
	await expect(admin.getByTestId('fd-version').first()).toContainText('發布並標記不符的議題');
	await expect(admin.getByTestId('fd-version').first().getByTestId('fd-version-who')).toHaveText('你');

	// Layout: Severity in the first section; a section shown only when Severity is High holds Root cause.
	await admin.getByTestId('fd-tab-layout').click();
	await admin.getByTestId('fd-section').first().click();
	await admin.getByTestId('fd-lib-field').filter({ hasText: 'Severity' }).getByTestId('fd-lib-add').click();
	await admin.getByTestId('fd-section-add').click();
	const sec = admin.getByTestId('fd-section').last();
	await sec.getByTestId('fd-section-title').fill('原因分析');
	await admin.getByTestId('fd-lib-field').filter({ hasText: 'Root cause' }).getByTestId('fd-lib-add').click();
	await expect(sec.getByTestId('fd-field')).toHaveCount(1);
	await sec.getByTestId('fd-section-showif').click();
	await sec.getByTestId('cond-field').selectOption({ label: 'Severity' });
	await sec.getByTestId('cond-value').selectOption({ label: 'High' });
	await admin.getByTestId('fd-publish').click();
	await expect(admin.getByTestId('fd-publish-dialog')).toContainText('只改呈現，不影響任何寫入');
	await admin.getByTestId('fd-publish-confirm').click();
	await expect(admin.getByTestId('fd-versions')).toContainText('版面 v1');

	// Try-fill: both doors give the same answer (an empty subject is refused).
	// 「預覽」 at the top opens it (§6.10).
	await expect(admin.getByTestId('fd-try')).not.toHaveAttribute('open', '');
	await admin.getByTestId('fd-preview').click();
	await expect(admin.getByTestId('fd-try')).toHaveAttribute('open', '');
	await admin.getByTestId('fd-try-ui').click();
	await admin.getByTestId('fd-try-mcp').click();
	await expect(admin.getByTestId('fd-try-result').nth(0)).toContainText('會被擋下');
	await expect(admin.getByTestId('fd-try-result').nth(1)).toContainText('會被擋下');

	// The marked issue says so; the report form follows the layout.
	const page = await alice(browser, `/issue/${key}`);
	await expect(page.getByTestId('rule-marks')).toContainText('Root cause 必填');
	await page.goto('/new?project=AUTH');
	await page.getByTestId('new-tracker').selectOption({ label: 'Bug' });
	await expect(page.locator('#f-cf_1')).toBeVisible();
	await expect(page.locator('#f-cf_7')).toHaveCount(0);
	await page.locator('#f-cf_1').selectOption({ label: 'High' });
	await expect(page.getByTestId('form-section').filter({ hasText: '原因分析' }).locator('#f-cf_7')).toBeVisible();
	await page.locator('#f-subject').fill('E2E 依版面建立');
	await expect(page.getByTestId('new-submit')).toBeDisabled();
	await expect(page.getByTestId('new-form')).toContainText('還要填：Root cause');
	await page.locator('#f-cf_7').fill('設定錯誤');
	await page.getByTestId('new-submit').click();
	await page.waitForURL(/\/issue\/AUTH-\d+/);
	await expect(page.getByTestId('issue-subject')).toHaveText('E2E 依版面建立');
	await expect(page.getByTestId('rule-marks')).toHaveCount(0);
	// A field the layout hides is neither required nor sent (/new and edit).
	await page.goto('/new?project=AUTH');
	await page.getByTestId('new-tracker').selectOption({ label: 'Bug' });
	await page.locator('#f-cf_1').selectOption({ label: 'High' });
	await page.locator('#f-cf_7').fill('藏起來的值');
	await page.locator('#f-cf_1').selectOption({ label: 'Low' });
	await expect(page.locator('#f-cf_7')).toHaveCount(0);
	await page.locator('#f-subject').fill('E2E 藏起來不送');
	await expect(page.getByTestId('new-form')).not.toContainText('還要填');
	await page.getByTestId('new-submit').click();
	await page.waitForURL(/\/issue\/AUTH-\d+/);
	const hiddenKey = page.url().split('/').pop()!;
	const rootCause = async (k: string) => ((await papi(page, 'GET', `/issues/${k}`)).json.data.custom_fields as { name: string; value: unknown }[]).find((c) => c.name === 'Root cause')?.value ?? null;
	expect(await rootCause(hiddenKey)).toBeNull();
	await page.getByTestId('edit').click();
	await page.locator('#f-cf_1').selectOption({ label: 'High' });
	await expect(page.getByTestId('edit-missing')).toContainText('Root cause');
	await page.locator('#f-cf_7').fill('也藏起來');
	await page.locator('#f-cf_1').selectOption({ label: 'Normal' });
	await expect(page.getByTestId('edit-missing')).toHaveCount(0);
	await page.getByTestId('save').click();
	await expect.poll(async () => ((await issueView(page, hiddenKey)).custom_fields as { name: string; value: unknown }[]).find((c) => c.name === 'Severity')?.value).toMatchObject({ label: 'Normal' });
	expect(await rootCause(hiddenKey)).toBeNull();
});

test('概念與別名：建立概念、標籤被占用時說明並可前往、隱藏與暫存、試查', async ({ browser }) => {
	const admin = await adminPage(browser, '/admin/concepts');
	await admin.getByTestId('cc-new-pref').fill('Authentication');
	await admin.getByTestId('cc-new-alts').fill('登入, 認證');
	await admin.getByTestId('cc-new-submit').click();
	await expect(admin.getByTestId('cc-pref')).toHaveText('Authentication');
	await expect(admin.getByTestId('cc-label')).toHaveCount(3);
	await admin.getByTestId('cc-new-pref').fill('Session');
	await admin.getByTestId('cc-new-submit').click();
	await expect(admin.getByTestId('cc-pref')).toHaveText('Session');
	await admin.getByTestId('cc-label-new').fill('登入');
	await admin.getByTestId('cc-label-add').click();
	await expect(admin.getByTestId('cc-taken')).toContainText('「登入」已經是「Authentication」的標籤');
	await admin.getByTestId('cc-taken-go').click();
	await expect(admin.getByTestId('cc-pref')).toHaveText('Authentication');
	await admin.getByTestId('cc-label-new').fill('登錄');
	await admin.getByTestId('cc-label-add').click();
	const typo = admin.getByTestId('cc-label').filter({ hasText: '登錄' });
	await typo.getByTestId('cc-label-hidden').click();
	await expect(typo).toContainText('隱藏（錯字）');
	const alt = admin.getByTestId('cc-label').filter({ hasText: '認證' });
	await alt.getByTestId('cc-label-status').click();
	await expect(alt).toContainText('暫存（不展開）');
	await admin.getByTestId('cc-probe-q').fill('登入');
	await expect(admin.getByTestId('cc-probe-config')).toContainText('Authentication');
	await expect(admin.getByTestId('cc-probe-config')).not.toContainText('認證');
	await admin.getByTestId('cc-probe-run').click();
	await expect(admin.getByTestId('cc-probe-result')).toBeVisible();
});

// ---------------------------------------------------------------- F11: home, preferences, notifications, watch (contract 05 §6.12, §6.15–§6.17)

test('首次登入選角色、首頁編輯儲存、偏好（主題、預覽、改角色、捷徑）', async ({ browser }) => {
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const page = await ctx.newPage();
	await login(page, 'carol', '/');
	const ob = page.getByTestId('onboarding');
	await expect(ob).toContainText('你主要做什麼？');
	await expect(ob.getByTestId('ob-submit')).toBeDisabled();
	await ob.getByTestId('ob-role-engineer').check();
	await ob.getByTestId('ob-submit').click();
	await expect(ob).toHaveCount(0);
	// The engineer template: 本週工時 and Agent 摘要; shortcuts 看板 and 指派給我.
	const types = () => page.getByTestId('widget').evaluateAll((es) => es.map((e) => (e as HTMLElement).dataset.type));
	await expect.poll(types).toEqual(['need_decision', 'view', 'my_time_week', 'agent_summary']);
	await expect(page.getByTestId('nav-shortcut')).toHaveText([/看板$/, /指派給我$/]);
	// The engineer default view opens a project on its board.
	await page.goto('/project/AUTH');
	await expect(page).toHaveTitle(/看板/);

	// Edit: 需要你決定 has no remove button; add Triage 計數, widen, remove Agent 摘要, move.
	await page.goto('/');
	await page.getByTestId('edit-home').click();
	const decide = page.locator('[data-testid=widget][data-type=need_decision]');
	await expect(decide.getByTestId('w-remove')).toHaveCount(0);
	await expect(decide.getByTestId('w-protected')).toHaveAttribute('title', '這個區塊不能移除');
	await page.getByTestId('home-add-type').selectOption('triage_count');
	await page.getByTestId('home-add').click();
	await page.locator('[data-testid=widget][data-type=agent_summary]').getByTestId('w-remove').click();
	await page.locator('[data-testid=widget][data-type=my_time_week]').getByTestId('w-width').selectOption('2');
	await page.locator('[data-testid=widget][data-type=triage_count]').getByTestId('w-up').click();
	await page.getByTestId('home-save').click();
	await expect.poll(types).toEqual(['need_decision', 'view', 'triage_count', 'my_time_week']);
	const saved = (await papi(page, 'GET', '/me/home')).json.data;
	expect(saved.widgets.map((w: { type: string }) => w.type)).toEqual(['need_decision', 'view', 'triage_count', 'my_time_week']);
	expect(saved.widgets.find((w: { type: string }) => w.type === 'my_time_week').width).toBe(2);
	// Saved from another tab meanwhile: the latest version is loaded, not overwritten.
	await page.getByTestId('edit-home').click();
	await page.locator('[data-testid=widget][data-type=triage_count]').getByTestId('w-remove').click();
	await papi(page, 'PUT', '/me/home', { lock_version: saved.lock_version, widgets: saved.widgets.map((w: { id: string; type: string; width: number; view_id?: string; limit?: number }) => ({ id: w.id, type: w.type, width: w.width, ...(w.type === 'view' ? { view_id: w.view_id, limit: w.limit } : {}) })).filter((w: { type: string }) => w.type !== 'my_time_week') });
	await page.getByTestId('home-save').click();
	await expect(page.locator('.toast', { hasText: '首頁在別處被修改過，已載入最新版本' })).toBeVisible();
	await expect.poll(types).toEqual(['need_decision', 'view', 'triage_count']);

	// Preferences: theme on the server and on the page; role preview; role change asks.
	await page.goto('/me/preferences');
	await page.getByTestId('pf-theme-high_contrast').check();
	await expect(page.locator('html')).toHaveAttribute('data-theme', 'hc');
	await expect.poll(async () => (await papi(page, 'GET', '/me/preferences')).json.data.theme).toBe('high_contrast');
	await page.reload();
	await expect(page.locator('html')).toHaveAttribute('data-theme', 'hc');
	await page.getByTestId('pf-theme-dark').check();
	await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
	await page.getByTestId('pf-density-compact').check();
	await expect(page.locator('html')).toHaveAttribute('data-density', 'compact');
	await page.getByTestId('pf-density-comfortable').check();
	await page.getByTestId('pf-preview-role').selectOption('pm');
	await expect(page.getByTestId('pf-preview-banner')).toHaveText('預覽：PM的預設首頁（用你自己的權限顯示）');
	await expect(page.getByTestId('pf-preview-widget')).toHaveCount(4);
	await page.getByTestId('pf-role-pm').check();
	await expect(page.getByTestId('pf-apply-ask')).toBeVisible();
	await page.getByTestId('pf-keep').click();
	expect((await papi(page, 'GET', '/me/preferences')).json.data.interface_roles).toEqual(['pm', 'engineer']);
	expect((await papi(page, 'GET', '/me/home')).json.data.widgets.map((w: { type: string }) => w.type)).toEqual(['need_decision', 'view', 'triage_count']);
	// Shortcuts: drop 看板, add 我的工時 first.
	const sc = page.getByTestId('pf-shortcut');
	await expect(sc).toHaveCount(2);
	await sc.first().getByTestId('pf-shortcut-remove').click();
	await page.getByTestId('pf-shortcut-kind').selectOption('timesheet');
	await page.getByTestId('pf-shortcut-add').click();
	await sc.last().getByLabel('上移').click();
	await page.getByTestId('pf-shortcut-save').click();
	await expect(page.getByTestId('nav-shortcut')).toHaveText([/我的工時$/, /指派給我$/]);
	await expect(page.getByTestId('nav-shortcut').first()).toHaveAttribute('href', '/time');
});

const DISPATCH_MS = 60_000;
test('通知：作者收到別人的留言與欄位變更，鈴鐺、通知中心、開單即已讀、全部標為已讀；偏好靜音專案', async ({ browser }) => {
	// Dispatch waits for older transactions anywhere on the server (other test databases too).
	test.setTimeout(300_000);
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const page = await ctx.newPage();
	await login(page, 'carol', '/');
	// carol wrote the seed issues, so earlier tests already notified her: start clean.
	await papi(page, 'POST', '/notifications/read-all', { before: new Date(Date.now() + 1000).toISOString() });
	await page.goto('/notifications');
	await expect(page.getByTestId('nt-empty')).toHaveText(/沒有未讀通知。/);
	await expect(page.getByTestId('bell-count')).toHaveCount(0);
	await expect(page.getByTestId('bell')).toHaveAttribute('aria-label', '通知');

	// carol wrote AUTH-4 (seed); admin comments and changes a field.
	const admin = await adminPage(browser, '/search');
	const lv = (await issueView(admin, 'AUTH-4')).lock_version;
	const r = await papi(admin, 'POST', '/commands', {
		command_schema_version: 1,
		ops: [
			{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-4'), body: 'E2E：已在 Safari 17 重現' },
			{ kind: 'update_fields', op_id: uuid(), target: ref('AUTH-4'), set: { estimated_hours: '2.5' } }
		],
		preconditions: [{ target: ref('AUTH-4'), lock_version: lv }]
	});
	expect(r.status, JSON.stringify(r.json)).toBe(200);

	// The bell asks again when the tab gains focus (no push channel, B-26).
	await expect
		.poll(
			async () => {
				await page.evaluate(() => window.dispatchEvent(new Event('focus')));
				return (await page.getByTestId('bell-count').textContent({ timeout: 500 }).catch(() => null)) ?? '';
			},
			{ timeout: DISPATCH_MS }
		)
		.not.toBe('');
	await expect(page.getByTestId('bell')).toHaveAttribute('aria-label', /^通知，\d+ 則未讀$/);
	await page.getByTestId('bell').click();
	const row = page.getByTestId('bell-row').filter({ hasText: 'AUTH-4' });
	await expect(row).toHaveCount(1);
	await expect(row).toContainText('你建立的');
	await expect(row).toContainText(/Admin 把預估工時從 .+ 改成 2\.5/);
	await expect(row).toContainText('還有 1 則更新');
	await page.getByTestId('bell-all').click();
	await page.waitForURL('**/notifications');
	const nrow = page.getByTestId('nt-row').filter({ hasText: 'AUTH-4' });
	await expect(nrow).toHaveCount(1);
	await expect(nrow).toHaveClass(/unread/);
	await expect(nrow).toContainText('你建立的');
	// Marked read here, unread again, then opened: the issue page reads it.
	await nrow.getByTestId('nt-toggle').click();
	await expect(nrow).toHaveCount(0);
	await page.getByTestId('nt-all').click();
	// Earlier tests may have left older (read) AUTH-4 rows; this one is the newest.
	const latest = nrow.first();
	await expect(latest).toContainText('2.5');
	await expect(latest).not.toHaveClass(/unread/);
	// J moves the selection.
	if ((await page.getByTestId('nt-row').count()) > 1) {
		await page.keyboard.press('j');
		await expect(page.getByTestId('nt-row').nth(1)).toHaveClass(/sel/);
		await page.keyboard.press('k');
		await expect(page.getByTestId('nt-row').nth(0)).toHaveClass(/sel/);
	}
	await latest.getByTestId('nt-toggle').click();
	await expect(latest.getByTestId('nt-toggle')).toHaveText('標為已讀');
	await page.getByTestId('nt-unread').click();
	await expect(nrow).toHaveCount(1);
	await nrow.locator('.nmain').click();
	await page.waitForURL('**/issue/AUTH-4');
	const unreadAuth4 = async () => ((await papi(page, 'GET', '/notifications?filter=unread')).json.data.items as { issue: { key: string } | null }[]).filter((n) => n.issue?.key === 'AUTH-4').length;
	await expect.poll(unreadAuth4).toBe(0);

	const note = async (body: string) => {
		const lv = (await issueView(admin, 'AUTH-4')).lock_version;
		const r = await papi(admin, 'POST', '/commands', { command_schema_version: 1, ops: [{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-4'), body }], preconditions: [{ target: ref('AUTH-4'), lock_version: lv }] });
		expect(r.status, JSON.stringify(r.json)).toBe(200);
	};
	// Opening the issue page itself reads its notification (§3.11.3).
	await note('E2E：第二則');
	await expect.poll(unreadAuth4, { timeout: DISPATCH_MS }).toBe(1);
	await page.goto('/issue/AUTH-4');
	await expect.poll(unreadAuth4).toBe(0);

	// 全部標為已讀 marks only what the list showed: a later update stays unread.
	await note('E2E：第三則');
	await expect.poll(unreadAuth4, { timeout: DISPATCH_MS }).toBe(1);
	await page.goto('/notifications');
	const r3 = page.getByTestId('nt-row').filter({ hasText: 'AUTH-4' });
	await expect(r3.getByTestId('nt-event')).toHaveText('Admin 留言：E2E：第三則');
	const lastAt = async () => ((await papi(page, 'GET', '/notifications?filter=unread')).json.data.items as { issue: { key: string } | null; last_at: string }[]).find((n) => n.issue?.key === 'AUTH-4')?.last_at;
	const shownAt = await lastAt();
	await note('E2E：第四則');
	await expect.poll(lastAt, { timeout: DISPATCH_MS }).not.toBe(shownAt);
	// Filtered to one project the button would still mark every project: it is off.
	await page.getByTestId('nt-project').selectOption('AUTH');
	await expect(page.getByTestId('nt-read-all')).toBeDisabled();
	await page.getByTestId('nt-project').selectOption('');
	await expect(r3.getByTestId('nt-event')).toHaveText('Admin 留言：E2E：第四則');
	// Reload happened with the filter change; take the list as now shown, then one more arrives.
	const shownAt2 = await lastAt();
	await note('E2E：第五則');
	await expect.poll(lastAt, { timeout: DISPATCH_MS }).not.toBe(shownAt2);
	await page.getByTestId('nt-read-all').click();
	await expect(page.locator('.toast', { hasText: '已標為已讀' })).toBeVisible();
	await expect(r3.getByTestId('nt-event')).toHaveText('Admin 留言：E2E：第五則');
	expect(await unreadAuth4()).toBe(1);
	await papi(page, 'POST', '/notifications/read-all', { before: new Date(Date.now() + 1000).toISOString() });

	// Preferences: mute AUTH (saved; nothing comes from it), then undo.
	await page.getByTestId('nt-prefs').click();
	await page.waitForURL('**/me/notifications');
	await expect(page.getByTestId('np-on_self')).not.toBeChecked();
	await page.getByTestId('np-mute-AUTH').check();
	await page.getByTestId('np-save').click();
	await expect(page.locator('.toast', { hasText: '通知偏好已儲存' })).toBeVisible();
	const authId = (await papi(page, 'GET', '/projects')).json.data.find((p: { key: string }) => p.key === 'AUTH').id;
	expect((await papi(page, 'GET', '/me/notification-preferences')).json.data.muted_projects).toEqual([authId]);
	// Saved elsewhere meanwhile: the latest settings come back.
	const cur = (await papi(page, 'GET', '/me/notification-preferences')).json.data;
	await papi(page, 'PUT', '/me/notification-preferences', { lock_version: cur.lock_version, muted_projects: [] });
	await page.getByTestId('np-on_self').check();
	await page.getByTestId('np-save').click();
	await expect(page.locator('.toast', { hasText: '設定在別處被修改過，已載入最新版本' })).toBeVisible();
	await expect(page.getByTestId('np-mute-AUTH')).not.toBeChecked();
	await expect(page.getByTestId('np-on_self')).not.toBeChecked();
});

test('關注：議題頁關注與取消（人數、取消後不再自動關注），失敗時回原狀', async ({ browser }) => {
	const page = await alice(browser, '/');
	const key = await newIssue(page, 'E2E 關注');
	// Auto-watch is written by the background dispatcher (B-26: within 10 s).
	await expect.poll(async () => (await papi(page, 'GET', `/issues/${key}`)).json.data.watch?.watching, { timeout: DISPATCH_MS }).toBe(true);
	await page.goto(`/issue/${key}`);
	const w = page.getByTestId('watch');
	// alice wrote it: watching automatically, and says why.
	await expect(w).toContainText('關注中 ✓');
	await expect(w).toHaveAttribute('title', '你是這張單的作者，所以自動關注');
	await expect(page.getByTestId('watch-count')).toHaveText('1 人關注');
	await w.hover();
	await expect(w).toContainText('取消關注');
	await w.click();
	await page.mouse.move(0, 0);
	await expect(w).toContainText('關注');
	await expect(w).not.toContainText('關注中');
	await expect(page.getByTestId('watch-count')).toHaveText('0 人關注');
	await expect(w).toHaveAttribute('title', '你已取消關注，不會再自動關注這張單');
	await expect(w).toBeEnabled();
	expect((await papi(page, 'GET', `/issues/${key}`)).json.data.watch).toMatchObject({ watching: false, opted_out: true, watcher_count: 0 });
	await page.reload();
	await expect(page.getByTestId('watch')).toHaveAttribute('title', '你已取消關注，不會再自動關注這張單');
	await page.getByTestId('watch').click();
	await page.mouse.move(0, 0);
	await expect(page.getByTestId('watch-count')).toHaveText('1 人關注');
	await expect(page.getByTestId('watch')).toBeEnabled();
	expect((await papi(page, 'GET', `/issues/${key}`)).json.data.watch).toMatchObject({ watching: true, opted_out: false });
	// The server refuses: the button goes back and says so.
	await page.route(`**/api/v1/issues/${key}/watch`, (r) => r.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { code: 'temporarily_unavailable', message: 'x', retryable: true, next_actions: [] } }) }));
	await page.getByTestId('watch').click();
	await page.mouse.move(0, 0);
	await expect(page.locator('.toast', { hasText: '取消關注失敗' })).toBeVisible();
	await expect(page.getByTestId('watch')).toContainText('關注中');
	await expect(page.getByTestId('watch-count')).toHaveText('1 人關注');
});

/** Step-up shows only when the session has not confirmed recently. */
async function maybeStepUp(page: Page) {
	try {
		await page.getByTestId('step-up').waitFor({ timeout: 2500 });
	} catch {
		return;
	}
	await page.getByTestId('step-up-password').fill(PASSWORD);
	await page.getByTestId('step-up-confirm').click();
	await expect(page.getByTestId('step-up')).toHaveCount(0);
}

test('AI 助理：管理員設定服務（測試、預算、啟用），使用者設定助理後對話、串流、外部連結確認、寫入卡片', async ({ browser }) => {
	test.setTimeout(180_000);
	// The model is e2e/fakeai.mjs on loopback, registered as endpoint FAKE.
	const admin = await adminPage(browser, '/admin/ai');
	await expect(admin.getByTestId('ai-endpoints')).toContainText('FAKE');
	await expect(admin.getByTestId('ai-enabled')).toBeDisabled();
	await expect(admin.getByTestId('ai-blockers')).toContainText('還沒有使用中的服務');
	await admin.getByTestId('ai-provider-new').click();
	await admin.getByTestId('pf-kind').selectOption('openai_compat');
	await admin.getByTestId('pf-name').fill('本機假模型');
	await admin.getByTestId('pf-endpoint').selectOption('FAKE');
	await admin.getByTestId('pf-model').fill('fake-model');
	await expect(admin.getByTestId('pf-save')).toBeDisabled();
	await admin.getByTestId('pf-d17').check();
	await admin.getByTestId('pf-save').click();
	await maybeStepUp(admin);
	const prov = admin.getByTestId('ai-provider').filter({ hasText: '本機假模型' });
	await expect(prov).toBeVisible();
	await prov.getByTestId('ai-test').click();
	await maybeStepUp(admin);
	await expect(prov.getByTestId('ai-provider-test')).toContainText('成功');
	await expect(prov).toContainText('工具呼叫支援');
	if (await prov.getByTestId('ai-activate').isVisible()) {
		await prov.getByTestId('ai-activate').click();
		await maybeStepUp(admin);
	}
	await expect(admin.getByTestId('ai-blockers')).toContainText('沒有月預算');
	await admin.getByTestId('ai-budget-month').fill('50');
	await admin.getByTestId('ai-budget-save').click();
	await expect(admin.getByTestId('ai-blockers')).toHaveCount(0);
	await admin.getByTestId('ai-enabled').check();
	await maybeStepUp(admin);
	await expect(admin.getByTestId('ai-enabled')).toBeChecked();

	// alice: the right column shows 「AI 助理」, first the setup.
	const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
	contexts.push(ctx);
	const page = await ctx.newPage();
	await login(page, 'alice', '/issue/AUTH-1');
	await page.getByTestId('tab-ai').click();
	await page.getByTestId('ai-setup-open').click();
	await page.getByTestId('ai-setup-AUTH').check();
	// 05 §3.12: read_own_time is on by default for the personal assistant, behind the A1 warning.
	await expect(page.getByTestId('ai-setup-time')).toBeChecked();
	await expect(page.getByTestId('ai-setup-save')).toBeDisabled();
	await page.getByTestId('ai-setup-time-ack').check();
	await page.getByTestId('ai-setup-save').click();
	await maybeStepUp(page);
	await expect(page.getByTestId('ai-setup')).toHaveCount(0);
	// The page reference rides along, never the page's text.
	await expect(page.getByTestId('ai-scope')).toContainText('AUTH-1');

	await page.getByTestId('ai-input').fill('你好');
	await page.getByTestId('ai-input').press('Enter');
	const reply = page.getByTestId('ai-msg').filter({ has: page.getByTestId('ai-text') }).last();
	await expect(reply).toContainText('你好，我是 AI 助理。相關的單：AUTH-1');
	await expect(reply.getByRole('link', { name: 'AUTH-1' })).toHaveAttribute('href', '/issue/AUTH-1');
	await expect(page.getByTestId('ai-send')).toBeVisible();

	// A write: the model submits a note through command_submit.
	await page.getByTestId('ai-input').fill('幫我在 AUTH-1 留言');
	await page.getByTestId('ai-send').click();
	await expect(page.getByTestId('ai-tool').last()).toBeVisible();
	// The backend sends `changed` where §3.13.4 says `note_added` (backend gap G38).
	await expect(page.getByTestId('ai-write').last()).toContainText(/已在 AUTH-1 (新增留言|修改)/);
	const last = page.getByTestId('ai-text').last();
	await expect(last).toContainText('說明文件');
	await expect(last).toContainText('〔docs.example.com〕');
	// External links ask first.
	await last.getByRole('link', { name: '說明文件' }).click();
	await expect(page.getByTestId('ai-leave')).toContainText('docs.example.com');
	await page.getByTestId('ai-leave').getByRole('button', { name: '取消' }).click();
	await expect(page.getByTestId('ai-leave')).toHaveCount(0);

	// Reloading keeps the conversation (messages come from the server).
	await page.reload();
	await page.getByTestId('tab-ai').click();
	await expect(page.getByTestId('ai-text').first()).toContainText('你好，我是 AI 助理。');

	// A page the assistant cannot read (WEB is not delegated): the server's notice
	// comes first, the reply streams below it.
	await page.goto('/project/WEB');
	await page.getByTestId('tab-ai').click();
	await expect(page.getByTestId('ai-scope')).toContainText('WEB');
	const before = await page.getByTestId('ai-text').count();
	await page.getByTestId('ai-input').fill('這頁在講什麼');
	await page.getByTestId('ai-input').press('Enter');
	await expect(page.getByTestId('ai-notice').last()).toHaveText('AI 助理讀不到這個頁面（私有議題或沒委派的專案）');
	await expect(page.getByTestId('ai-text')).toHaveCount(before + 1);
	await expect(page.getByTestId('ai-text').last()).toContainText('你好，我是 AI 助理。相關的單：AUTH-1');
	const order = await page.getByTestId('ai-messages').evaluate((el) => [...el.querySelectorAll('[data-testid=ai-notice],[data-testid=ai-text]')].slice(-2).map((x) => x.getAttribute('data-testid')));
	expect(order).toEqual(['ai-notice', 'ai-text']);
	await expect(page.getByTestId('ai-send')).toBeVisible();

	// The ⋯ menu: the full conversation history opens an older conversation.
	await page.getByTestId('ai-menu').click();
	await page.getByTestId('ai-history').click();
	await expect(page.getByTestId('ai-history-item')).toHaveCount(1);
	await page.getByTestId('ai-history-item').first().click();
	await expect(page.getByTestId('ai-text').first()).toContainText('你好，我是 AI 助理。');

	// The admin's audit list says what changed (§6.20).
	await admin.reload();
	await expect(admin.getByTestId('ai-event').first()).toBeVisible();
	await expect(admin.getByTestId('ai-events')).toContainText('月預算 未設定 → 50');
	await expect(admin.getByTestId('ai-events')).toContainText('本機假模型');

	// Below 1280 px the column is a drawer the top bar's 「AI」 opens; Esc closes it,
	// and a link followed from it shows its page.
	await page.setViewportSize({ width: 1100, height: 800 });
	await expect(page.getByTestId('collab')).toBeHidden();
	await expect(page.getByTestId('top-ai')).toHaveText('AI');
	await page.getByTestId('top-ai').click();
	await expect(page.getByTestId('collab')).toBeVisible();
	await expect(page.getByTestId('tab-ai')).toBeVisible();
	await expect(page.getByTestId('ai-text').first()).toBeVisible();
	// Focus moves into the drawer, and back to the button when it closes.
	await expect(page.getByTestId('drawer-close')).toBeFocused();
	await expect(page.getByRole('dialog', { name: '協作欄' })).toBeVisible();
	await page.keyboard.press('Escape');
	await expect(page.getByTestId('collab')).toBeHidden();
	await expect(page.getByTestId('top-ai')).toBeFocused();
	await page.getByTestId('top-ai').click();
	await page.getByTestId('ai-text').first().getByRole('link', { name: 'AUTH-1' }).click();
	await page.waitForURL('**/issue/AUTH-1');
	await expect(page.getByTestId('collab')).toBeHidden();
	await page.setViewportSize({ width: 1440, height: 900 });
	await expect(page.getByTestId('top-ai')).toHaveCount(0);
	await expect(page.getByTestId('collab')).toBeVisible();

	// §6.14: the assistant reads alice's time (on by default), and its activity page says so.
	const assistant = (await papi(page, 'GET', '/ai/status')).json.data.assistant.principal;
	await page.goto(`/agents/${assistant}/activity`);
	await expect(page.getByTestId('reads-my-time')).toContainText('這個 agent 可讀你的工時');

	// Turning the AI off hides the tab.
	await admin.getByTestId('ai-enabled').uncheck();
	await maybeStepUp(admin);
	await expect(admin.getByTestId('ai-enabled')).not.toBeChecked();
	await page.reload();
	await expect(page.getByTestId('tab-activity')).toBeVisible();
	await expect(page.getByTestId('tab-ai')).toHaveCount(0);
});

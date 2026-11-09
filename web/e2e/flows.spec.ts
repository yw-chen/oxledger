// SPDX-License-Identifier: GPL-2.0-or-later
// The main human flows of contract 02 §14 against the real backend, with
// e2e/agent.mjs playing the agent over MCP: delegate and get a token (§16),
// open a proposal link and approve (用法 1, 2, 8), approve a batch from the
// queue (用法 3), revise a conflicting proposal (用法 7), suspend, undo, and
// the human's own direct path (report, edit). Runs serially on one fresh DB.
import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test';
// @ts-expect-error plain ESM helper without types
import { Agent } from './agent.mjs';
import { MCP_URL } from '../playwright.config';
import { execFileSync } from 'node:child_process';

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
}

/** A REST call inside the page with the session's CSRF token. */
async function papi(page: Page, method: string, path: string, body?: unknown) {
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
test.afterEach(async () => {
	for (const c of contexts.splice(0)) await c.close();
});
async function alice(browser: Browser, next = '/') {
	const ctx = await browser.newContext();
	contexts.push(ctx);
	const page = await ctx.newPage();
	await login(page, 'alice', next);
	return page;
}

test('委派：精靈建立 AUTH 委派，token 只顯示一次', async ({ browser }) => {
	const page = await alice(browser, '/agents/new');
	await page.locator('input[name=client]').first().check();
	await page.getByTestId('next').click();
	await page.locator('input[name=project]', { hasText: '' }).first().waitFor();
	await page.getByLabel(/AUTH・/).check();
	await page.getByTestId('next').click();
	await page.getByTestId('all-review').click();
	await page.getByTestId('next').click();
	await expect(page.getByTestId('confirm')).toContainText('AUTH');
	await page.getByTestId('create-delegation').click();
	if (await page.getByTestId('step-up').isVisible().catch(() => false)) {
		await page.getByTestId('step-up-password').fill(PASSWORD);
		await page.getByTestId('create-delegation').click();
	}
	await expect(page.getByTestId('wizard-done')).toBeVisible();
	token = (await page.getByTestId('token-value').textContent())!.trim();
	expect(token).toMatch(/^oxa_/);
	agent = new Agent(token, MCP_URL);
	// The token works for MCP right away.
	const got = await agent.call('get', { key: 'AUTH-1', fields: ['subject'] });
	expect(got.subject).toBeTruthy();
	// Leaving and coming back never shows it again.
	await page.goto('/agents');
	await expect(page.getByTestId('context').first()).toBeVisible();
	await expect(page.getByTestId('token-value')).toHaveCount(0);
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
	await page.getByTestId('use-proposed').first().check();
	await expect(page.getByTestId('preview-status')).toContainText(/通過/);
	await page.getByTestId('revise-apply').click();
	await expect(page.getByTestId('exec-result')).toContainText('已套用');
	await page.goto('/issue/AUTH-4');
	await expect(page.getByTestId('issue-subject')).toHaveText('agent 的標題');
});

test('暫停：人暫停後 agent 寫入被擋，恢復後可以再提案', async ({ browser }) => {
	const page = await alice(browser, '/agents');
	await page.getByTestId('suspend').first().click();
	await expect(page.getByTestId('suspended').first()).toBeVisible();
	await expect(propose('暫停中', [{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-2'), body: 'x' }], ['AUTH-2'])).rejects.toThrow(/agent_suspended/);
	await page.getByTestId('resume').first().click();
	await expect(page.getByTestId('suspended')).toHaveCount(0);
	const ok = await propose('恢復後', [{ kind: 'add_note', op_id: uuid(), target: ref('AUTH-2'), body: '恢復後的提案' }], ['AUTH-2']);
	expect(ok.changeset_id).toBeTruthy();
});

test('回復：活動頁把 agent 套用的到期日改回去', async ({ browser }) => {
	const page = await alice(browser, '/agents');
	await page.getByRole('link', { name: '活動', exact: true }).first().click();
	await expect(page.getByTestId('activity').first()).toBeVisible();
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
});

/** SQL on the E2E database only (GitHub data cannot come from GitHub here). */
function sql(text: string) {
	execFileSync('docker', ['exec', '-i', 'oxledger-pg', 'psql', '-v', 'ON_ERROR_STOP=1', '-q', '-U', 'oxledger', '-d', 'oxledger_fe_e2e'], { input: text });
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
});

test('登入的 next 只接受本站路徑', async ({ browser }) => {
	for (const [next, want] of [
		['//evil.example/x', '/'],
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

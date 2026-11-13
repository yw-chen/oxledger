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

function sqlValue(text: string): string {
	return execFileSync('docker', ['exec', '-i', 'oxledger-pg', 'psql', '-q', '-t', '-A', '-U', 'oxledger', '-d', 'oxledger_fe_e2e'], { input: text }).toString().trim();
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

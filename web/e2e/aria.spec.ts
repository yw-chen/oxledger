// SPDX-License-Identifier: GPL-2.0-or-later
// G10: the accessibility tree of a few stable pages as golden files
// (e2e/__snapshots__/aria.spec.ts/*.aria.yml). They catch a lost heading,
// label or button name that a screenshot would not. Only read-only visits on
// the fixture data, so this file can run before flows.spec.ts on the same
// fresh database. Dynamic text (keys, times, names, counts) is a /regex/ in the
// .aria.yml. Update locally with `npx playwright test e2e/aria.spec.ts
// --update-snapshots`, then review the diff.
import { expect, test, type Page } from '@playwright/test';

const PASSWORD = 'oxledger-demo-pass'; // test value for the oxledger_fe_* databases only
test.describe.configure({ mode: 'serial' });

async function login(page: Page, user: string, next: string) {
	await page.goto(`/login?next=${encodeURIComponent(next)}`);
	await page.fill('input[name=username]', user);
	await page.fill('input[name=password]', PASSWORD);
	await page.click('button[type=submit]');
	await page.waitForURL((u) => u.pathname === next.split('?')[0]);
	await page.waitForLoadState('networkidle');
}

test('aria：登入頁', async ({ page }) => {
	await page.goto('/login');
	await expect(page.locator('button[type=submit]')).toBeVisible();
	await expect(page.locator('#app')).toMatchAriaSnapshot({ name: 'login.aria.yml' });
});

test('aria：專案議題清單', async ({ page }) => {
	await login(page, 'alice', '/project/AUTH/issues');
	await expect(page.locator('#main')).toMatchAriaSnapshot({ name: 'project-issues.aria.yml' });
});

test('aria：議題頁', async ({ page }) => {
	await login(page, 'alice', '/issue/AUTH-1');
	await expect(page.locator('#main')).toMatchAriaSnapshot({ name: 'issue.aria.yml' });
});

test('aria：通知中心', async ({ page }) => {
	await login(page, 'alice', '/notifications');
	await expect(page.locator('#main')).toMatchAriaSnapshot({ name: 'notifications.aria.yml' });
});

test('aria：偏好設定', async ({ page }) => {
	await login(page, 'alice', '/me/preferences');
	await expect(page.locator('#main')).toMatchAriaSnapshot({ name: 'preferences.aria.yml' });
});

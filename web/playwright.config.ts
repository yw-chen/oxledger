// SPDX-License-Identifier: GPL-2.0-or-later
// E2E against the real backend (playbook): its own database oxledger_fe_e2e
// (reset on every run), backend on 18082 and the SPA on 5176 so the dev
// preview (18080/5174) keeps running. Build the backend first:
//   cargo build -p oxledger --features test-support
import { defineConfig } from '@playwright/test';

const BACKEND_PORT = 18082;
const WEB_PORT = 5176;
const WEB = `http://localhost:${WEB_PORT}`;

export default defineConfig({
	testDir: 'e2e',
	testMatch: '*.spec.ts',
	fullyParallel: false,
	workers: 1,
	timeout: 60_000,
	expect: { timeout: 15_000 },
	reporter: [['list']],
	use: { baseURL: WEB, locale: 'zh-TW', timezoneId: 'Asia/Taipei', trace: 'retain-on-failure', screenshot: 'only-on-failure' },
	webServer: [
		{
			command: 'node e2e/backend.mjs reset-serve oxledger_fe_e2e',
			url: `http://127.0.0.1:${BACKEND_PORT}/healthz`,
			env: { OX_BACKEND_PORT: String(BACKEND_PORT), OX_SPA_ORIGIN: WEB },
			timeout: 120_000,
			reuseExistingServer: false,
			stdout: 'ignore'
		},
		{
			command: `npx vite dev --host localhost --port ${WEB_PORT} --strictPort`,
			url: WEB,
			env: { OX_BACKEND: `http://127.0.0.1:${BACKEND_PORT}` },
			timeout: 120_000,
			reuseExistingServer: false
		}
	]
});

export const MCP_URL = `http://127.0.0.1:${BACKEND_PORT}/mcp`;

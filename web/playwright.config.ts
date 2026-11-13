// SPDX-License-Identifier: GPL-2.0-or-later
// E2E against the real backend (playbook): its own database oxledger_fe_e2e
// (reset on every run), backend on 18082, the SPA on 5176 and the stand-in
// model (e2e/fakeai.mjs) on 18091, so the dev preview (18080/5174) keeps
// running. A second run beside this one (a verifier) sets its own:
//   OX_E2E_DB=oxledger_fe_e2e_verify OX_E2E_BACKEND_PORT=18083 OX_E2E_WEB_PORT=5177 OX_FAKE_AI_PORT=18092
// Build the backend first:
//   cargo build -p oxledger --features test-support,ai
import { defineConfig } from '@playwright/test';

const DB = process.env.OX_E2E_DB ?? 'oxledger_fe_e2e';
const BACKEND_PORT = Number(process.env.OX_E2E_BACKEND_PORT ?? 18082);
const WEB_PORT = Number(process.env.OX_E2E_WEB_PORT ?? 5176);
const FAKE_AI_PORT = process.env.OX_FAKE_AI_PORT ?? '18091';
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
			command: `node e2e/backend.mjs reset-serve ${DB}`,
			url: `http://127.0.0.1:${BACKEND_PORT}/healthz`,
			env: { OX_BACKEND_PORT: String(BACKEND_PORT), OX_SPA_ORIGIN: WEB, OX_FAKE_AI_PORT: FAKE_AI_PORT },
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

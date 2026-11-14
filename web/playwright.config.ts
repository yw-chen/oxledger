// SPDX-License-Identifier: GPL-2.0-or-later
// E2E against the real backend (playbook): its own database oxledger_fe_e2e
// (reset on every run), backend on 18082, the SPA on 5176 and the stand-in
// model (e2e/fakeai.mjs) on 18091, so the dev preview (18080/5174) keeps
// running. A second run beside this one (a verifier) sets its own:
//   OX_E2E_DB=oxledger_fe_e2e_verify OX_E2E_BACKEND_PORT=18083 OX_E2E_WEB_PORT=5177 OX_FAKE_AI_PORT=18092
// Build the backend first:
//   cargo build -p oxledger --features test-support,ai
import { defineConfig } from '@playwright/test';
import { fileURLToPath } from 'node:url';

const DB = process.env.OX_E2E_DB ?? 'oxledger_fe_e2e';
const BACKEND_PORT = Number(process.env.OX_E2E_BACKEND_PORT ?? 18082);
const WEB_PORT = Number(process.env.OX_E2E_WEB_PORT ?? 5176);
const FAKE_AI_PORT = process.env.OX_FAKE_AI_PORT ?? '18091';
const WEB = `http://localhost:${WEB_PORT}`;
// D11: the backend log (RUST_LOG=info) for the whole run; a failing test gets
// its own part of it and a /metrics snapshot attached (e2e/flows.spec.ts).
// Outside outputDir, which Playwright empties at the start of a run.
export const BACKEND_LOG = fileURLToPath(new URL(`./test-results/backend-${DB}.log`, import.meta.url));
export const METRICS_TOKEN = `e2e-metrics-${DB}`; // test value, local backend only

export default defineConfig({
	testDir: 'e2e',
	testMatch: '*.spec.ts',
	outputDir: 'test-results/artifacts',
	fullyParallel: false,
	workers: 1,
	timeout: 60_000,
	// G10: aria snapshots live at e2e/__snapshots__/<spec>/<name>.aria.yml. A
	// missing one is written on a local run; `npm run e2e:ci` (merge) passes
	// --update-snapshots=none so it fails instead. Playwright does not notice an
	// orphaned .aria.yml: delete the file with its test.
	expect: { timeout: 15_000, toMatchAriaSnapshot: { pathTemplate: '{testDir}/__snapshots__/{testFilePath}/{arg}{ext}' } },
	reporter: [['list']],
	use: { baseURL: WEB, locale: 'zh-TW', timezoneId: 'Asia/Taipei', trace: 'retain-on-failure', screenshot: 'only-on-failure' },
	webServer: [
		{
			command: `node e2e/backend.mjs reset-serve ${DB}`,
			url: `http://127.0.0.1:${BACKEND_PORT}/healthz`,
			env: { OX_BACKEND_PORT: String(BACKEND_PORT), OX_SPA_ORIGIN: WEB, OX_FAKE_AI_PORT: FAKE_AI_PORT, OX_BACKEND_LOG: BACKEND_LOG, OXLEDGER_METRICS_TOKEN: METRICS_TOKEN, RUST_LOG: process.env.RUST_LOG ?? 'info' },
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

export const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}`;
export const MCP_URL = `http://127.0.0.1:${BACKEND_PORT}/mcp`;

// SPDX-License-Identifier: GPL-2.0-or-later
import { sveltekit } from '@sveltejs/kit/vite';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';

// The backend (oxledger serve) must have OXLEDGER_SPA_ORIGIN equal to the
// origin the browser uses here (http://localhost:5174), because every
// non-GET /api/v1 request is checked against it. The proxy keeps Origin.
const env = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env ?? {};
const backend = env.OX_BACKEND ?? 'http://127.0.0.1:18080';

export default defineConfig({
	plugins: [sveltekit(), svelteTesting()],
	server: { proxy: { '/api': { target: backend, changeOrigin: false }, '/mcp': { target: backend, changeOrigin: false } }, fs: { allow: ['..'] } },
	preview: { proxy: { '/api': { target: backend, changeOrigin: false }, '/mcp': { target: backend, changeOrigin: false } } },
	test: {
		environment: 'jsdom',
		include: ['src/**/*.test.ts']
	}
});

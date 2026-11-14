// SPDX-License-Identifier: GPL-2.0-or-later
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';

// The backend (oxledger serve) must have OXLEDGER_SPA_ORIGIN equal to the
// origin the browser uses here (http://localhost:5174), because every
// non-GET /api/v1 request is checked against it. The proxy keeps Origin.
const env = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env ?? {};
const backend = env.OX_BACKEND ?? 'http://127.0.0.1:18080';
// ../api as a file path (no @types/node here, so no node:url): drop the
// leading slash before a Windows drive letter.
const apiDir = decodeURIComponent(new URL('../api', import.meta.url).pathname).replace(/^\/(?=[A-Za-z]:)/, '');

export default defineConfig({
	plugins: [
		// SvelteKit 3: svelte.config.js is gone; its options are the plugin's.
		sveltekit({
			preprocess: vitePreprocess(),
			// SPA (D6): one index.html for every route; the server only serves files.
			adapter: adapter({ fallback: 'index.html' })
		}),
		svelteTesting()
	],
	// $api is the generated contract in ../api (outside this package, so it
	// cannot be a package.json subpath import like #lib). Kit's `alias` option
	// is deprecated in 3.0; the TypeScript side is `paths` in tsconfig.json.
	resolve: { alias: { $api: apiDir } },
	server: { proxy: { '/api': { target: backend, changeOrigin: false }, '/mcp': { target: backend, changeOrigin: false } }, fs: { allow: ['..'] } },
	preview: { proxy: { '/api': { target: backend, changeOrigin: false }, '/mcp': { target: backend, changeOrigin: false } } },
	test: {
		environment: 'jsdom',
		include: ['src/**/*.test.ts']
	}
});

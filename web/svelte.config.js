// SPDX-License-Identifier: GPL-2.0-or-later
import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
export default {
	preprocess: vitePreprocess(),
	kit: {
		// SPA (D6): one index.html for every route; the server only serves files.
		adapter: adapter({ fallback: 'index.html' }),
		alias: { $api: '../api' }
	}
};

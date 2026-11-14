// ESLint 最小規則集（研究 07 §6.1、決定 D5）：typescript-eslint 與
// eslint-plugin-svelte 的 recommended，再加 R-M10 的 {@html} 限制。
// 依測試規則只對改到的檔案跑：npx eslint <檔案…>
import ts from 'typescript-eslint';
import svelte from 'eslint-plugin-svelte';
import svelteConfig from './svelte.config.js';

export default ts.config(
	{ ignores: ['.svelte-kit/', 'build/', 'node_modules/', 'test-results/', 'playwright-report/', 'src/api/'] },
	...ts.configs.recommended,
	...svelte.configs.recommended,
	{
		files: ['**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
		languageOptions: { parserOptions: { parser: ts.parser, extraFileExtensions: ['.svelte'], svelteConfig } }
	},
	// R-M10：{@html} 只准出現在 RichText（伺服器的 *_html）與 AiChat（自己的子集，一律 escape）。
	// 「只能傳 *_html」由 src/lib/html-guard.test.ts 檢查。
	{
		rules: {
			'svelte/no-at-html-tags': 'error',
			// SPA 沒有 paths.base（svelte.config.js），連結不需要 resolve()。
			'svelte/no-navigation-without-resolve': 'off',
			// 會誤報 $derived 裡算完就回傳的區域 Map／Set。
			'svelte/prefer-svelte-reactivity': 'off'
		}
	},
	// E2E 用 any 接任意 JSON 回應。
	{ files: ['e2e/**'], rules: { '@typescript-eslint/no-explicit-any': 'off' } },
	{
		files: ['src/lib/components/RichText.svelte', 'src/lib/components/AiChat.svelte'],
		rules: { 'svelte/no-at-html-tags': 'off' }
	}
);

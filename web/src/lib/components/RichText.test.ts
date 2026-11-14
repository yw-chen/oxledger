import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import RichText from './RichText.svelte';

const html =
	'<p><a class="ox-ext" href="https://evil.example/x" data-ox-host="evil.example">看這裡</a> ' +
	'<a class="ox-img-ext" href="https://img.example/a.png">圖</a> <a class="ox-issue" href="/issue/A-1" title="登入失敗">A-1</a></p>';

describe('RichText (§6.1, R-M3)', () => {
	it('agent content: external links lose href and open only after a confirmation', async () => {
		render(RichText, { html, agent: true, testid: 'rt' });
		const ext = screen.getByText('看這裡');
		const img = screen.getByText('圖');
		expect(ext.getAttribute('href')).toBe('#');
		expect(ext.dataset.oxHref).toBe('https://evil.example/x');
		expect(img.getAttribute('href')).toBe('#');
		expect(screen.getByText('A-1').getAttribute('href')).toBe('/issue/A-1');
		expect(screen.getByText('A-1').getAttribute('aria-label')).toBe('A-1 登入失敗');

		await fireEvent.click(img);
		expect(screen.getByTestId('rt-leave').textContent).toContain('img.example');
		await fireEvent.click(screen.getByText('取消'));
		await fireEvent.click(ext);
		expect(screen.getByTestId('rt-leave').textContent).toContain('evil.example');
	});

	it('a person’s content keeps its links; no HTML shows the text', () => {
		const { unmount } = render(RichText, { html, testid: 'rt' });
		expect(screen.getByText('看這裡').getAttribute('href')).toBe('https://evil.example/x');
		unmount();
		render(RichText, { html: null, text: '<b>不是 HTML</b>', testid: 'rt' });
		expect(screen.getByTestId('rt').textContent).toBe('<b>不是 HTML</b>');
	});
});

// SPDX-License-Identifier: GPL-2.0-or-later
import { afterEach, describe, expect, it } from 'vitest';
import { ApiError } from './api.js';
import { fieldLabel, importFileProblem, mappingProblem, progress, rowErrorText, setImportTransport, stepOf, templateCsv, uploadImport, working } from './imports.js';

const err = (status: number, code: string, details: unknown = {}) =>
	new ApiError(status, { code, details, message: 'm', next_actions: [], retryable: false, request_id: 'r' } as never, null);

afterEach(() => setImportTransport(null));

describe('import wizard helpers', () => {
	it('opens an import at the step its state calls for', () => {
		expect(stepOf('uploaded')).toBe(1);
		expect(stepOf('ready')).toBe(2);
		expect(stepOf('validating')).toBe(4);
		expect(stepOf('validated')).toBe(4);
		for (const s of ['running', 'done', 'canceled', 'failed', 'expired']) expect(stepOf(s)).toBe(5);
		expect(working({ state: 'running' })).toBe(true);
		expect(working({ state: 'validated' })).toBe(false);
		expect(progress({ done: 5, row_count: 20 })).toBe(0.25);
		expect(progress({ done: 0, row_count: 0 })).toBe(0);
	});

	it('writes a template with a BOM and only a header row', () => {
		const t = templateCsv('issues');
		expect(t.charCodeAt(0)).toBe(0xfeff);
		expect(t.slice(1)).toBe('標題,追蹤類型,狀態,優先度,負責人,里程碑,開始日,到期日,預估工時,說明\r\n');
		expect(templateCsv('time_entries').slice(1)).toBe('日期,時數,單號,活動,使用者,說明\r\n');
	});

	it('says what is wrong with a file', () => {
		expect(importFileProblem(err(422, 'import_invalid_file', { kind: 'encoding', row: 3 }))).toContain('第 3 列');
		expect(importFileProblem(err(422, 'import_invalid_file', { kind: 'too_many_rows' }))).toContain('5,000');
		expect(importFileProblem(err(422, 'import_invalid_file', { kind: 'no_header' }))).toBe('第一列必須是欄位名稱。');
		expect(importFileProblem(err(413, 'payload_too_large'))).toContain('10 MB');
		// A proxy's 413 has no error document.
		expect(importFileProblem(new ApiError(413, null, null, 'HTTP 413'))).toContain('10 MB');
		expect(importFileProblem(err(404, 'not_found'))).toBeNull();
		expect(importFileProblem(new Error('x'))).toBeNull();
	});

	it('words row errors and mapping problems by code, never by content', () => {
		expect(rowErrorText('unknown_value')).toBe('找不到對應的值');
		expect(rowErrorText('some_new_code')).toBe('無法匯入（some_new_code）');
		expect(rowErrorText(null)).toBe('');
		const fields = [{ key: 'subject', label: '標題', kind: 'text', required: true }];
		expect(fieldLabel(fields, 'subject')).toBe('標題');
		expect(fieldLabel(fields, 'cf:9')).toBe('cf:9');
		expect(mappingProblem({ field: 'subject', reason: 'required' }, (k) => fieldLabel(fields, k))).toContain('「標題」是必填');
		expect(mappingProblem({ reason: 'whatever' }, (k) => k)).toBe('欄位對應有問題，請檢查後再試');
	});

	it('uploads the raw file and turns a refusal into an ApiError', async () => {
		const seen: { url: string; headers: Record<string, string> }[] = [];
		setImportTransport(async (url, _file, headers, onProgress) => {
			seen.push({ url, headers });
			onProgress(1);
			return seen.length === 1 ? { status: 201, json: { data: { id: 'i1', state: 'uploaded' } } } : { status: 422, json: { error: { code: 'import_invalid_file', details: { kind: 'empty' }, message: 'm' } } };
		});
		let p = 0;
		const d = await uploadImport('WEB', 'time_entries', new Blob(['a']), (x) => (p = x));
		expect(d.id).toBe('i1');
		expect(p).toBe(1);
		expect(seen[0].url).toBe('/api/v1/projects/WEB/imports?kind=time_entries');
		expect(seen[0].headers['Content-Type']).toBe('application/octet-stream');
		await expect(uploadImport('WEB', 'issues', new Blob(['a']))).rejects.toMatchObject({ code: 'import_invalid_file' });
	});
});

import { afterEach, describe, expect, it } from 'vitest';
import { ApiError, setCsrf } from './api';
import {
	attachDoc,
	deleteDoc,
	encodeName,
	iconFor,
	insertAt,
	linked,
	markdownFor,
	runUploads,
	setUploadTransport,
	sizeText,
	split,
	uploadErrorText,
	uploadFile,
	type UploadRow,
	type UploadTransport
} from './attachments';

afterEach(() => {
	setUploadTransport(null);
	setCsrf(null);
});

const view = (name: string, type = 'image/png') => ({ upload_id: `u-${name}`, filename: name, content_type: type, size: 3, expires_at: '' });

describe('markdown for an upload', () => {
	it('images get !, other files a link', () => {
		expect(markdownFor(view('a.png'))).toBe('![a.png](attachment:a.png)');
		expect(markdownFor(view('a.zip', 'application/octet-stream'))).toBe('[a.zip](attachment:a.zip)');
	});
	it('names that would break the link are encoded, the rest kept', () => {
		expect(encodeName('my file (1).png')).toBe('my%20file%20%281%29.png');
		expect(encodeName('設計圖.png')).toBe('設計圖.png');
		expect(encodeName('100%.png')).toBe('100%25.png');
		expect(markdownFor(view('[x].png'))).toBe('![\\[x\\].png](attachment:%5Bx%5D.png)');
	});
	it('goes in on its own line at the caret', () => {
		expect(insertAt('', 0, 'X')).toEqual({ text: 'X', caret: 1 });
		expect(insertAt('ab', 1, 'X')).toEqual({ text: 'a\nX\nb', caret: 4 });
		expect(insertAt('a\n', 2, 'X')).toEqual({ text: 'a\nX', caret: 3 });
	});
});

describe('which uploads go with the text', () => {
	it('only those whose link is still there', () => {
		const ups = [view('a b.png'), view('gone.png'), view('x.zip', 'application/zip')];
		const text = `hi ${markdownFor(ups[0])} and ${markdownFor(ups[2])}`;
		expect(linked(ups, text).map((u) => u.filename)).toEqual(['a b.png', 'x.zip']);
		expect(linked(ups, '')).toEqual([]);
		expect(linked(ups, '![t](attachment:gone.png "說明")').map((u) => u.filename)).toEqual(['gone.png']);
		expect(linked(ups, '(attachment:gone.png.bak)')).toEqual([]);
	});
});

describe('display', () => {
	it('sizes', () => {
		expect(sizeText(12)).toBe('12 B');
		expect(sizeText(1536)).toBe('1.5 KB');
		expect(sizeText(20 * 1024 * 1024)).toBe('20 MB');
	});
	it('images apart from files; redacted ones are files', () => {
		const a = (id: string, t: string, redacted = false) => ({ id, filename: id, content_type: t, size: 1, author: 'a', created_at: '', redacted });
		const s = split([a('1', 'image/png'), a('2', 'application/octet-stream'), a('3', 'image/gif', true)]);
		expect(s.images.map((x) => x.id)).toEqual(['1']);
		expect(s.files.map((x) => x.id)).toEqual(['2', '3']);
		expect(iconFor({ filename: 'x.PDF', content_type: 'application/octet-stream' })).toBe('📕');
	});
	it('documents', () => {
		expect(attachDoc({ issue: 'AUTH-1' }, ['u1'], 'op')).toEqual({ command_schema_version: 1, ops: [{ kind: 'attach', op_id: 'op', target: { issue: 'AUTH-1' }, attachments: [{ upload_id: 'u1' }] }] });
		expect(deleteDoc('a1', 'op').ops[0]).toEqual({ kind: 'delete_attachment', op_id: 'op', target: { attachment: 'a1' } });
	});
});

describe('uploads', () => {
	it('sends the raw file with the CSRF token and the name in the query', async () => {
		let seen: { url: string; headers: Record<string, string> } | null = null;
		setCsrf('tok');
		setUploadTransport(async (url, _f, headers, onProgress) => {
			seen = { url, headers };
			onProgress(0.5);
			return { status: 201, json: { data: view('a b.png') } };
		});
		const steps: number[] = [];
		const r = await uploadFile('P1', new Blob(['x']), 'a b.png', (p) => steps.push(p));
		expect(r.upload_id).toBe('u-a b.png');
		expect(seen!.url).toBe('/api/v1/projects/P1/uploads?filename=a%20b.png');
		expect(seen!.headers['Content-Type']).toBe('application/octet-stream');
		expect(seen!.headers['X-CSRF-Token']).toBe('tok');
		expect(steps).toEqual([0.5]);
	});
	it('errors become §6.2 texts', async () => {
		const fail = (status: number, error: unknown): UploadTransport => async () => ({ status, json: { error } });
		setUploadTransport(fail(413, { code: 'payload_too_large', message: 'm', details: { limit: 20 * 1024 * 1024, kind: 'file' } }));
		await expect(uploadFile('P', new Blob([]), 'x')).rejects.toBeInstanceOf(ApiError);
		const text = async () => uploadErrorText(await uploadFile('P', new Blob([]), 'x').catch((e) => e));
		expect(await text()).toBe('檔案超過 20 MB');
		setUploadTransport(fail(413, { code: 'payload_too_large', message: 'm', details: { limit: 1, kind: 'daily' } }));
		expect(await text()).toBe('今天的上傳量已達上限');
		setUploadTransport(fail(507, { code: 'insufficient_storage', message: 'm' }));
		expect(await text()).toBe('伺服器儲存空間不足，請通知管理者');
		setUploadTransport(fail(429, { code: 'rate_limited', message: 'm' }));
		expect(await text()).toBe('同時上傳太多，請稍候再試');
		setUploadTransport(async () => ({ status: 413, json: null }));
		expect(await text()).toBe('檔案太大，伺服器拒收');
	});
	it('at most three at once, in order; a failure does not stop the rest', async () => {
		let running = 0;
		let most = 0;
		const order: string[] = [];
		setUploadTransport(async (url) => {
			running++;
			most = Math.max(most, running);
			await new Promise((r) => setTimeout(r, 5));
			running--;
			const name = decodeURIComponent(url.split('filename=')[1]);
			order.push(name);
			return name === 'bad' ? { status: 507, json: { error: { code: 'insufficient_storage', message: 'm' } } } : { status: 201, json: { data: view(name) } };
		});
		const names = ['1', '2', 'bad', '4', '5'];
		const files = names.map((n, i) => ({ file: new Blob([n]), row: { id: i, name: n, size: 1, progress: 0, state: 'queued' } as UploadRow }));
		await runUploads('P', files, () => {});
		expect(most).toBe(3);
		expect(order.length).toBe(5);
		expect(files.map((f) => f.row.state)).toEqual(['done', 'done', 'failed', 'done', 'done']);
		expect(files[2].row.error).toBe('伺服器儲存空間不足，請通知管理者');
	});
});

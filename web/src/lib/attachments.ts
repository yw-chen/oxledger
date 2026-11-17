// SPDX-License-Identifier: GPL-2.0-or-later
// Attachments in the web app (design 06 §6.2 「附檔」, §6.8 附件卡, R-A1–R-A9):
// uploads (raw body, progress, at most 3 at a time), the Markdown a file is
// inserted as, the command documents, and the texts people see.

import type { AttachmentCommandDoc, AttachmentMeta, AttachTarget, UploadView } from '$api/types';
import { ApiError, csrf, recoverCsrf, sessionEnded } from './api';

/** §6.2: at most 3 uploads at once; the rest wait (the app queues, not the API). */
export const UPLOAD_CONCURRENCY = 3;
/** R-A7: one `attach` (or `attachments` parameter) takes 1–10 files. */
export const MAX_PER_OP = 10;

/** One upload as its row shows it. */
export type UploadRow = {
	id: number;
	name: string;
	size: number;
	/** 0–1 while sending. */
	progress: number;
	state: 'queued' | 'sending' | 'done' | 'failed';
	error?: string;
	result?: UploadView;
};

/** Sends one file; tests replace it. `onProgress` gets 0–1. */
export type UploadTransport = (url: string, file: Blob, headers: Record<string, string>, onProgress: (p: number) => void, signal?: AbortSignal) => Promise<{ status: number; json: unknown }>;

const xhrTransport: UploadTransport = (url, file, headers, onProgress, signal) =>
	new Promise((resolve, reject) => {
		const x = new XMLHttpRequest();
		x.open('POST', url);
		x.withCredentials = true;
		for (const [k, v] of Object.entries(headers)) x.setRequestHeader(k, v);
		x.upload.onprogress = (e) => {
			if (e.lengthComputable && e.total > 0) onProgress(e.loaded / e.total);
		};
		x.onload = () => {
			let json: unknown = null;
			try {
				json = JSON.parse(x.responseText);
			} catch {
				/* not JSON */
			}
			resolve({ status: x.status, json });
		};
		x.onerror = () => reject(new ApiError(0, null, null, '無法連線'));
		x.onabort = () => reject(new DOMException('aborted', 'AbortError'));
		signal?.addEventListener('abort', () => x.abort());
		x.send(file);
	});

let transport: UploadTransport = xhrTransport;
/** Tests replace the transport. */
export function setUploadTransport(t: UploadTransport | null) {
	transport = t ?? xhrTransport;
}

/** `POST /projects/{id}/uploads?filename=`: the raw file (R-A1). */
export async function uploadFile(project: string, file: Blob, name: string, onProgress: (p: number) => void = () => {}, signal?: AbortSignal): Promise<UploadView> {
	const url = `/api/v1/projects/${encodeURIComponent(project)}/uploads?filename=${encodeURIComponent(name)}`;
	const once = async () => {
		const headers: Record<string, string> = { Accept: 'application/json', 'Content-Type': 'application/octet-stream' };
		const token = csrf();
		if (token) headers['X-CSRF-Token'] = token;
		const r = await transport(url, file, headers, onProgress, signal);
		const body = r.json as { data?: UploadView; error?: ConstructorParameters<typeof ApiError>[1] } | null;
		if (r.status >= 200 && r.status < 300 && body?.data) return body.data;
		return new ApiError(r.status, body?.error ?? null, null, `HTTP ${r.status}`);
	};
	// As `api()` does: a replaced session gets one more try with the new token;
	// an ended one is reported.
	let r = await once();
	if (r instanceof ApiError && r.code === 'csrf_failed' && (await recoverCsrf())) r = await once();
	if (r instanceof ApiError) {
		if (r.code === 'unauthenticated') sessionEnded();
		throw r;
	}
	return r;
}

/** §6.2's texts for a failed upload. */
export function uploadErrorText(e: unknown): string {
	if (!(e instanceof ApiError)) return '上傳失敗';
	const d = e.details;
	// A proxy in front refusing the body has no error document.
	if (e.status === 413 && !e.body) return '檔案太大，伺服器拒收';
	switch (e.code) {
		case 'payload_too_large':
			if (d.kind === 'daily') return '今天的上傳量已達上限';
			if (d.kind === 'pending') return '還沒用掉的上傳太多，請先送出或等它們過期';
			return `檔案超過 ${sizeText(Number(d.limit) || 0)}`;
		case 'rate_limited':
			return '同時上傳太多，請稍候再試';
		case 'insufficient_storage':
			return '伺服器儲存空間不足，請通知管理者';
		case 'forbidden_op':
			return d.reason === 'attachments_disabled' ? '這個站台沒有開啟附件' : '你不能在這個專案上傳檔案';
		case 'invalid_argument':
		case 'invalid_value':
			return '檔名不能使用';
		case 'network':
			return '無法連線，請再試一次';
		case 'unauthenticated':
			return '登入已過期，請重新登入';
		default:
			return e.message || '上傳失敗';
	}
}

/**
 * Runs uploads with at most `limit` at once, in order of `rows`; each row's
 * state and progress change as it goes (`onChange` after each change).
 */
export async function runUploads(
	project: string,
	files: { row: UploadRow; file: Blob }[],
	onChange: () => void,
	limit = UPLOAD_CONCURRENCY
): Promise<void> {
	let next = 0;
	const worker = async () => {
		while (next < files.length) {
			const { row, file } = files[next++];
			row.state = 'sending';
			onChange();
			try {
				row.result = await uploadFile(project, file, row.name, (p) => {
					row.progress = p;
					onChange();
				});
				row.progress = 1;
				row.state = 'done';
			} catch (e) {
				row.state = 'failed';
				row.error = uploadErrorText(e);
			}
			onChange();
		}
	};
	await Promise.all(Array.from({ length: Math.min(limit, files.length) }, worker));
}

/** R-A3: the four image types are shown inline; everything else downloads. */
export function isImage(contentType: string): boolean {
	return ['image/png', 'image/jpeg', 'image/gif', 'image/webp'].includes(contentType);
}

/** The download address (R-A5, R-A6). */
export function contentUrl(id: string): string {
	return `/api/v1/attachments/${encodeURIComponent(id)}/content`;
}

/** 「1.2 MB」. */
export function sizeText(bytes: number): string {
	if (bytes < 1024) return `${bytes} B`;
	const units = ['KB', 'MB', 'GB'];
	let v = bytes / 1024;
	let i = 0;
	while (v >= 1024 && i < units.length - 1) {
		v /= 1024;
		i++;
	}
	return `${v >= 10 ? Math.round(v) : Math.round(v * 10) / 10} ${units[i]}`;
}

/** A file name as a Markdown link destination: only what would end or break
 *  the destination is percent-encoded (the server decodes, R-M4). */
export function encodeName(name: string): string {
	return name.replace(/[%\s()<>\\[\]]/g, (c) => encodeURIComponent(c).replace(/[()]/g, (p) => (p === '(' ? '%28' : '%29')));
}

/** Text in a link label: brackets and backslashes escaped. */
function label(name: string): string {
	return name.replace(/[\\[\]]/g, (c) => `\\${c}`);
}

/** §6.2: `![name](attachment:name)` for an image, `[name](attachment:name)` otherwise. */
export function markdownFor(u: Pick<UploadView, 'filename' | 'content_type'>): string {
	const link = `[${label(u.filename)}](attachment:${encodeName(u.filename)})`;
	return isImage(u.content_type) ? `!${link}` : link;
}

/**
 * The uploads whose link is still in `text`: a file whose link was deleted
 * from the editor (or that finished after the text was cleared) is not sent.
 */
export function linked<U extends Pick<UploadView, 'filename'>>(uploads: U[], text: string): U[] {
	return uploads.filter((u) => {
		const at = `(attachment:${encodeName(u.filename)}`;
		// The destination ends at `)` or at the space before a title.
		return text.includes(`${at})`) || text.includes(`${at} `);
	});
}

/** §6.2's text when one save would carry more than R-A7's 10 files. */
export const TOO_MANY = `一次最多附加 ${MAX_PER_OP} 個檔案；請分兩次送出`;

/** Inserts `snippet` at the caret (own line when the caret is mid-line). */
export function insertAt(text: string, caret: number, snippet: string): { text: string; caret: number } {
	const before = text.slice(0, caret);
	const after = text.slice(caret);
	const lead = before && !before.endsWith('\n') ? '\n' : '';
	const tail = after.startsWith('\n') || !after ? '' : '\n';
	const ins = `${lead}${snippet}${tail}`;
	return { text: before + ins + after, caret: before.length + ins.length };
}

/** An `attach` document (R-A7): 1–10 uploads onto an issue, a note or a page. */
export function attachDoc(target: AttachTarget, uploads: string[], opId: string): AttachmentCommandDoc {
	return {
		command_schema_version: 1,
		ops: [{ kind: 'attach', op_id: opId, target, attachments: uploads.map((upload_id) => ({ upload_id })) }]
	};
}

/** A `delete_attachment` document (R-A7). */
export function deleteDoc(id: string, opId: string): AttachmentCommandDoc {
	return {
		command_schema_version: 1,
		ops: [{ kind: 'delete_attachment', op_id: opId, target: { attachment: id } }]
	};
}

/** Images first (a thumbnail grid), then the other files, each oldest first. */
export function split(list: AttachmentMeta[]): { images: AttachmentMeta[]; files: AttachmentMeta[] } {
	const images = list.filter((a) => !a.redacted && isImage(a.content_type));
	return { images, files: list.filter((a) => !images.includes(a)) };
}

/** The type icon of a file (§6.8 「圖示依類型」). */
export function iconFor(a: Pick<AttachmentMeta, 'content_type' | 'filename'>): string {
	if (isImage(a.content_type)) return '🖼';
	const ext = a.filename.toLowerCase().split('.').pop() ?? '';
	if (['pdf'].includes(ext)) return '📕';
	if (['zip', '7z', 'gz', 'tar', 'rar'].includes(ext)) return '🗜';
	if (['txt', 'md', 'log', 'csv'].includes(ext)) return '📄';
	if (['doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'odt', 'ods'].includes(ext)) return '📊';
	return '📎';
}

/** R-A7 undo reasons. */
export const UNDO_REASON: Record<string, string> = {
	expired: '超過 30 天，已不能復原',
	changed_since: '之後又被放回或刪除過',
	redacted: '內容已遮蔽',
	content_gone: '檔案已清除',
	container_gone: '所在的議題、note 或頁已不存在',
	project_not_writable: '專案已封存',
	permission: '你目前沒有權限'
};

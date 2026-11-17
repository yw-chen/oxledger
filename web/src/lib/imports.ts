// SPDX-License-Identifier: GPL-2.0-or-later
// CSV import in the web app (design 06 §3.10, §6.12): the upload (a raw
// body, so not through `api()`), the wizard's steps, and the texts people
// see for a file that cannot be read and for a row that failed.

import type { ImportDetail, ImportField, ImportStatus } from '$api/types';
import { ApiError, csrf, recoverCsrf, sessionEnded } from './api.js';

export type ImportKind = 'issues' | 'time_entries';

export const KIND_LABEL: Record<ImportKind, string> = { issues: '議題', time_entries: '工時' };

export const ENCODINGS: { value: string; label: string }[] = [
	{ value: 'utf-8', label: 'UTF-8' },
	{ value: 'big5', label: 'Big5' },
	{ value: 'utf-16le', label: 'UTF-16LE' },
	{ value: 'utf-16be', label: 'UTF-16BE' }
];
export const DELIMITERS: { value: string; label: string }[] = [
	{ value: ',', label: '逗號' },
	{ value: ';', label: '分號' },
	{ value: '\t', label: 'Tab' }
];
export const DATE_FORMATS: { value: string; label: string }[] = [
	{ value: 'ymd', label: '年-月-日（2026-11-05）' },
	{ value: 'mdy', label: '月/日/年（11/05/2026）' },
	{ value: 'dmy', label: '日/月/年（05/11/2026）' }
];

export const STEPS = ['上傳', '檔案設定', '欄位對應', '值對應', '試跑', '執行與結果'] as const;

/** The step an import opens at when one comes back to it (§6.12「離開與回來」). */
export function stepOf(state: string): number {
	switch (state) {
		case 'uploaded':
			return 1;
		case 'ready':
			return 2;
		case 'validating':
		case 'validated':
			return 4;
		default:
			return 5;
	}
}

export const STATE_LABEL: Record<string, string> = {
	uploaded: '已上傳',
	ready: '設定中',
	validating: '試跑中',
	validated: '已試跑',
	running: '匯入中',
	done: '已完成',
	canceled: '已停止',
	failed: '失敗',
	expired: '已過期'
};

/** The header row of a template file (§6.12 步驟 1「下載範本」), with the BOM Excel wants. */
export function templateCsv(kind: ImportKind): string {
	const header = kind === 'issues' ? ['標題', '追蹤類型', '狀態', '優先度', '負責人', '里程碑', '開始日', '到期日', '預估工時', '說明'] : ['日期', '時數', '單號', '活動', '使用者', '說明'];
	return `﻿${header.join(',')}\r\n`;
}

/** §6.12 步驟 1: what is wrong with the file, under the drop zone; null: the ordinary error box. */
export function importFileProblem(e: unknown): string | null {
	if (!(e instanceof ApiError)) return null;
	const d = e.details ?? {};
	// A proxy in front refusing the body has no error document.
	if (e.status === 413 || e.code === 'payload_too_large') return '檔案超過 10 MB，請分成幾個檔案再匯入。';
	if (e.code === 'forbidden_op') return '你沒有在這個專案匯入的權限。';
	if (e.code !== 'import_invalid_file') return null;
	const row = Number(d.row);
	const at = Number.isFinite(row) && row > 0 ? `（第 ${row} 列附近）` : '';
	switch (d.kind) {
		case 'encoding':
			return `無法判斷檔案的編碼${at}。請用 UTF-8 或 Big5 另存後再上傳。`;
		case 'too_many_rows':
			return '超過 5,000 列，請分成幾個檔案再匯入。';
		case 'too_many_columns':
			return '超過 64 個欄位，請刪掉不需要的欄位。';
		case 'cell_too_large':
			return `有一格的內容太長${at}，一格最多 64 KB。`;
		case 'no_header':
			return '第一列必須是欄位名稱。';
		case 'empty':
			return '檔案裡沒有資料列（第一列是欄位名稱，之後才是資料）。';
		case 'parse':
			return `檔案格式有問題${at}，請確認引號是否成對。`;
		default:
			return '這個檔案無法匯入。';
	}
}

/** What a settings change was refused for (`import_mapping_invalid.details`). */
export function mappingProblem(d: Record<string, unknown>, labelOf: (key: string) => string): string {
	const f = typeof d.field === 'string' ? d.field : '';
	switch (d.reason) {
		case 'mapped_twice':
			return `「${labelOf(f)}」被對應了兩次，一個欄位只能對應一個檔案欄`;
		case 'required':
			return `「${labelOf(f)}」是必填，請對應一個檔案欄`;
		case 'unknown_field':
			return '有一個欄位不存在，或你不能填寫；請重新選擇';
		case 'unknown_value':
			return '預設的追蹤類型不在這個專案裡';
		default:
			return '欄位對應有問題，請檢查後再試';
	}
}

const ROW_ERROR: Record<string, string> = {
	unknown_value: '找不到對應的值',
	ambiguous_value: '有好幾個同名的對象，請在「值對應」指定',
	bad_date: '日期格式不對',
	bad_number: '不是數字，或超出範圍',
	bad_boolean: '要填「是」或「否」',
	unknown_field: '這個欄位不存在，或你不能填寫',
	required_field_missing: '必填欄位沒有填',
	forbidden_op: '你沒有做這件事的權限',
	forbidden_field: '你不能填寫這個欄位',
	workflow_disallowed: '不能直接建立成這個狀態（工作流程不允許）',
	invalid_value: '值不符合規則',
	invalid_argument: '值不符合規則',
	not_found: '找不到對應的對象',
	form_rules_changed: '表單規則剛改過，請重新試跑',
	version_closed: '里程碑已關閉',
	time_tracking_disabled: '這個專案沒有啟用工時'
};

/** One failed row as the table shows it; an unknown code is shown as it is. */
export function rowErrorText(code: string | null | undefined): string {
	if (!code) return '';
	return ROW_ERROR[code] ?? `無法匯入（${code}）`;
}

/** The label of a field key, by the fields the import offers. */
export function fieldLabel(fields: ImportField[], key: string | null | undefined): string {
	if (!key) return '';
	return fields.find((f) => f.key === key)?.label ?? key;
}

export type UploadTransport = (url: string, file: Blob, headers: Record<string, string>, onProgress: (p: number) => void) => Promise<{ status: number; json: unknown }>;

const xhrTransport: UploadTransport = (url, file, headers, onProgress) =>
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
		x.send(file);
	});

let transport: UploadTransport = xhrTransport;
/** Tests replace the transport. */
export function setImportTransport(t: UploadTransport | null) {
	transport = t ?? xhrTransport;
}

/** `POST /projects/{id}/imports?kind=`: the raw file. */
export async function uploadImport(project: string, kind: ImportKind, file: Blob, onProgress: (p: number) => void = () => {}): Promise<ImportDetail> {
	const url = `/api/v1/projects/${encodeURIComponent(project)}/imports?kind=${kind}`;
	const once = async () => {
		const headers: Record<string, string> = { Accept: 'application/json', 'Content-Type': 'application/octet-stream' };
		const token = csrf();
		if (token) headers['X-CSRF-Token'] = token;
		const r = await transport(url, file, headers, onProgress);
		const body = r.json as { data?: ImportDetail; error?: ConstructorParameters<typeof ApiError>[1] } | null;
		if (r.status >= 200 && r.status < 300 && body?.data) return body.data;
		return new ApiError(r.status, body?.error ?? null, null, `HTTP ${r.status}`);
	};
	let r = await once();
	if (r instanceof ApiError && r.code === 'csrf_failed' && (await recoverCsrf())) r = await once();
	if (r instanceof ApiError) {
		if (r.code === 'unauthenticated') sessionEnded();
		throw r;
	}
	return r;
}

/** Still being worked on by the server: poll it. */
export const working = (s: Pick<ImportStatus, 'state'> | null | undefined) => s?.state === 'validating' || s?.state === 'running';

/** Rows done of rows in all, 0–1. */
export function progress(s: Pick<ImportStatus, 'done' | 'row_count'>): number {
	return s.row_count > 0 ? Math.min(1, s.done / s.row_count) : 0;
}

// CSV export (design 06 §3.9, §6.11): the file comes back as the response
// body, not as JSON, so it is fetched here rather than through `api()`.
// A response cut by the server (over 50 MB) fails while reading the body:
// nothing incomplete is ever saved.
import type { ErrorBody } from '$api/types';
import { ApiError, csrf, recoverCsrf, sessionEnded } from './api.js';

/** The built-in columns an issue export can hold, with the names the list shows. */
export const EXPORT_COLUMNS: { key: string; label: string }[] = [
	{ key: 'key', label: '單號' },
	{ key: 'subject', label: '標題' },
	{ key: 'project', label: '專案' },
	{ key: 'tracker', label: '追蹤類型' },
	{ key: 'status', label: '狀態' },
	{ key: 'status_category', label: '狀態分類' },
	{ key: 'triage', label: '分流' },
	{ key: 'assignee', label: '負責人' },
	{ key: 'author', label: '建立者' },
	{ key: 'priority', label: '優先度' },
	{ key: 'fixed_version', label: '里程碑' },
	{ key: 'is_private', label: '私有' },
	{ key: 'start_date', label: '開始日' },
	{ key: 'due_date', label: '到期日' },
	{ key: 'created_at', label: '建立時間' },
	{ key: 'updated_at', label: '更新時間' },
	{ key: 'estimated_hours', label: '預估工時' },
	{ key: 'spent_hours', label: '已用工時' }
];

export const DEFAULT_EXPORT_COLUMNS = ['key', 'subject', 'status', 'assignee', 'priority', 'updated_at'];

/** The stream stopped before the file was whole (R-E6: over 50 MB, or over 30 seconds). */
export class ExportCut extends Error {}

/** What the dialog says for an export that failed (§6.11); null: the ordinary error box. */
export function exportProblem(e: unknown): string | null {
	if (e instanceof ExportCut) return '下載中斷：資料量可能超過 50 MB，或處理時間過長。請減少欄位或縮小範圍後再試。';
	if (!(e instanceof ApiError)) return null;
	if (e.code === 'export_too_large') {
		const n = Number(e.details?.limit);
		return `超過 ${Number.isFinite(n) && n > 0 ? n.toLocaleString('en-US') : '10,000'} 列，請縮小篩選範圍。`;
	}
	if (e.code === 'rate_limited') return e.details?.reason === 'export_running' ? '你有一個匯出正在進行，請等它完成。' : '請稍候再匯出。';
	return null;
}

function filenameOf(resp: Response, fallback: string): string {
	const cd = resp.headers.get('Content-Disposition') ?? '';
	const star = /filename\*=UTF-8''([^;]+)/i.exec(cd);
	if (star) {
		try {
			return decodeURIComponent(star[1]);
		} catch {
			/* fall through to the plain name */
		}
	}
	const m = /filename="([^"]+)"/.exec(cd);
	return m ? m[1] : fallback;
}

async function refused(resp: Response): Promise<ApiError> {
	const err = ((await resp.json().catch(() => null)) as { error?: ErrorBody } | null)?.error ?? null;
	const e = new ApiError(resp.status, err, null, `HTTP ${resp.status}`);
	if (e.code === 'unauthenticated') sessionEnded();
	return e;
}

/** A file behind a GET (a wiki page as Markdown): fetched, then saved; throws ApiError. */
export async function downloadFile(path: string, fallback: string): Promise<void> {
	let resp: Response;
	try {
		resp = await fetch(`/api/v1${path}`, { credentials: 'same-origin' });
	} catch {
		throw new ApiError(0, null, null, '無法連線');
	}
	if (!resp.ok) throw await refused(resp);
	saveBlob(await resp.blob(), filenameOf(resp, fallback));
}

async function post(path: string, body: unknown): Promise<Response> {
	const headers: Record<string, string> = { 'Content-Type': 'application/json', Accept: 'text/csv, application/json' };
	const token = csrf();
	if (token) headers['X-CSRF-Token'] = token;
	try {
		return await fetch(`/api/v1${path}`, { method: 'POST', headers, body: JSON.stringify(body), credentials: 'same-origin' });
	} catch {
		throw new ApiError(0, null, null, '無法連線');
	}
}

/** Fetches the file; throws ApiError (refused before the first byte) or ExportCut. */
export async function fetchExport(path: '/exports/issues' | '/exports/time-entries', body: unknown): Promise<{ blob: Blob; filename: string }> {
	let resp = await post(path, body);
	if (!resp.ok) {
		let err: ErrorBody | null = null;
		try {
			err = ((await resp.json()) as { error?: ErrorBody }).error ?? null;
		} catch {
			/* not JSON */
		}
		if (err?.code === 'csrf_failed' && (await recoverCsrf())) {
			resp = await post(path, body);
			if (!resp.ok) throw new ApiError(resp.status, ((await resp.json().catch(() => null)) as { error?: ErrorBody } | null)?.error ?? null, null, `HTTP ${resp.status}`);
		} else {
			const e = new ApiError(resp.status, err, null, `HTTP ${resp.status}`);
			if (e.code === 'unauthenticated') sessionEnded();
			throw e;
		}
	}
	let blob: Blob;
	try {
		blob = await resp.blob();
	} catch {
		throw new ExportCut();
	}
	return { blob, filename: filenameOf(resp, 'export.csv') };
}

/** Hands a finished file to the browser's download. */
export function saveBlob(blob: Blob, filename: string) {
	const url = URL.createObjectURL(blob);
	const a = document.createElement('a');
	a.href = url;
	a.download = filename;
	document.body.appendChild(a);
	a.click();
	a.remove();
	setTimeout(() => URL.revokeObjectURL(url), 1000);
}

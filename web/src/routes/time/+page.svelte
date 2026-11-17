<script lang="ts">
	// 我的工時（週表）(contract 05 §6.7): rows = project / issue / activity,
	// columns = the 7 days in the org's week order. A blank cell takes hours
	// and Enter logs them; a cell with one entry edits it in place (with its
	// lock_version); a cell with several opens a small panel. Clearing asks
	// before deleting; 0 is a real entry only when the org accepts 0 hours.
	import type { Named, ProjectRef, IssueRef, Timesheet, TimesheetRow, TimeEntryView, TimelogSettings } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { submit } from '#lib/commands.js';
	import { addHours, dayLabel, deleteTimeDoc, isoWeek, logTimeDoc, parseHours, shiftWeek, showHours, todayLocal, updateTimeDoc } from '#lib/hours.js';
	import { toast } from '#lib/toast.svelte.js';
	import { exportProblem, fetchExport, saveBlob } from '#lib/exports.js';
	import ErrorBox from '#lib/components/ErrorBox.svelte';

	type Row = { project: ProjectRef; issue: IssueRef | null; activity: Named; cells: TimesheetRow['cells']; local?: boolean };

	let week = $state(isoWeek(todayLocal()));
	let sheet = $state<Timesheet | null>(null);
	let exporting = $state(false);
	/** 匯出 CSV (design 06 §6.11): my own entries of the week on screen. */
	async function exportCsv() {
		exporting = true;
		try {
			const f = await fetchExport('/exports/time-entries', { user: 'me', from: sheet?.days[0], to: sheet?.days[6] });
			saveBlob(f.blob, f.filename);
		} catch (e) {
			toast(exportProblem(e) ?? '匯出失敗，請稍後再試', 'w');
		} finally {
			exporting = false;
		}
	}
	let rules = $state<TimelogSettings | null>(null);
	let error = $state<unknown>(null);
	let cellError = $state<unknown>(null);
	let extra = $state<{ project: ProjectRef; issue: IssueRef | null; activity: Named }[]>([]);
	let seq = 0;

	async function load() {
		const s = ++seq;
		error = null;
		try {
			const t = await api('GET /me/timesheet', { query: { week } });
			if (s === seq) sheet = t;
		} catch (e) {
			if (s === seq) error = e;
		}
	}
	$effect(() => {
		void week;
		editing = null;
		panel = null;
		load();
	});
	$effect(() => {
		api('GET /org/timelog-settings')
			.then((r) => (rules = r))
			.catch(() => (rules = null));
	});

	const sameRow = (a: { project: ProjectRef; issue: IssueRef | null; activity: Named }, b: typeof a) =>
		a.project.key === b.project.key && (a.issue?.key ?? null) === (b.issue?.key ?? null) && a.activity.id === b.activity.id;
	const rows = $derived.by<Row[]>(() => {
		const days = sheet?.days ?? [];
		const out: Row[] = (sheet?.rows ?? []).map((r) => ({ ...r }));
		for (const x of extra) if (!out.some((r) => sameRow(r, x))) out.push({ ...x, cells: days.map((d) => ({ date: d, hours: null, entry_ids: [] })), local: true });
		return out;
	});
	const max = $derived(Number(sheet?.max_hours_per_day ?? 0));
	const over = (i: number) => max > 0 && Number(sheet?.day_totals[i] ?? 0) > max;

	// ---- one cell ----
	let editing = $state<{ r: number; d: number; text: string; entry: TimeEntryView | null } | null>(null);
	let panel = $state<{ r: number; d: number; entries: TimeEntryView[]; edit: Record<string, string> } | null>(null);
	let busy = $state(false);
	let opId = newOperationId();

	async function openCell(r: number, d: number) {
		cellError = null;
		panel = null;
		const c = rows[r].cells[d];
		if (c.entry_ids.length > 1) {
			try {
				const entries = await Promise.all(c.entry_ids.map((id) => api('GET /time-entries/{id}', { path: { id } })));
				panel = { r, d, entries, edit: Object.fromEntries(entries.map((e) => [e.id, showHours(e.hours)])) };
			} catch (e) {
				cellError = e;
			}
			return;
		}
		let entry: TimeEntryView | null = null;
		if (c.entry_ids.length === 1) {
			try {
				entry = await api('GET /time-entries/{id}', { path: { id: c.entry_ids[0] } });
			} catch (e) {
				cellError = e;
				return;
			}
			if (entry.can_edit === false) {
				toast('這筆工時你不能修改', 'w');
				return;
			}
		}
		editing = { r, d, text: entry ? showHours(entry.hours) : '', entry };
		opId = newOperationId();
		queueMicrotask(() => document.querySelector<HTMLInputElement>('[data-testid="cell-input"]')?.focus());
	}

	async function run(d: ReturnType<typeof logTimeDoc>, done: string) {
		busy = true;
		cellError = null;
		try {
			await submit(d, opId);
			opId = newOperationId();
			toast(done, 'ok');
			editing = null;
			panel = null;
			await load();
			return true;
		} catch (e) {
			cellError = e;
			if (!(e instanceof ApiError && e.retryable)) opId = newOperationId();
			// A stale lock_version on a time entry answers precondition_failed.
			if (e instanceof ApiError && (e.code === 'conflict' || e.code === 'precondition_failed')) {
				cellError = null;
				toast('這筆工時剛被改過，已重新載入', 'w');
				editing = null;
				panel = null;
				await load();
			}
			return false;
		} finally {
			busy = false;
		}
	}

	/** Blank or (0 when 0 is not accepted) = delete; otherwise log or update. */
	async function commit() {
		if (!editing || busy) return;
		const { r, d, text, entry } = editing;
		const row = rows[r];
		const date = sheet!.days[d];
		const blank = text.trim() === '';
		const h = blank ? null : parseHours(text);
		if (!blank && h === null) {
			cellError = null;
			toast('看不懂這個時數（可以輸入 1.5、1:30 或 1h30m）', 'w');
			return;
		}
		const zero = h !== null && Number(h) === 0;
		if (!entry) {
			if (blank) return void (editing = null);
			await run(logTimeDoc({ issueKey: row.issue?.key ?? null, projectId: row.project.id, spent_on: date, hours: h!, activity_id: row.activity.id }), `已記 ${showHours(h)} 小時`);
			return;
		}
		if (blank || (zero && rules?.accept_0_hours === false)) {
			if (!confirm('要刪除這筆工時嗎？')) return;
			await run(deleteTimeDoc(entry.id, entry.lock_version), '已刪除工時');
			return;
		}
		if (h === entry.hours) return void (editing = null);
		await run(updateTimeDoc(entry.id, entry.lock_version, { hours: h }), `已改成 ${showHours(h)} 小時`);
	}
	function cellKey(e: KeyboardEvent) {
		if (e.key === 'Enter') {
			e.preventDefault();
			commit();
		} else if (e.key === 'Escape') {
			editing = null;
		}
	}

	async function panelSave(e: TimeEntryView) {
		const h = parseHours(panel?.edit[e.id] ?? '');
		if (h === null) return toast('看不懂這個時數', 'w');
		if (Number(h) === 0 && rules?.accept_0_hours === false) return panelDelete(e);
		opId = newOperationId();
		await run(updateTimeDoc(e.id, e.lock_version, { hours: h }), `已改成 ${showHours(h)} 小時`);
	}
	async function panelDelete(e: TimeEntryView) {
		if (!confirm('要刪除這筆工時嗎？')) return;
		opId = newOperationId();
		await run(deleteTimeDoc(e.id, e.lock_version), '已刪除工時');
	}

	// ---- 「＋ 新增一列」 ----
	let adding = $state(false);
	let addKey = $state('');
	let addActs = $state<Named[]>([]);
	let addAct = $state('');
	let addIssue = $state<{ key: string; subject: string; project: ProjectRef } | null>(null);
	let addError = $state<unknown>(null);
	async function lookIssue() {
		addError = null;
		addIssue = null;
		const k = addKey.trim().toUpperCase();
		if (!k) return;
		try {
			const i = await api('GET /issues/{key}', { path: { key: k } });
			const a = await api('GET /time-activities', { query: { project: i.project } });
			const pid = (await api('GET /projects')).find((p) => p.key === i.project)?.id ?? i.project;
			addIssue = { key: i.key, subject: i.subject, project: { id: pid, key: i.project } };
			addActs = a.items;
			addAct = a.default_id ?? a.items[0]?.id ?? '';
		} catch (e) {
			addError = e;
		}
	}
	function addRow(issue = addIssue, act = addActs.find((a) => a.id === addAct)) {
		if (!issue || !act) return;
		extra = [...extra, { project: issue.project, issue: { id: '', key: issue.key, subject: issue.subject }, activity: act }];
		adding = false;
		addKey = '';
		addIssue = null;
	}

	// Empty week: start from issues assigned to me (§6.7).
	let mine = $state<{ key: string; subject: string; project: string }[]>([]);
	$effect(() => {
		if (sheet && !sheet.rows.length && !mine.length) {
			api('POST /issues/search', { body: { assignee: 'me', status_category: 'open', limit: 5, fields: ['key', 'subject', 'project'] } })
				.then((r) => (mine = r.items.map((i) => ({ key: i.key, subject: String(i.subject ?? ''), project: String(i.project ?? i.key.split('-')[0]) }))))
				.catch(() => (mine = []));
		}
	});
	async function quickAdd(m: { key: string; subject: string; project: string }) {
		try {
			const [a, ps] = await Promise.all([api('GET /time-activities', { query: { project: m.project } }), api('GET /projects')]);
			const act = a.items.find((x) => x.id === a.default_id) ?? a.items[0];
			const p = ps.find((x) => x.key === m.project);
			if (!act || !p) return toast('這個專案沒有可用的工時活動', 'w');
			addRow({ key: m.key, subject: m.subject, project: { id: p.id, key: p.key } }, act);
		} catch (e) {
			cellError = e;
		}
	}
	function pickDate(e: Event) {
		const v = (e.target as HTMLInputElement).value;
		if (v) week = isoWeek(v);
	}
</script>

<svelte:head><title>我的工時・oxledger</title></svelte:head>
<div class="page">
	<div class="row" style="gap:8px;flex-wrap:wrap">
		<h1 style="margin:0">我的工時</h1>
		<span style="flex:1"></span>
		<button class="btn sm" onclick={() => (week = shiftWeek(week, -1))} aria-label="上一週" data-testid="week-prev">‹</button>
		<button class="btn sm" onclick={() => (week = isoWeek(todayLocal()))} data-testid="week-this">本週</button>
		<button class="btn sm" onclick={() => (week = shiftWeek(week, 1))} aria-label="下一週" data-testid="week-next">›</button>
		<input type="date" onchange={pickDate} aria-label="跳到某一天所在的週" />
		<button class="btn sm" onclick={exportCsv} disabled={exporting || !sheet} data-testid="time-export">{exporting ? '匯出中…' : '匯出 CSV'}</button>
		<span class="mono dim" data-testid="week">{week}</span>
	</div>
	<ErrorBox {error} onaction={load} />
	<ErrorBox error={cellError} onaction={() => ((cellError = null), (editing = null), (panel = null), load())} />
	{#if !sheet && !error}
		<div class="sync"><span class="spin"></span>載入中…</div>
	{:else if sheet}
		<div class="tsw">
			<table class="ts" data-testid="timesheet">
				<thead>
					<tr>
						<th>專案／議題／活動</th>
						{#each sheet.days as day, i (day)}<th class:over={over(i)} title={over(i) ? `超過每日上限 ${showHours(sheet.max_hours_per_day)} 小時` : ''} data-testid="day-head">{dayLabel(day)}</th>{/each}
						<th>合計</th>
					</tr>
				</thead>
				<tbody>
					{#each rows as row, r (`${row.project.key}/${row.issue?.key ?? '-'}/${row.activity.id}`)}
						<tr data-testid="ts-row" data-issue={row.issue?.key ?? ''}>
							<td class="rh">
								<span class="mono dim">{row.project.key}</span>
								{#if row.issue}<a class="mono" href="/issue/{row.issue.key}">{row.issue.key}</a> {row.issue.subject}{:else}<span class="dim">（沒有議題）</span>{/if}
								<div class="dim">{row.activity.name}</div>
							</td>
							{#each row.cells as c, d (c.date)}
								<td class="cell" class:multi={c.entry_ids.length > 1}>
									{#if editing && editing.r === r && editing.d === d}
										<input class="cin" bind:value={editing.text} onkeydown={cellKey} onblur={() => !busy && (editing = null)} disabled={busy} data-testid="cell-input" aria-label="{row.issue?.key ?? row.project.key} {c.date} 時數" />
									{:else}
										<button class="cbtn" onclick={() => openCell(r, d)} data-testid="cell" data-date={c.date} title={c.entry_ids.length > 1 ? `${c.entry_ids.length} 筆，點開逐筆改` : ''}>
											{c.hours !== null ? showHours(c.hours) : ''}{c.entry_ids.length > 1 ? ' ⋯' : ''}
										</button>
									{/if}
								</td>
							{/each}
							<td class="tot">{showHours(addHours(...row.cells.map((c) => c.hours)))}</td>
						</tr>
						{#if panel && panel.r === r}
							<tr>
								<td colspan="9">
									<div class="card" data-testid="cell-panel">
										<b>{sheet.days[panel.d]} 的 {panel.entries.length} 筆工時</b>
										{#each panel.entries as e (e.id)}
											<div class="row" style="gap:6px;margin-top:4px">
												<input style="width:80px" bind:value={panel.edit[e.id]} disabled={!e.can_edit || busy} data-testid="panel-hours" />
												<span class="dim">{e.comments || '（沒有說明）'}</span>
												<span style="flex:1"></span>
												{#if e.can_edit}<button class="btn sm" onclick={() => panelSave(e)} disabled={busy}>儲存</button>{/if}
												{#if e.can_delete}<button class="btn sm" onclick={() => panelDelete(e)} disabled={busy} data-testid="panel-delete">刪除</button>{/if}
											</div>
										{/each}
										<button class="btn sm ghost" style="margin-top:6px" onclick={() => (panel = null)}>關閉</button>
									</div>
								</td>
							</tr>
						{/if}
					{/each}
				</tbody>
				<tfoot>
					<tr>
						<th>每日合計</th>
						{#each sheet.day_totals as t, i (i)}<th class:over={over(i)} data-testid="day-total">{showHours(t)}</th>{/each}
						<th data-testid="week-total">{showHours(sheet.week_total)}</th>
					</tr>
				</tfoot>
			</table>
		</div>
		{#if !sheet.rows.length && !extra.length}
			<div class="empty" data-testid="ts-empty">
				這週還沒有記工時。
				{#if mine.length}
					<div style="margin-top:6px">從指派給我的議題開始：</div>
					{#each mine as m (m.key)}<div><button class="btn sm" onclick={() => quickAdd(m)} data-testid="quick-add">＋</button> <span class="mono">{m.key}</span> {m.subject}</div>{/each}
				{/if}
			</div>
		{/if}
		{#if adding}
			<div class="card" data-testid="add-row">
				<div class="row" style="gap:6px">
					<input bind:value={addKey} placeholder="單號，例如 AUTH-12" onchange={lookIssue} data-testid="add-key" />
					<button class="btn sm" onclick={lookIssue}>找</button>
					{#if addIssue}
						<span>{addIssue.subject}</span>
						<select bind:value={addAct} data-testid="add-activity">{#each addActs as a (a.id)}<option value={a.id}>{a.name}</option>{/each}</select>
						<button class="btn sm pri" onclick={() => addRow()} data-testid="add-submit">加入</button>
					{/if}
					<button class="btn sm ghost" onclick={() => (adding = false)}>取消</button>
				</div>
				<ErrorBox error={addError} />
			</div>
		{:else}
			<button class="btn" style="margin-top:8px" onclick={() => (adding = true)} data-testid="add-row-btn">＋ 新增一列</button>
		{/if}
		{#if max > 0}<p class="hint">每天最多記 {showHours(sheet.max_hours_per_day)} 小時。</p>{/if}
	{/if}
</div>

<style>
	.tsw {
		overflow-x: auto;
	}
	.ts {
		border-collapse: collapse;
		width: 100%;
		min-width: 760px;
	}
	.ts th,
	.ts td {
		border: 1px solid var(--line2);
		padding: 3px 6px;
		text-align: center;
		font-size: 13px;
	}
	.ts .rh {
		text-align: left;
		min-width: 220px;
	}
	.ts .over {
		color: var(--bad);
	}
	.cell {
		padding: 0;
		width: 72px;
	}
	.cbtn {
		width: 100%;
		min-height: 30px;
		background: none;
		border: none;
		color: inherit;
		cursor: pointer;
	}
	.cbtn:hover {
		background: var(--card2);
	}
	.cin {
		width: 64px;
		text-align: center;
	}
	.tot {
		font-weight: 600;
	}
</style>

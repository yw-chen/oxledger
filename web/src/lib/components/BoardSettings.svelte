<script lang="ts">
	// Board settings drawer (contract 05 §6.3): lanes (order, title, statuses,
	// WIP limit), statuses in no lane, swimlanes, card fields, the done window.
	// Applying changes the draft; the view page saves it (PATCH with
	// lock_version). A built-in board is copied first (R-VD5): a project board
	// needs manage_public_views, otherwise a personal board.
	import type { FilterFieldInfo, FilterOption } from '$api/types';
	import { CARD_FIELD_KEYS, CATEGORY_LABEL, DEFAULT_CARD_FIELDS, MAX_CARD_FIELDS, cardFieldLabel, toggleCardField, withCardKey } from '$lib/filter';
	import { FORBIDDEN } from '$lib/errors';

	type Lane = { id: string; title: string; values: unknown[]; wip_max?: number | null };
	let {
		def,
		statuses,
		builtin,
		fields = [],
		projectView = false,
		canProject = false,
		projectDenied = false,
		onapply,
		oncopy,
		oncancel
	}: {
		def: Record<string, unknown>;
		statuses: FilterOption[];
		builtin: boolean;
		/** Filter fields: custom fields offered as card fields. */
		fields?: FilterFieldInfo[];
		/** A project view: restricted fields cannot be card fields (R-VD3). */
		projectView?: boolean;
		/** The board belongs to a project, so a project copy is possible. */
		canProject?: boolean;
		/** A project copy was refused (forbidden_op: no manage_public_views). */
		projectDenied?: boolean;
		onapply: (d: Record<string, unknown>) => void;
		oncopy: (scope: 'project' | 'personal') => void;
		oncancel: () => void;
	} = $props();

	// svelte-ignore state_referenced_locally
	const start = structuredClone($state.snapshot(def)) as Record<string, any>;
	let by = $state<string>(start.lanes?.by ?? 'status_category');
	let lanes = $state<Lane[]>(structuredClone(start.lanes?.items ?? []));
	let swim = $state<string>(start.swimlanes?.by ?? '');
	let doneDays = $state<number>(start.done_window_days ?? 14);
	// A saved list without `key` (the API allows it) gets it here, so the limit the
	// checkboxes show is the one toggleCardField applies.
	let cards = $state<string[]>(withCardKey(Array.isArray(start.card_fields) ? start.card_fields : DEFAULT_CARD_FIELDS));
	const cardChoices = $derived([
		...CARD_FIELD_KEYS,
		...fields.filter((f) => f.field.startsWith('cf:') && !(projectView && f.restricted)).map((f) => f.field)
	]);
	const cardLabels = $derived(fields.map((f) => ({ field: f.field, label: f.label })));
	const LANE_BY: Record<string, string> = { status: '狀態', status_category: '狀態分類', assignee: '指派', priority: '優先權', fixed_version: '里程碑' };
	const SWIM: Record<string, string> = { '': '無', assignee: '指派', priority: '優先權', fixed_version: '里程碑', project: '專案' };

	const mapped = $derived(new Set(lanes.flatMap((l) => l.values.map(String))));
	const unmapped = $derived(by === 'status' ? statuses.filter((s) => !mapped.has(s.id)) : []);
	let n = 0;
	function addLane() {
		lanes.push({ id: `l${Date.now().toString(36)}${n++}`, title: '新欄', values: [] });
	}
	function move(i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= lanes.length) return;
		[lanes[i], lanes[j]] = [lanes[j], lanes[i]];
	}
	function toggle(l: Lane, id: string) {
		l.values = l.values.includes(id) ? l.values.filter((v) => v !== id) : [...l.values, id];
	}
	function changeBy(v: string) {
		by = v;
		if (v === 'status') lanes = [];
		else if (v === 'status_category') lanes = Object.entries(CATEGORY_LABEL).map(([id, title]) => ({ id, title, values: [id] }));
		else lanes = []; // the server makes one lane per option, plus 未設定
	}
	const invalid = $derived(
		(by === 'status' && (!lanes.length || lanes.some((l) => !l.values.length))) ||
			lanes.some((l) => !l.title.trim() || (l.wip_max != null && (!Number.isInteger(l.wip_max) || l.wip_max < 1 || l.wip_max > 999))) ||
			!Number.isInteger(doneDays) ||
			doneDays < 1 ||
			doneDays > 365
	);
	function apply() {
		const d = structuredClone(start);
		d.lanes = { ...(d.lanes ?? {}), by, items: lanes.length ? $state.snapshot(lanes).map((l) => ({ ...l, wip_max: l.wip_max || undefined })) : undefined };
		// Leave an absent swimlanes absent, so applying without a change is not a change.
		if (swim) d.swimlanes = { by: swim };
		else if (d.swimlanes !== undefined) d.swimlanes = null;
		d.done_window_days = doneDays;
		// Only written when changed, so opening and applying is not a change.
		const before = withCardKey(Array.isArray(start.card_fields) ? start.card_fields : DEFAULT_CARD_FIELDS);
		if (JSON.stringify(before) !== JSON.stringify(cards)) d.card_fields = [...cards];
		onapply(d);
	}
</script>

<div class="drawer-bg" onclick={oncancel} role="presentation"></div>
<div class="drawer" role="dialog" aria-modal="true" aria-label="看板設定" data-testid="board-settings-drawer">
	<div class="dh"><b>看板設定</b><span style="flex:1"></span><button class="btn sm ghost" onclick={oncancel} aria-label="關閉">✕</button></div>
	<div class="db">
		{#if builtin}
			<div class="banner i">內建看板不能修改。先複製一份再改欄位：專案看板需要「管理公開視圖」權限，個人看板只有你看得到。</div>
			<div class="row" style="gap:8px;flex-wrap:wrap;margin-top:8px">
				{#if canProject}
					<button class="btn" class:pri={!projectDenied} onclick={() => oncopy('project')} disabled={projectDenied} title={projectDenied ? `${FORBIDDEN}（需要「管理公開視圖」）` : ''} data-testid="board-copy">複製成專案看板…</button>
				{/if}
				<button class="btn" class:pri={!canProject || projectDenied} onclick={() => oncopy('personal')} data-testid="board-copy-personal">複製成個人看板…</button>
			</div>
			{#if projectDenied}<div class="forbid-note" data-testid="board-copy-denied">{FORBIDDEN}：你沒有「管理公開視圖」權限，可以改成複製成個人看板。</div>{/if}
		{:else}
			<label class="f"><span>欄依據</span>
				<select value={by} onchange={(e) => changeBy(e.currentTarget.value)} data-testid="bs-by">
					{#each Object.entries(LANE_BY) as [k, l] (k)}<option value={k}>{l}</option>{/each}
				</select>
			</label>
			{#if by !== 'status' && by !== 'status_category'}<div class="hint">依每個選項自動分欄，另加一欄「未設定」。</div>{/if}
			{#each lanes as l, i (l.id)}
				<div class="cond" data-testid="bs-lane">
					<div class="r">
						<input type="text" bind:value={l.title} aria-label="欄名稱" maxlength="40" style="flex:1" />
						<input type="number" min="1" max="999" placeholder="WIP" value={l.wip_max ?? ''} oninput={(e) => (l.wip_max = e.currentTarget.value ? Number(e.currentTarget.value) : null)} aria-label="WIP 上限" style="width:70px" />
						<button class="btn sm ghost" onclick={() => move(i, -1)} disabled={i === 0} aria-label="往前">↑</button>
						<button class="btn sm ghost" onclick={() => move(i, 1)} disabled={i === lanes.length - 1} aria-label="往後">↓</button>
						<button class="btn sm ghost" onclick={() => lanes.splice(i, 1)} aria-label="移除這欄">✕</button>
					</div>
					{#if by === 'status'}
						<div class="optpick">
							{#each statuses as s (s.id)}<label><input type="checkbox" checked={l.values.includes(s.id)} onchange={() => toggle(l, s.id)} />{s.label}</label>{/each}
						</div>
						{#if !l.values.length}<div class="hint" style="color:var(--bad)">至少選一個狀態</div>{/if}
					{/if}
				</div>
			{/each}
			{#if by === 'status'}
				<button class="btn sm" onclick={addLane} data-testid="bs-add">＋ 欄</button>
				{#if unmapped.length}<div class="banner w" style="margin-top:8px">沒有對應到任何欄的狀態：{unmapped.map((s) => s.label).join('、')}；這些議題不會顯示。</div>{/if}
			{/if}
			<label class="f" style="margin-top:10px"><span>泳道</span>
				<select bind:value={swim} data-testid="bs-swim">{#each Object.entries(SWIM) as [k, l] (k)}<option value={k}>{l}</option>{/each}</select>
			</label>
			<fieldset class="cond" style="margin-top:10px" data-testid="bs-card-fields">
				<legend>卡片欄位（最多 {MAX_CARD_FIELDS} 個，單號固定顯示）</legend>
				<div class="optpick">
					{#each cardChoices as k (k)}
						<label><input type="checkbox" checked={cards.includes(k)} disabled={!cards.includes(k) && cards.length >= MAX_CARD_FIELDS} onchange={() => (cards = toggleCardField(cards, k))} data-testid="bs-card-{k}" />{cardFieldLabel(k, cardLabels)}</label>
					{/each}
				</div>
				{#if cards.length >= MAX_CARD_FIELDS}<div class="hint">已經 {MAX_CARD_FIELDS} 個，要加別的先取消一個。</div>{/if}
			</fieldset>
			<label class="f" style="margin-top:6px"><span>完成欄只顯示最近幾天</span><input type="number" min="1" max="365" bind:value={doneDays} style="width:80px" /></label>
		{/if}
	</div>
	<div class="df">
		{#if !builtin}<button class="btn pri" onclick={apply} disabled={invalid} data-testid="bs-apply">套用</button>{/if}
		<button class="btn" onclick={oncancel}>取消</button>
		{#if !builtin}<span class="hint">套用後記得在上方按「儲存」。</span>{/if}
	</div>
</div>

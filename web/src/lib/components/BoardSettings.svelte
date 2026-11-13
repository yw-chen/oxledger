<script lang="ts">
	// Board settings drawer (contract 05 §6.3): lanes (order, title, statuses,
	// WIP limit), statuses in no lane, swimlanes, the done window. Applying
	// changes the draft; the view page saves it. A built-in board is copied
	// first (R-VD5).
	import type { FilterOption } from '$api/types';
	import { CATEGORY_LABEL } from '$lib/filter';

	type Lane = { id: string; title: string; values: unknown[]; wip_max?: number | null };
	let {
		def,
		statuses,
		builtin,
		onapply,
		oncopy,
		oncancel
	}: {
		def: Record<string, unknown>;
		statuses: FilterOption[];
		builtin: boolean;
		onapply: (d: Record<string, unknown>) => void;
		oncopy: () => void;
		oncancel: () => void;
	} = $props();

	// svelte-ignore state_referenced_locally
	const start = structuredClone($state.snapshot(def)) as Record<string, any>;
	let by = $state<string>(start.lanes?.by ?? 'status_category');
	let lanes = $state<Lane[]>(structuredClone(start.lanes?.items ?? []));
	let swim = $state<string>(start.swimlanes?.by ?? '');
	let doneDays = $state<number>(start.done_window_days ?? 14);
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
		onapply(d);
	}
</script>

<div class="drawer-bg" onclick={oncancel} role="presentation"></div>
<div class="drawer" role="dialog" aria-modal="true" aria-label="看板設定" data-testid="board-settings-drawer">
	<div class="dh"><b>看板設定</b><span style="flex:1"></span><button class="btn sm ghost" onclick={oncancel} aria-label="關閉">✕</button></div>
	<div class="db">
		{#if builtin}
			<div class="banner i">內建看板不能修改。先複製成專案看板（需要「管理公開視圖」權限）或個人看板，再改欄位。</div>
			<button class="btn pri" onclick={oncopy} data-testid="board-copy">複製成專案看板…</button>
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
				<select bind:value={swim}>{#each Object.entries(SWIM) as [k, l] (k)}<option value={k}>{l}</option>{/each}</select>
			</label>
			<label class="f" style="margin-top:6px"><span>完成欄只顯示最近幾天</span><input type="number" min="1" max="365" bind:value={doneDays} style="width:80px" /></label>
		{/if}
	</div>
	<div class="df">
		{#if !builtin}<button class="btn pri" onclick={apply} disabled={invalid} data-testid="bs-apply">套用</button>{/if}
		<button class="btn" onclick={oncancel}>取消</button>
		{#if !builtin}<span class="hint">套用後記得在上方按「儲存」。</span>{/if}
	</div>
</div>

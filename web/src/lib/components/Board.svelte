<script lang="ts">
	// A board view (contract 05 §6.3, K4). Lanes with counts and WIP hints,
	// cards, 「載入更多」 per lane. Dragging a card asks the issue's actions
	// and greys out lanes it cannot go to; dropping sends a transition (lanes
	// by status / category) or update_fields (other lanes) with the card's
	// lock_version. `M` on a focused card opens 「移到…」 (no mouse needed).
	import type { IssueActions, LaneOut, RunInput, RunItem, ViewRun, FieldSchema, Assignee, VersionView } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { doc, issueRef, submit, wireFields } from '#lib/commands.js';
	import { cardFieldLabel, cellText, countText } from '#lib/filter.js';
	import { toast } from '#lib/toast.svelte.js';
	import { goto } from '$app/navigation';
	import ErrorBox from './ErrorBox.svelte';
	import FieldInput from './FieldInput.svelte';

	let {
		run,
		reloadKey,
		by,
		statusCategory = {},
		canSettings = false,
		onsettings,
		canCreate = false,
		createHref = '/new',
		fieldType,
		doneDays = 14,
		cardFields = null,
		fieldLabels = [],
		onnarrow,
		onerror
	}: {
		run: (i: RunInput) => Promise<ViewRun>;
		reloadKey: string;
		/** `lanes.by` of the definition. */
		by: string;
		/** status id → todo/doing/done (lanes by category). */
		statusCategory?: Record<string, string>;
		canSettings?: boolean;
		onsettings?: () => void;
		canCreate?: boolean;
		createHref?: string;
		/** Type of a custom field `by` (`cf:N`), for the value's wrapper. */
		fieldType?: string;
		doneDays?: number;
		/** `def.card_fields`: what each card shows, in this order (`key` is always shown). */
		cardFields?: string[] | null;
		/** Filter fields, to name custom card fields. */
		fieldLabels?: { field: string; label: string }[];
		/** 「縮小範圍」 after `query_timeout` (§6). */
		onnarrow?: () => void;
		/** Called with the error, or null after a successful run. */
		onerror?: (e: unknown) => void;
	} = $props();

	let data = $state<ViewRun | null>(null);
	let lanes = $state<LaneOut[]>([]);
	let error = $state<unknown>(null);
	let loading = $state(false);
	let slow = $state(false);
	/** §6: no loading sign in the first 400 ms. */
	let skeleton = $state(false);
	let seq = 0;

	async function load(retried = false) {
		const n = ++seq;
		loading = true;
		error = null;
		const t = setTimeout(() => n === seq && (slow = true), 2000);
		const t0 = setTimeout(() => n === seq && (skeleton = true), 400);
		try {
			const r = await run({ limit: 50 });
			if (n !== seq) return;
			data = r;
			lanes = r.lanes ?? [];
			onerror?.(null);
		} catch (e) {
			if (n !== seq) return;
			if (e instanceof ApiError && e.code === 'invalid_cursor' && !retried) return load(true);
			error = e;
			onerror?.(e);
		} finally {
			clearTimeout(t);
			clearTimeout(t0);
			if (n === seq) {
				loading = false;
				slow = false;
				skeleton = false;
			}
		}
	}
	$effect(() => {
		void reloadKey;
		load();
	});
	/** Swimlane heads seen in a partial answer join the ones we have. */
	function mergeSwim(r: ViewRun) {
		if (!data || !r.swimlanes?.length) return;
		const have = new Set((data.swimlanes ?? []).map((s) => JSON.stringify(s.value ?? null)));
		const add = r.swimlanes.filter((s) => !have.has(JSON.stringify(s.value ?? null)));
		if (add.length) data.swimlanes = [...(data.swimlanes ?? []), ...add];
	}
	let laneBusy = $state<Record<string, boolean>>({});
	async function moreIn(l: LaneOut) {
		const n = seq;
		laneBusy[l.id] = true;
		try {
			const r = await run({ lane: l.id, cursor: l.next_cursor, limit: 50 });
			if (n !== seq) return; // the board was reloaded meanwhile
			const got = r.lanes?.find((x) => x.id === l.id);
			const cur = lanes.find((x) => x.id === l.id);
			if (got && cur) {
				const seen = new Set(cur.items.map((x) => x.id));
				cur.items = [...cur.items, ...got.items.filter((x) => !seen.has(x.id))];
				cur.next_cursor = got.next_cursor;
				mergeSwim(r);
			}
		} catch (e) {
			if (n !== seq) return;
			// §6: a stale cursor reloads that lane from the top; other errors stay
			// with the lane instead of replacing the board.
			if (e instanceof ApiError && e.code === 'invalid_cursor') await reloadLanes([l.id]);
			else toast(`「${l.title}」載入更多失敗，請再試一次`, 'bad');
		} finally {
			delete laneBusy[l.id];
		}
	}
	/** Reload only the given lanes (after a move). A full reload started
	 *  meanwhile wins: its lanes come from the newer definition. */
	async function reloadLanes(ids: string[]) {
		const n = seq;
		for (const id of ids) {
			try {
				const r = await run({ lane: id, limit: 50 });
				if (n !== seq) return;
				const got = r.lanes?.find((x) => x.id === id);
				const i = lanes.findIndex((x) => x.id === id);
				if (got && i >= 0) lanes[i] = got;
				mergeSwim(r);
			} catch {
				/* the next full load will show it */
			}
		}
	}

	// ---- moving cards
	const FIELD_OF: Record<string, string> = { assignee: 'assigned_to_id', priority: 'priority_id', fixed_version: 'fixed_version_id' };
	type Move = { status: string[] } | { set: unknown } | null;
	let drag = $state<{ item: RunItem; from: string; actions: IssueActions | null; failed: boolean } | null>(null);
	let over = $state<string | null>(null);
	let busy = $state<Record<string, boolean>>({});
	let flash = $state<string | null>(null);

	async function actionsOf(key: string): Promise<IssueActions> {
		return api('GET /issues/{key}/actions', { path: { key } });
	}
	/** What dropping into `lane` would send; null = cannot. */
	function moveTo(lane: LaneOut, a: IssueActions | null): Move {
		if (!a) return { status: [] }; // not known yet: decide on drop
		if (by === 'status' || by === 'status_category') {
			const want = (to: string) => (by === 'status' ? lane.values.includes(to) : lane.values.includes(statusCategory[to]));
			const ok = a.transitions.filter((t) => !t.blocked && want(t.to_status_id)).map((t) => t.to_status_id);
			return ok.length ? { status: ok } : null;
		}
		const key = FIELD_OF[by] ?? by;
		const f = a.fields.find((x) => x.key === key);
		if (!f?.editable) return null;
		return { set: lane.values[0] ?? null };
	}
	function why(lane: LaneOut): string {
		if (!drag?.actions) return '';
		return by === 'status' || by === 'status_category' ? '沒有可用的轉移' : '你不能修改這個欄位';
	}
	function onDragStart(e: DragEvent, item: RunItem, from: string) {
		e.dataTransfer?.setData('text/plain', item.key);
		drag = { item, from, actions: null, failed: false };
		const d = drag;
		actionsOf(item.key)
			.then((a) => {
				if (drag === d) drag.actions = a;
			})
			.catch(() => {
				if (drag === d) drag.failed = true;
			});
	}
	function onDragEnd() {
		drag = null;
		over = null;
	}
	async function onDrop(e: DragEvent, lane: LaneOut) {
		e.preventDefault();
		over = null;
		const d = drag;
		drag = null;
		if (!d || lane.id === d.from) return;
		const a = d.actions ?? (await actionsOf(d.item.key).catch(() => null));
		if (!a) return toast('暫時讀不到這張單能做什麼，請再試一次', 'bad');
		start(d.item, d.from, lane, a);
	}

	// A drop that needs a choice or required fields opens a small panel.
	let pick = $state<{ item: RunItem; from: string; lane: LaneOut; a: IssueActions; options: string[] } | null>(null);
	let need = $state<{ item: RunItem; from: string; lane: LaneOut; to: string; name: string; fields: FieldSchema[]; form: Record<string, unknown>; assignees: Assignee[]; versions: VersionView[] } | null>(null);
	let needError = $state<unknown>(null);

	function start(item: RunItem, from: string, lane: LaneOut, a: IssueActions) {
		const m = moveTo(lane, a);
		if (!m) return toast(`不能移到「${lane.title}」：${by === 'status' || by === 'status_category' ? '沒有可用的轉移' : '你不能修改這個欄位'}`, 'bad');
		if ('set' in m) return send(item, from, lane, { set: m.set });
		if (m.status.length > 1) {
			pick = { item, from, lane, a, options: m.status };
			return;
		}
		return chooseStatus(item, from, lane, a, m.status[0]);
	}
	async function chooseStatus(item: RunItem, from: string, lane: LaneOut, a: IssueActions, to: string): Promise<void> {
		pick = null;
		const t = a.transitions.find((x) => x.to_status_id === to);
		if (t?.then_required.length) {
			// 「轉到〈待驗證〉需要填：〈欄位〉」
			try {
				const v = await api('GET /issues/{key}/view', { path: { key: item.key } });
				const [s, as, vs] = await Promise.all([
					api('GET /projects/{id}/trackers/{tracker_id}/schema', { path: { id: v.project_id, tracker_id: v.tracker_id } }),
					api('GET /projects/{id}/assignees', { path: { id: v.project_id } }).catch(() => ({ assignees: [] as Assignee[] })),
					api('GET /projects/{id}/versions', { path: { id: v.project_id }, query: { status: 'open,locked' } }).catch(() => null)
				]);
				const fields = s.fields.filter((f) => t.then_required.includes(f.key));
				need = { item, from, lane, to, name: t.name, fields, form: {}, assignees: as.assignees, versions: vs?.versions ?? [] };
				needError = null;
			} catch (e) {
				toast('讀不到這張單的欄位設定，沒有移動', 'bad');
			}
			return;
		}
		return send(item, from, lane, { to });
	}
	async function send(item: RunItem, from: string, lane: LaneOut, what: { to?: string; set?: unknown; fields?: Record<string, unknown>; schema?: FieldSchema[] }): Promise<void> {
		busy[item.id] = true;
		const ops = [];
		// Transition first: later writes in the document are judged by the new
		// status' rules (02 §6.3), which is where then_required fields are writable.
		if (what.to) {
			ops.push({ kind: 'transition' as const, op_id: newOperationId(), target: issueRef(item.key), to_status_id: what.to });
			if (what.fields && Object.keys(what.fields).length) ops.push({ kind: 'update_fields' as const, op_id: newOperationId(), target: issueRef(item.key), set: wireFields(what.fields, what.schema ?? []) });
		} else {
			const k = FIELD_OF[by] ?? by;
			// Custom fields carry their type wrapper ({"list": id}).
			const schema = k.startsWith('cf:') ? ([{ key: k, type: fieldType ?? 'string' }] as FieldSchema[]) : [];
			ops.push({ kind: 'update_fields' as const, op_id: newOperationId(), target: issueRef(item.key), set: wireFields({ [k]: what.set ?? null }, schema) });
		}
		try {
			await submit(doc(ops, [{ key: item.key, lock: item.lock_version }]));
			need = null;
			flash = item.id;
			setTimeout(() => flash === item.id && (flash = null), 900);
			await reloadLanes([from, lane.id]);
		} catch (e) {
			const code = e instanceof ApiError ? e.code : '';
			if (need && (code === 'required_field_missing' || code === 'invalid_value')) {
				needError = e;
			} else {
				need = null;
				toast(
					code === 'conflict' ? '這張單剛被別人更新，已重新載入。' : code === 'invalid_transition' ? '這個轉移現在不允許。' : code === 'forbidden_op' ? '你沒有權限執行這個動作' : '沒有移動成功，尚未改到資料',
					'bad'
				);
				if (code === 'required_field_missing') {
					// The rules changed since the drag: ask again.
					const a = await actionsOf(item.key).catch(() => null);
					if (a && what.to) return chooseStatus(item, from, lane, a, what.to);
				}
				await reloadLanes([from, lane.id]);
			}
		} finally {
			delete busy[item.id];
		}
	}

	// Keyboard: Enter opens, M opens 「移到…」.
	let moveMenu = $state<{ item: RunItem; from: string; a: IssueActions | null } | null>(null);
	function onCardKey(e: KeyboardEvent, item: RunItem, from: string) {
		if (e.key === 'Enter') goto(`/issue/${item.key}`);
		else if (e.key === 'm' || e.key === 'M') {
			e.preventDefault();
			if (busy[item.id]) return; // still sending the last move
			moveMenu = { item, from, a: null };
			const m = moveMenu;
			actionsOf(item.key)
				.then((a) => moveMenu === m && (moveMenu.a = a))
				.catch(() => (moveMenu = null));
		}
	}
	const swimLabel = (v: unknown) => data?.swimlanes?.find((s) => JSON.stringify(s.value ?? null) === JSON.stringify(v ?? null))?.label ?? '未設定';
	function bySwim(items: RunItem[]) {
		if (!data?.swimlanes?.length) return [{ label: null as string | null, items }];
		const out: { label: string | null; items: RunItem[] }[] = [];
		for (const it of items) {
			const label = swimLabel(it.swimlane);
			const g = out.find((x) => x.label === label);
			if (g) g.items.push(it);
			else out.push({ label, items: [it] });
		}
		return out;
	}
	const isDone = (l: LaneOut) => l.values.includes('done') || (by === 'status' && l.values.every((v) => statusCategory[String(v)] === 'done') && l.values.length > 0);
	// Card body: the definition's card fields in order; without them, whatever
	// the server sent (its defaults).
	const SPECIAL = new Set(['key', 'subject', 'assignee', 'priority', 'due_date', 'overdue']);
	function extras(c: Record<string, unknown>): [string, unknown][] {
		const keys = cardFields?.length ? cardFields.filter((k) => !SPECIAL.has(k)) : Object.keys(c).filter((k) => !SPECIAL.has(k));
		return keys.map((k) => [k, c[k]] as [string, unknown]);
	}
	const shows = (c: Record<string, unknown>, k: string) => (cardFields?.length ? cardFields.includes(k) : c[k] !== undefined);
	export function reload() {
		load();
	}
</script>

{#if loading && !data}
	{#if skeleton}
		<div data-testid="board-skeleton" aria-busy="true" aria-label="載入中">
			{#if slow}<div class="hint" data-testid="board-slow">還在載入…</div>{/if}
			<div class="board">
				{#each { length: lanes.length || 3 } as _, li (li)}
					<div class="lane" data-testid="skel-lane">
						<div class="lh"><span class="skel" style="width:40%"></span></div>
						<div class="lb">
							{#each { length: 3 } as _, ci (ci)}
								<div class="skel-card" data-testid="skel-card"><span class="skel" style="width:30%"></span><span class="skel" style="width:85%"></span><span class="skel" style="width:50%"></span></div>
							{/each}
						</div>
					</div>
				{/each}
			</div>
		</div>
	{/if}
{:else if error}
	<ErrorBox {error} onaction={(a) => (a === 'narrow' && onnarrow ? onnarrow() : load())} />
{:else if data}
	{#if data.unmapped?.status_count}
		<div class="banner w" data-testid="unmapped">
			有 {data.unmapped.status_count} 個狀態沒有對應到任何欄，那些議題不會顯示。
			{#if canSettings}<button class="btn sm" onclick={onsettings}>設定欄位</button>{/if}
		</div>
	{:else if data.unmapped?.issues?.value}
		<div class="hint" style="margin-bottom:6px">有 {countText(data.unmapped.issues)} 張議題不在任何欄裡。</div>
	{/if}
	{#if !lanes.length}
		<div class="empty">還沒設定欄位。{#if canSettings}<button class="btn sm" onclick={onsettings}>設定欄位</button>{/if}</div>
	{:else if lanes.every((l) => !l.items.length)}
		<div class="empty" data-testid="board-empty">這個看板沒有議題。{#if canCreate}<a class="btn sm" href={createHref}>建立議題</a>{/if}</div>
	{/if}
	<div class="board" data-testid="board" role="list">
		{#each lanes as lane (lane.id)}
			{@const m = drag ? (drag.actions ? moveTo(lane, drag.actions) : { status: [] }) : null}
			{@const can = !!drag && lane.id !== drag.from && m !== null}
			<div
				class="lane"
				class:can-drop={can}
				class:no-drop={!!drag && lane.id !== drag.from && m === null}
				class:over={over === lane.id}
				role="listitem"
				aria-label="{lane.title}，{countText(lane.count)} 張"
				data-testid="lane"
				data-lane={lane.id}
				ondragover={(e) => {
					if (can) {
						e.preventDefault();
						over = lane.id;
					}
				}}
				ondragleave={() => over === lane.id && (over = null)}
				ondrop={(e) => onDrop(e, lane)}
			>
				<div class="lh" class:wip={lane.wip?.exceeded}>
					<span>{lane.title}</span><span class="hint">· {countText(lane.count)}</span>
					{#if lane.wip?.exceeded}<span class="chip c-warn" data-testid="wip">超過上限 {lane.wip.max}</span>{/if}
				</div>
				{#if drag && lane.id !== drag.from && m === null}<div class="hint" style="padding:4px 10px">{why(lane)}</div>{/if}
				<div class="lb">
					{#each bySwim(lane.items) as sw, si (si)}
						{#if sw.label !== null}<div class="swim">{sw.label}</div>{/if}
						{#each sw.items as it (it.id)}
							{@const c = it.card ?? {}}
							<div
								class="bcard"
								class:busy={busy[it.id]}
								class:flash={flash === it.id}
								draggable={!busy[it.id]}
								tabindex="0"
								role="button"
								aria-label="{it.key} {cellText(c.subject)}；Enter 開啟，M 移到其他欄"
								data-testid="card"
								data-key={it.key}
								ondragstart={(e) => onDragStart(e, it, lane.id)}
								ondragend={onDragEnd}
								onkeydown={(e) => onCardKey(e, it, lane.id)}
							>
								<div class="row" style="gap:6px"><a class="key" href="/issue/{it.key}" draggable="false">{it.key}</a>{#if busy[it.id]}<span class="spin"></span>{/if}</div>
								{#if shows(c, 'subject')}<div class="st" data-testid="card-subject">{cellText(c.subject)}</div>{/if}
								<div class="row" style="gap:6px;flex-wrap:wrap;font-size:12px;margin-top:2px">
									{#if shows(c, 'assignee')}<span class="hint" data-testid="card-assignee">{c.assignee ? cellText(c.assignee) : '未指派'}</span>{/if}
									{#if shows(c, 'priority') && c.priority}<span class="chip c-dim" data-testid="card-priority">{cellText(c.priority)}</span>{/if}
									{#if shows(c, 'due_date') && c.due_date}<span class:overdue={c.overdue} data-testid="card-due_date">{cellText(c.due_date)}{#if c.overdue} <span class="chip c-bad">逾期</span>{/if}</span>{/if}
									{#each extras(c) as [k, v] (k)}
										{#if v !== null && v !== undefined}<span class="hint" data-testid="card-{k}" title={cardFieldLabel(k, fieldLabels)}>{cardFields?.length ? `${cardFieldLabel(k, fieldLabels)}：` : ''}{cellText(v)}</span>{/if}
									{/each}
								</div>
							</div>
						{/each}
					{/each}
					{#if lane.next_cursor}<button class="btn sm" onclick={() => moreIn(lane)} disabled={laneBusy[lane.id]} data-testid="lane-more">{laneBusy[lane.id] ? '載入中…' : '載入更多'}</button>{/if}
					{#if isDone(lane)}<div class="hint" data-testid="done-window">只顯示最近 {doneDays} 天完成的。</div>{/if}
				</div>
			</div>
		{/each}
	</div>
	<div class="hint">只包含你看得到的議題。拖拉卡片，或選取卡片按 M 移到其他欄。</div>
{/if}

{#if pick}
	<div class="modal-bg" role="presentation" onclick={() => (pick = null)}>
		<div class="modal" role="dialog" aria-modal="true" aria-label="要轉到哪個狀態" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.key === 'Escape' && (pick = null)} tabindex="-1">
			<h3>要轉到哪個狀態？</h3>
			<div class="row" style="gap:6px;flex-wrap:wrap">
				{#each pick.options as to (to)}
					{@const t = pick.a.transitions.find((x) => x.to_status_id === to)}
					<button class="btn" onclick={() => chooseStatus(pick!.item, pick!.from, pick!.lane, pick!.a, to)} data-testid="pick-status">{t?.name ?? to}</button>
				{/each}
				<button class="btn ghost" onclick={() => (pick = null)}>取消</button>
			</div>
		</div>
	</div>
{/if}

{#if need}
	<div class="modal-bg" role="presentation">
		<div class="modal" role="dialog" aria-modal="true" aria-label="轉換需要填的欄位" data-testid="need-form">
			<h3>轉到〈{need.name}〉需要填：{need.fields.map((f) => f.name ?? f.key).join('、')}</h3>
			{#each need.fields as f (f.key)}
				<FieldInput field={f} bind:value={need.form[f.key]} assignees={need.assignees} versions={need.versions} required />
			{/each}
			<ErrorBox error={needError} />
			<div class="row" style="gap:8px;margin-top:8px">
				<button class="btn pri" disabled={busy[need.item.id]} onclick={() => send(need!.item, need!.from, need!.lane, { to: need!.to, fields: $state.snapshot(need!.form), schema: need!.fields })} data-testid="need-submit">確定</button>
				<button class="btn" onclick={() => (need = null)}>取消</button>
			</div>
		</div>
	</div>
{/if}

{#if moveMenu}
	<div class="modal-bg" role="presentation" onclick={() => (moveMenu = null)}>
		<div class="modal" role="dialog" aria-modal="true" aria-label="移到哪一欄" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.key === 'Escape' && (moveMenu = null)} tabindex="-1" data-testid="move-menu">
			<h3>把 {moveMenu.item.key} 移到…</h3>
			{#if !moveMenu.a}<div class="sync"><span class="spin"></span>讀取可用的移動…</div>
			{:else}
				<div style="display:flex;flex-direction:column;gap:4px">
					{#each lanes.filter((l) => l.id !== moveMenu!.from) as l (l.id)}
						{@const m = moveTo(l, moveMenu.a)}
						<button class="btn" disabled={!m} title={m ? '' : by === 'status' || by === 'status_category' ? '沒有可用的轉移' : '你不能修改這個欄位'} onclick={() => { const mm = moveMenu!; moveMenu = null; start(mm.item, mm.from, l, mm.a!); }} data-testid="move-to">{l.title}{m ? '' : '（不能移）'}</button>
					{/each}
				</div>
			{/if}
			<button class="btn ghost" style="margin-top:8px" onclick={() => (moveMenu = null)}>取消</button>
		</div>
	</div>
{/if}

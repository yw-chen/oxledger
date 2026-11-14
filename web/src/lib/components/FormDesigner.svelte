<script lang="ts">
	// 表單設計器（contract 05 §6.10, §3.6）: data rules and layout of one form
	// (a tracker at the site or in a project). Edits are drafts until
	// published; publishing rules shows the impact first and needs step-up.
	import type { Assignee, FieldSchema, FormDescription, FormDetail, FormImpact, FormVersionView, ValidateResult, VersionView } from '$api/types';
	import { api, ApiError, newOperationId } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import { stepped } from '#lib/stepup.svelte.js';
	import { toast } from '#lib/toast.svelte.js';
	import { fmtTime } from '#lib/format.js';
	import { wireFields } from '#lib/commands.js';
	import { condValues, evalWhen, whenProblem, type Group } from '#lib/formcond.js';
	import { RULE_KIND, cleanLayout, kindsFor, layoutOf, libName, loadLibrary, nextId, ruleSentence, rulesOf, type LField, type LayoutBody, type Library, type Rule, type Section } from '#lib/formdesign.js';
	import { describeError } from '#lib/errors.js';
	import { statusCategories } from '#lib/triage.js';
	import { whoName } from '#lib/who.js';
	import ErrorBox from './ErrorBox.svelte';
	import CondEditor from './CondEditor.svelte';
	import FormSections from './FormSections.svelte';

	let { formId, isAdmin = false }: { formId: string; isAdmin?: boolean } = $props();

	let detail = $state<FormDetail | null>(null);
	let lib = $state<Library | null>(null);
	let loadError = $state<unknown>(null);
	let error = $state<unknown>(null);
	let conflict = $state(false);
	let busy = $state(false);
	let tab = $state<'rules' | 'layout'>('rules');

	let rules = $state<Rule[]>([]);
	let layout = $state<LayoutBody>({ modes: { create: { sections: [] }, update: 'same_as_create' } });
	let rulesSrc = $state('');
	let layoutSrc = $state('');
	/** form_invalid: the rule or field to mark. */
	let badRule = $state<string | null>(null);
	let badField = $state<string | null>(null);

	function adopt(d: FormDetail) {
		detail = d;
		rules = rulesOf(d.rules.draft?.body ?? d.rules.published?.body ?? null);
		layout = layoutOf(d.layout.draft?.body ?? d.layout.published?.body ?? null, lib?.fields ?? []);
		rulesSrc = JSON.stringify(rules);
		layoutSrc = JSON.stringify(cleanLayout(layout));
		conflict = false;
	}
	async function load() {
		loadError = null;
		try {
			const d = await api('GET /forms/{id}', { path: { id: formId } });
			lib = await loadLibrary(
				d,
				session.projects.map((p) => p.id),
				isAdmin
			);
			adopt(d);
			loadVersions();
		} catch (e) {
			loadError = e;
		}
	}
	$effect(() => {
		if (formId) load();
	});

	const rulesDirty = $derived(JSON.stringify(rules) !== rulesSrc);
	const layoutDirty = $derived(JSON.stringify(cleanLayout(layout)) !== layoutSrc);
	const dirty = $derived(tab === 'rules' ? rulesDirty : layoutDirty);
	const hasDraft = $derived(tab === 'rules' ? !!detail?.rules.draft : !!detail?.layout.draft);
	const ruleProblem = $derived.by(() => {
		for (const r of rules) {
			if (!r.field) return `規則 ${r.id}：還沒選欄位`;
			const p = whenProblem(r.when);
			if (p) return `規則 ${r.id}：${p}`;
			if (r.kind === 'allowed_values_if' && !r.values?.length) return `規則 ${r.id}：至少要有一個允許的值`;
		}
		return '';
	});
	const layoutProblem = $derived.by(() => {
		for (const s of allSections()) {
			if (!s.title.trim()) return '有區塊沒有標題';
			const p = whenProblem(s.show_if) ?? s.fields.map((f) => whenProblem(f.show_if)).find(Boolean);
			if (p) return `區塊「${s.title}」：${p}`;
		}
		return '';
	});
	const problem = $derived(tab === 'rules' ? ruleProblem : layoutProblem);

	function fail(e: unknown) {
		if (e instanceof ApiError && e.code === 'conflict') conflict = true;
		else error = e;
		if (e instanceof ApiError && e.code === 'form_invalid') {
			const d = (e.body?.details ?? {}) as { rule_id?: string; field?: string };
			badRule = d.rule_id ?? null;
			badField = d.field ?? null;
		}
	}
	async function saveDraft(): Promise<boolean> {
		if (!detail) return false;
		busy = true;
		error = null;
		badRule = badField = null;
		const part = tab;
		try {
			const base = part === 'rules' ? (detail.rules.draft?.version ?? detail.published_rules_version) : (detail.layout.draft?.version ?? detail.published_layout_version);
			await api('PUT /forms/{id}/drafts/{part}', { path: { id: formId, part }, operationId: newOperationId(), body: { base_version: base, body: part === 'rules' ? { rules } : cleanLayout(layout) } });
			const d = await api('GET /forms/{id}', { path: { id: formId } });
			// Keep the other tab's unsaved edits.
			const keepR = part === 'layout' && rulesDirty ? rules : null;
			const keepL = part === 'rules' && layoutDirty ? layout : null;
			adopt(d);
			if (keepR) rules = keepR;
			if (keepL) layout = keepL;
			toast('已存成草稿', 'ok');
			return true;
		} catch (e) {
			fail(e);
			return false;
		} finally {
			busy = false;
		}
	}
	async function discard() {
		if (!detail) return;
		if (hasDraft && !confirm('丟棄草稿？已發布的版本不受影響。')) return;
		if (!hasDraft) {
			adopt(detail);
			return;
		}
		busy = true;
		error = null;
		try {
			adopt(await api('POST /forms/{id}/drafts/{part}/discard', { path: { id: formId, part: tab }, operationId: newOperationId(), body: {} as never }));
			toast('已丟棄草稿', 'ok');
		} catch (e) {
			fail(e);
		} finally {
			busy = false;
		}
	}

	// ---- publish (§6.10 發布流程) ----
	let pub = $state<{ part: 'rules' | 'layout'; impact: FormImpact | null; mode: 'block' | 'accept_and_mark'; rollbackTo?: number } | null>(null);
	async function openPublish() {
		if (dirty && !(await saveDraft())) return;
		if (!detail) return;
		const part = tab;
		const draft = part === 'rules' ? detail.rules.draft : detail.layout.draft;
		if (!draft) return;
		busy = true;
		error = null;
		try {
			const impact = await api('POST /forms/{id}/drafts/{part}/impact', { path: { id: formId, part }, operationId: newOperationId(), body: {} as never });
			const forced = part === 'rules' && impact.existing_issues && !impact.existing_issues.complete;
			pub = { part, impact, mode: forced ? 'accept_and_mark' : 'block' };
		} catch (e) {
			fail(e);
		} finally {
			busy = false;
		}
	}
	async function publish() {
		if (!pub || !detail) return;
		const p = pub;
		busy = true;
		error = null;
		try {
			const r = await stepped(
				() =>
					p.rollbackTo !== undefined
						? api('POST /forms/{id}/rollback', { path: { id: formId }, operationId: newOperationId(), body: { part: p.part, to_version: p.rollbackTo, ...(p.part === 'rules' ? { mode: p.mode } : {}) } })
						: api('POST /forms/{id}/drafts/{part}/publish', { path: { id: formId, part: p.part }, operationId: newOperationId(), body: { draft_version: p.impact!.draft_version, ...(p.part === 'rules' ? { mode: p.mode } : {}) } }),
				p.part === 'rules' ? '發布表單規則需要確認身分' : '發布表單版面需要確認身分'
			);
			if (!r) return;
			pub = null;
			adopt(await api('GET /forms/{id}', { path: { id: formId } }));
			loadVersions();
			toast(`已發布${p.part === 'rules' ? '規則' : '版面'} v${r.version}${r.marked ? `；${r.marked} 張不符的議題已標記` : ''}`, 'ok');
		} catch (e) {
			// The reason shows on the page (blocked counts, form_invalid marks).
			pub = null;
			fail(e);
		} finally {
			busy = false;
		}
	}
	const blocked = $derived(error instanceof ApiError && error.code === 'form_publish_blocked' ? ((error.body?.details ?? null) as FormImpact | null) : null);

	// ---- history ----
	let versions = $state<FormVersionView[]>([]);
	async function loadVersions() {
		try {
			versions = await api('GET /forms/{id}/versions', { path: { id: formId }, query: { part: tab } });
		} catch {
			versions = [];
		}
	}
	$effect(() => {
		void tab;
		if (detail) loadVersions();
	});
	const currentVersion = $derived(tab === 'rules' ? detail?.published_rules_version : detail?.published_layout_version);
	const MODE: Record<string, string> = { block: '有違規就不發布', accept_and_mark: '發布並標記不符的議題' };
	const STATE: Record<string, string> = { draft: '草稿', published: '目前版本', superseded: '舊版', discarded: '已丟棄' };
	function rollback(v: FormVersionView) {
		if (!confirm(`以 v${v.version} 的內容建立新的${tab === 'rules' ? '規則' : '版面'}版本？`)) return;
		pub = { part: tab, impact: null, mode: tab === 'rules' ? 'accept_and_mark' : 'block', rollbackTo: v.version };
	}

	// ---- rules tab ----
	let openRule = $state<string | null>(null);
	function addRule() {
		const id = nextId('r', rules.map((r) => r.id));
		rules.push({ id, field: '', kind: 'required_if', when: { all: [{ field: '', op: 'eq', value: '' }] } });
		openRule = id;
	}
	function setRuleField(r: Rule, key: string) {
		r.field = key;
		const k = kindsFor(lib?.fields.find((f) => f.key === key));
		if (!k.includes(r.kind)) r.kind = k[0];
		if (r.kind !== 'allowed_values_if') delete r.values;
	}
	function setRuleKind(r: Rule, kind: string) {
		r.kind = kind;
		if (kind === 'allowed_values_if') r.values ??= [];
		else delete r.values;
	}
	const wrapRuleValue = (key: string, id: string) => {
		const f = lib?.fields.find((x) => x.key === key);
		return key.startsWith('cf:') ? { [f?.type ?? 'string']: id } : id;
	};
	const plainV = (v: unknown) => (v && typeof v === 'object' ? Object.values(v as object)[0] : v);
	function toggleValue(r: Rule, id: string, on: boolean) {
		const cur = r.values ?? [];
		r.values = on ? [...cur, wrapRuleValue(r.field, id)] : cur.filter((x) => plainV(x) !== id);
	}
	function setValuesText(r: Rule, raw: string) {
		r.values = raw
			.split(/[,，]/)
			.map((s) => s.trim())
			.filter(Boolean)
			.map((s) => wrapRuleValue(r.field, s));
	}

	// ---- layout tab ----
	let layoutMode = $state<'create' | 'update'>('create');
	const sameAsCreate = $derived(layout.modes.update === undefined || layout.modes.update === 'same_as_create');
	function sectionsOf(mode: 'create' | 'update'): Section[] {
		if (mode === 'update' && !sameAsCreate) return (layout.modes.update as { sections: Section[] }).sections;
		return layout.modes.create.sections;
	}
	const sections = $derived(sectionsOf(layoutMode));
	function allSections(): Section[] {
		return [...layout.modes.create.sections, ...(sameAsCreate ? [] : (layout.modes.update as { sections: Section[] }).sections)];
	}
	function setUpdateOwn(own: boolean) {
		layout.modes.update = own ? { sections: structuredClone($state.snapshot(layout.modes.create.sections)) } : 'same_as_create';
	}
	const placed = $derived(new Set(sections.flatMap((s) => s.fields.map((f) => f.key))));
	let selSection = $state(0);
	let selField = $state<string | null>(null);
	const selected = $derived.by(() => {
		for (const s of sections) {
			const f = s.fields.find((x) => x.key === selField);
			if (f) return { s, f };
		}
		return null;
	});
	function addSection() {
		const id = nextId('s', allSections().map((s) => s.id));
		sections.push({ id, title: '新區塊', columns: 1, fields: [] });
		selSection = sections.length - 1;
	}
	function moveSection(i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= sections.length) return;
		[sections[i], sections[j]] = [sections[j], sections[i]];
	}
	function removeSection(i: number) {
		if (sections[i].fields.length && !confirm(`移除區塊「${sections[i].title}」？裡面的欄位會回到欄位庫。`)) return;
		sections.splice(i, 1);
		selSection = Math.max(0, Math.min(selSection, sections.length - 1));
	}
	function placeField(key: string, si: number, at?: number) {
		for (const s of sections) {
			const i = s.fields.findIndex((f) => f.key === key);
			if (i >= 0) {
				const [f] = s.fields.splice(i, 1);
				const t = sections[si];
				t.fields.splice(at ?? t.fields.length, 0, f);
				return;
			}
		}
		if (!sections[si]) return;
		sections[si].fields.splice(at ?? sections[si].fields.length, 0, { key } as LField);
		selField = key;
	}
	function unplace(key: string) {
		for (const s of sections) {
			const i = s.fields.findIndex((f) => f.key === key);
			if (i >= 0) s.fields.splice(i, 1);
		}
		if (selField === key) selField = null;
	}
	function moveField(s: Section, i: number, d: number) {
		const j = i + d;
		if (j < 0 || j >= s.fields.length) return;
		[s.fields[i], s.fields[j]] = [s.fields[j], s.fields[i]];
	}
	// Drag and drop (with the buttons as the keyboard way).
	let drag = $state<string | null>(null);
	function onDrop(e: DragEvent, si: number, at?: number) {
		e.preventDefault();
		const key = drag ?? e.dataTransfer?.getData('text/plain');
		drag = null;
		if (key) placeField(key, si, at);
	}
	const toggleShowIf = (o: { show_if?: unknown }) => {
		o.show_if = o.show_if ? undefined : ({ all: [{ field: '', op: 'eq', value: '' }] } as Group);
	};

	// ---- try-fill (§6.10 ④): the same evaluator from both doors ----
	let tryProject = $state('');
	// The header's 預覽 (§6.10 ①) opens the try-fill panel and brings it into view.
	let tryOpen = $state(false);
	let tryEl = $state<HTMLDetailsElement | null>(null);
	function openPreview() {
		tryOpen = true;
		queueMicrotask(() => tryEl?.scrollIntoView({ behavior: 'smooth', block: 'start' }));
	}
	let cats = $state<Map<string, { label: string; category: string }>>(new Map());
	$effect(() => {
		statusCategories()
			.then((m) => (cats = m))
			.catch(() => {});
	});
	let tryForm = $state<Record<string, unknown>>({});
	let tryAssignees = $state<Assignee[]>([]);
	let tryVersions = $state<VersionView[]>([]);
	let tryUi = $state<ValidateResult | null>(null);
	let tryMcp = $state<ValidateResult | null>(null);
	let tryError = $state<unknown>(null);
	$effect(() => {
		if (lib && !tryProject) tryProject = detail?.project_id ?? lib.projectId ?? '';
	});
	$effect(() => {
		const pid = tryProject;
		if (!pid) return;
		Promise.all([api('GET /projects/{id}/assignees', { path: { id: pid } }).catch(() => null), api('GET /projects/{id}/versions', { path: { id: pid }, query: { status: 'open' } }).catch(() => null)]).then(([a, v]) => {
			tryAssignees = a?.assignees ?? [];
			tryVersions = v?.versions ?? [];
		});
	});
	/** The form of the project being tried (another project may have other fields and rules). */
	let tryDesc = $state<FormDescription | null>(null);
	$effect(() => {
		const pid = tryProject;
		const t = detail?.tracker_id;
		if (!pid || !t) return;
		api('GET /projects/{id}/trackers/{t}/form', { path: { id: pid, t }, query: { op: 'create' } })
			.then((d) => {
				if (tryProject === pid) tryDesc = d;
			})
			.catch(() => (tryDesc = null));
	});
	const tryDescribe = $derived(tryDesc ?? lib?.describe ?? null);
	const tryFields = $derived((tryDescribe?.fields ?? []).filter((f) => f.key !== 'is_private') as unknown as FieldSchema[]);
	/** This form's published rule ids: the draft replaces them; other layers' rules still count. */
	const ownPublished = $derived(new Set(rulesOf(detail?.rules.published?.body ?? null).map((r) => r.id)));
	const tryStatus = $derived(lib?.describe?.creation?.initial_status_id ?? null);
	const tryValues = $derived(condValues(tryForm, tryFields, { tracker: detail?.tracker_id ?? null, status: tryStatus, status_category: tryStatus ? (cats.get(tryStatus)?.category ?? null) : null }));
	/** Required with the rules being edited (base + draft `required_if`). */
	const tryEntries = $derived(
		tryFields.map((f) => {
			const ff = f as unknown as { required: boolean; required_if?: { rule_id: string; when: unknown }[] };
			const base = ff.required || !!ff.required_if?.some((r) => !ownPublished.has(r.rule_id) && evalWhen(r.when, tryValues));
			const byDraft = rules.some((r) => r.kind === 'required_if' && r.field === f.key && evalWhen(r.when, tryValues));
			return { f, required: base || byDraft };
		})
	);
	const tryLayout = $derived(cleanLayout(layout).modes.create);
	function tryDoc() {
		const set: Record<string, unknown> = {};
		for (const [k, v] of Object.entries(tryForm)) if (v !== undefined && v !== null && v !== '') set[k] = v;
		return { command_schema_version: 1, ops: [{ kind: 'create_issue', op_id: newOperationId(), project_id: tryProject, tracker_id: detail!.tracker_id, fields: wireFields(set, tryFields) }], preconditions: [] };
	}
	async function runTry(which: 'ui' | 'mcp') {
		tryError = null;
		try {
			const r = await api('POST /commands/validate', { body: { command: tryDoc() } });
			if (which === 'ui') tryUi = r;
			else tryMcp = r;
		} catch (e) {
			// Document-level refusals (e.g. a missing subject) come as a plain
			// error, not in `items` (gap G34): show them as this door's answer.
			if (e instanceof ApiError && e.status >= 400 && e.status < 500 && !['forbidden_op', 'not_found', 'unauthenticated', 'csrf_failed', 'rate_limited'].includes(e.code)) {
				const d = (e.body?.details ?? {}) as { fields?: unknown };
				const r: ValidateResult = { outcome: 'invalid', rules_version: null, would_require_review: false, items: [{ op_id: '', status: 'blocked', errors: [{ code: e.code as never, message: e.message, details: e.body?.details, fields: d.fields }] }] };
				if (which === 'ui') tryUi = r;
				else tryMcp = r;
			} else tryError = e;
		}
	}
	function tryLines(r: ValidateResult): string[] {
		const out: string[] = [];
		for (const it of r.items)
			for (const e of it.errors) {
				const fields = Array.isArray(e.fields) ? (e.fields as string[]).map((k) => (lib ? libName(lib.fields, k) : k)).join('、') : '';
				const shown = describeError(new ApiError(400, { code: e.code, message: e.message, details: e.details } as never, null)).title;
				out.push(`${shown}${fields ? `（${fields}）` : ''}${e.rule_id ? ` [${e.rule_id}]` : ''}`);
			}
		return out;
	}
</script>

<ErrorBox error={loadError} onaction={load} />
{#if detail && lib}
	{@const d = detail}
	{@const L = lib}
	<div class="row hdr">
		<h1 style="margin:0" data-testid="fd-title">{d.tracker_name} 的表單</h1>
		<span class="chip">{d.project_key ? `專案 ${d.project_key}` : '站台'}</span>
		<span class="chip c-dim" data-testid="fd-versions">規則 v{d.published_rules_version}{d.rules_draft ? '・有草稿' : ''}・版面 v{d.published_layout_version}{d.layout_draft ? '・有草稿' : ''}</span>
	</div>
	{#if d.project_key}<p class="hint">專案層只能加規則（三種規則都只會更嚴），版面會整份取代站台的版面。</p>{/if}
	<div class="row tabs">
		<button class="tab" class:on={tab === 'rules'} onclick={() => (tab = 'rules')} data-testid="fd-tab-rules">資料規則{rulesDirty ? ' ●' : ''}</button>
		<button class="tab" class:on={tab === 'layout'} onclick={() => (tab = 'layout')} data-testid="fd-tab-layout">版面{layoutDirty ? ' ●' : ''}</button>
		<span style="flex:1"></span>
		<button class="btn" onclick={openPreview} data-testid="fd-preview">預覽</button>
		<button class="btn" onclick={saveDraft} disabled={busy || !dirty || !!problem} data-testid="fd-save">存草稿</button>
		<button class="btn" onclick={discard} disabled={busy || (!dirty && !hasDraft)} data-testid="fd-discard">丟棄{hasDraft ? '草稿' : '變更'}</button>
		<button class="btn pri" onclick={openPublish} disabled={busy || !!problem || (!dirty && !hasDraft)} data-testid="fd-publish">發布…</button>
	</div>
	{#if problem}<div class="err" data-testid="fd-problem">{problem}</div>{/if}
	{#if conflict}
		<div class="banner w" data-testid="fd-conflict">這份表單剛被別人改過，尚未儲存你的變更。<span class="sp"></span><button class="btn sm" onclick={load}>載入最新內容（會丟掉未存的變更）</button></div>
	{/if}
	{#if blocked}
		<div class="banner b" data-testid="fd-blocked">
			沒有發布：{blocked.existing_issues?.violating ?? 0} 張現有議題不符合新規則{blocked.existing_issues?.sample_keys?.length ? `（例：${blocked.existing_issues.sample_keys.join('、')}）` : ''}。可以改選「發布並標記不符的議題」。
		</div>
	{:else}
		<ErrorBox {error} />
	{/if}

	{#if tab === 'rules'}
		<div class="card" data-testid="fd-rules">
			{#each rules as r (r.id)}
				{@const f = L.fields.find((x) => x.key === r.field)}
				<div class="rule" class:bad={badRule === r.id} data-testid="fd-rule">
					<div class="row">
						<span class="mono dim">{r.id}</span>
						<span class="sent" data-testid="fd-rule-text">{r.field ? ruleSentence(r, L.fields, L.cat) : '（新規則）'}</span>
						<span style="flex:1"></span>
						<button class="btn sm" onclick={() => (openRule = openRule === r.id ? null : r.id)} data-testid="fd-rule-edit">{openRule === r.id ? '收合' : '編輯'}</button>
						<button class="btn sm ghost" onclick={() => rules.splice(rules.indexOf(r), 1)} aria-label="刪除規則" data-testid="fd-rule-remove">刪除</button>
					</div>
					{#if openRule === r.id}
						<div class="redit">
							<div class="row">
								<select value={r.field} onchange={(e) => setRuleField(r, e.currentTarget.value)} aria-label="欄位" data-testid="fd-rule-field">
									<option value="" disabled>選欄位</option>
									{#each L.fields as x (x.key)}<option value={x.key}>{x.name}</option>{/each}
								</select>
								<select value={r.kind} onchange={(e) => setRuleKind(r, e.currentTarget.value)} aria-label="規則" data-testid="fd-rule-kind" disabled={!r.field}>
									{#each kindsFor(f) as k (k)}<option value={k}>{RULE_KIND[k]}</option>{/each}
								</select>
							</div>
							{#if r.kind === 'allowed_values_if'}
								<div class="row" style="flex-wrap:wrap;gap:8px">
									<span class="hint">只能是：</span>
									{#if f?.options?.length}
										{#each f.options as o (o.id)}<label><input type="checkbox" checked={(r.values ?? []).some((x) => plainV(x) === o.id)} onchange={(e) => toggleValue(r, o.id, e.currentTarget.checked)} data-testid="fd-rule-value" /> {o.label}</label>{/each}
									{:else}
										<input value={(r.values ?? []).map(plainV).join(', ')} onchange={(e) => setValuesText(r, e.currentTarget.value)} placeholder="用逗號分開" />
									{/if}
								</div>
								<div class="hint">只檢查新指定的值；單上原本的值保留可以。</div>
							{:else if r.kind === 'readonly_if'}
								<div class="hint">依修改前的值判斷：條件成立時這個欄位不能改。</div>
							{/if}
							<div class="hint" style="margin-top:6px">當：</div>
							<CondEditor group={r.when as Group} cat={L.cat} />
						</div>
					{/if}
				</div>
			{:else}
				<div class="hint">還沒有規則。規則只會讓表單更嚴：條件成立時欄位必填、只能是某些值，或不能改。</div>
			{/each}
			<button class="btn" onclick={addRule} style="margin-top:8px" data-testid="fd-rule-add">＋ 新增規則</button>
			<p class="hint">條件只能用這張單的核心欄位與非受限的自訂欄位；受限欄位不會出現在條件清單。</p>
		</div>
	{:else}
		<div class="row" style="gap:8px;margin-bottom:8px">
			<span class="hint">版面：</span>
			<button class="tab sm" class:on={layoutMode === 'create'} onclick={() => (layoutMode = 'create')} data-testid="fd-mode-create">新建</button>
			<button class="tab sm" class:on={layoutMode === 'update'} onclick={() => (layoutMode = 'update')} data-testid="fd-mode-update">修改</button>
			{#if layoutMode === 'update'}
				<label><input type="checkbox" checked={sameAsCreate} onchange={(e) => setUpdateOwn(!e.currentTarget.checked)} data-testid="fd-same" /> 和新建相同</label>
			{/if}
			<span style="flex:1"></span>
			<span class="hint">版面只是呈現，不影響任何寫入。</span>
		</div>
		{#if layoutMode === 'update' && sameAsCreate}
			<div class="empty">修改時使用和新建相同的版面。取消勾選「和新建相同」來另外設計。</div>
		{:else}
			<div class="fd3">
				<div class="card lib" data-testid="fd-library">
					<h3>欄位庫</h3>
					{#each L.fields as x (x.key)}
						<div class="libf" class:used={placed.has(x.key)} draggable={!placed.has(x.key)} ondragstart={(e) => ((drag = x.key), e.dataTransfer?.setData('text/plain', x.key))} role="listitem" data-testid="fd-lib-field" data-key={x.key}>
							<span>{x.name}</span>
							{#if !placed.has(x.key)}<button class="btn sm ghost" onclick={() => placeField(x.key, selSection)} disabled={!sections.length} title="放到選取的區塊" aria-label="放進 {x.name}" data-testid="fd-lib-add">＋</button>{:else}<span class="hint">已放置</span>{/if}
						</div>
					{/each}
					<p class="hint">沒放進版面的欄位會出現在最後的「其他」區塊，不會因此看不到。</p>
				</div>
				<div data-testid="fd-canvas">
					{#each sections as s, si (s.id)}
						<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
						<div class="card sec" class:on={selSection === si} onclick={() => (selSection = si)} onfocusin={() => (selSection = si)} ondragover={(e) => e.preventDefault()} ondrop={(e) => onDrop(e, si)} role="group" aria-label="區塊 {s.title}" data-testid="fd-section">
							<div class="row">
								<input bind:value={s.title} aria-label="區塊標題" style="font-weight:600;flex:1" data-testid="fd-section-title" />
								<select bind:value={s.columns} aria-label="欄數" data-testid="fd-section-cols">
									{#each [1, 2, 3] as n (n)}<option value={n}>{n} 欄</option>{/each}
								</select>
								<button class="btn sm ghost" onclick={() => moveSection(si, -1)} disabled={si === 0} aria-label="上移區塊">↑</button>
								<button class="btn sm ghost" onclick={() => moveSection(si, 1)} disabled={si === sections.length - 1} aria-label="下移區塊">↓</button>
								<button class="btn sm ghost" onclick={() => toggleShowIf(s)} data-testid="fd-section-showif">{s.show_if ? '取消顯示條件' : '顯示條件'}</button>
								<button class="btn sm ghost" onclick={() => removeSection(si)} aria-label="移除區塊">移除</button>
							</div>
							{#if s.show_if}
								<div class="showif"><span class="hint">只在這些條件成立時顯示：</span><CondEditor group={s.show_if as Group} cat={L.cat} /></div>
							{/if}
							<div class="cgrid" style="grid-template-columns:repeat({s.columns},minmax(0,1fr))">
								{#each s.fields as fl, fi (fl.key)}
									<div
										class="fcard"
										class:sel={selField === fl.key}
										class:bad={badField === fl.key}
										style="grid-column:span {Math.min(s.columns, fl.span ?? 1)}"
										draggable="true"
										ondragstart={(e) => ((drag = fl.key), e.dataTransfer?.setData('text/plain', fl.key))}
										ondragover={(e) => e.preventDefault()}
										ondrop={(e) => (e.stopPropagation(), onDrop(e, si, fi))}
										onclick={(e) => (e.stopPropagation(), (selField = fl.key), (selSection = si))}
										onkeydown={(e) => e.key === 'Enter' && (selField = fl.key)}
										role="button"
										tabindex="0"
										data-testid="fd-field"
										data-key={fl.key}
									>
										<div class="row">
											<b>{libName(L.fields, fl.key)}</b>
											{#if fl.show_if}<span class="chip c-act" title="有顯示條件">條件</span>{/if}
											<span style="flex:1"></span>
											<button class="btn sm ghost" onclick={(e) => (e.stopPropagation(), moveField(s, fi, -1))} disabled={fi === 0} aria-label="前移">←</button>
											<button class="btn sm ghost" onclick={(e) => (e.stopPropagation(), moveField(s, fi, 1))} disabled={fi === s.fields.length - 1} aria-label="後移">→</button>
											<button class="btn sm ghost" onclick={(e) => (e.stopPropagation(), unplace(fl.key))} aria-label="移出版面" data-testid="fd-field-remove">×</button>
										</div>
										{#if badField === fl.key}<div class="err">這個欄位在某些情況必填，不能被顯示條件藏起來</div>{/if}
										{#if fl.help}<div class="hint">{fl.help}</div>{/if}
									</div>
								{/each}
							</div>
							{#if !s.fields.length}<div class="hint drop">把欄位拖到這裡，或在左邊按 ＋</div>{/if}
						</div>
					{/each}
					<button class="btn" onclick={addSection} data-testid="fd-section-add">＋ 區塊</button>
				</div>
				<div class="card props" data-testid="fd-props">
					<h3>屬性</h3>
					{#if selected}
						{@const sf = selected.f}
						<div><b>{libName(L.fields, sf.key)}</b> <span class="mono dim">{sf.key}</span></div>
						<label class="f">寬度
							<select value={sf.span ?? 1} onchange={(e) => (sf.span = Number(e.currentTarget.value))} data-testid="fd-span">
								{#each Array.from({ length: selected.s.columns }, (_, i) => i + 1) as n (n)}<option value={n}>{n} 欄</option>{/each}
							</select>
						</label>
						<label class="f">說明文字<input value={sf.help ?? ''} oninput={(e) => (sf.help = e.currentTarget.value)} data-testid="fd-help" /></label>
						<button class="btn sm" onclick={() => toggleShowIf(sf)} data-testid="fd-field-showif">{sf.show_if ? '取消顯示條件' : '加顯示條件'}</button>
						{#if sf.show_if}<CondEditor group={sf.show_if as Group} cat={L.cat} />{/if}
					{:else}
						<div class="hint">點一個欄位卡來設定寬度、說明與顯示條件。</div>
					{/if}
				</div>
			</div>
		{/if}
	{/if}

	<details class="card" bind:open={tryOpen} bind:this={tryEl} data-testid="fd-try">
		<summary><b>試填</b>：同一份內容從畫面與 MCP 送出，結果相同（只檢查，不寫入）</summary>
		{#if !d.project_id}
			<label class="f" style="max-width:280px">用哪個專案試
				<select bind:value={tryProject} data-testid="fd-try-project">
					{#each session.projects as p (p.id)}<option value={p.id}>{p.key}・{p.name}</option>{/each}
				</select>
			</label>
		{/if}
		{#if tryDescribe}
			<p class="hint">下方依目前編輯中的版面與規則即時標示必填與顯示；按鈕送到伺服器時，用的是<b>已發布</b>的規則。</p>
			<FormSections fields={tryEntries} layout={tryLayout} bind:form={tryForm} values={tryValues} assignees={tryAssignees} versions={tryVersions} />
			<div class="row" style="gap:8px">
				<button class="btn" onclick={() => runTry('ui')} disabled={!tryProject} data-testid="fd-try-ui">以畫面送出（只檢查）</button>
				<button class="btn" onclick={() => runTry('mcp')} disabled={!tryProject} data-testid="fd-try-mcp">用 MCP 送同樣內容</button>
			</div>
			<ErrorBox error={tryError} />
			<div class="try2">
				{#each [['畫面', tryUi], ['MCP', tryMcp]] as [label, r] (label)}
					<div data-testid="fd-try-result">
						<b>{label}</b>
						{#if r}
							{@const res = r as ValidateResult}
							<div class={res.outcome === 'valid' ? 'c-ok' : 'c-bad'}>{res.outcome === 'valid' ? '可以送出' : '會被擋下'}</div>
							{#each tryLines(res) as line (line)}<div class="hint">{line}</div>{/each}
						{:else}
							<div class="hint">還沒送出</div>
						{/if}
					</div>
				{/each}
			</div>
		{:else}
			<div class="hint">找不到使用這個類型的專案，無法試填。</div>
		{/if}
	</details>

	<div class="card" data-testid="fd-history">
		<h3>{tab === 'rules' ? '規則' : '版面'}歷史</h3>
		{#each versions as v (v.version)}
			<div class="row vrow" data-testid="fd-version">
				<b>v{v.version}</b>
				<span class="chip {v.state === 'published' ? 'c-ok' : 'c-dim'}">{STATE[v.state] ?? v.state}</span>
				<span class="dim">{fmtTime(v.published_at ?? v.created_at)}</span>
				{#if v.published_by ?? v.created_by}<span class="dim" data-testid="fd-version-who">{whoName(v.published_by ?? v.created_by, session.user, tryAssignees)}</span>{/if}
				{#if v.publish_mode}<span class="dim">{MODE[v.publish_mode] ?? v.publish_mode}</span>{/if}
				<span style="flex:1"></span>
				{#if (v.state === 'superseded' || v.state === 'published') && v.version !== currentVersion}
					<button class="btn sm" onclick={() => rollback(v)} disabled={busy} data-testid="fd-rollback">以此版本建立新版本</button>
				{/if}
			</div>
		{:else}
			<div class="hint">還沒有任何版本。</div>
		{/each}
	</div>

	{#if pub}
		<div class="modal-bg">
			<div class="modal" role="dialog" aria-modal="true" aria-label="發布" data-testid="fd-publish-dialog">
				{#if pub.part === 'layout'}
					<h3>{pub.rollbackTo !== undefined ? `回到版面 v${pub.rollbackTo}` : '發布版面'}</h3>
					<p>只改呈現，不影響任何寫入。</p>
					{#if pub.impact?.layout_warnings?.length}<p class="hint">有 {pub.impact.layout_warnings.length} 個欄位沒放進版面，會出現在「其他」區塊。</p>{/if}
				{:else}
					<h3>{pub.rollbackTo !== undefined ? `回到規則 v${pub.rollbackTo}` : '發布規則'}</h3>
					{#if pub.impact}
						{@const im = pub.impact}
						<ul class="impact" data-testid="fd-impact">
							<li>{im.existing_issues?.violating ? `${im.existing_issues.complete ? '' : '至少 '}${im.existing_issues.violating} 張現有議題不符合新規則${im.existing_issues.sample_keys.length ? `（例：${im.existing_issues.sample_keys.join('、')}）` : ''}` : `掃描的 ${im.existing_issues?.scanned ?? 0} 張議題都符合`}{im.existing_issues && !im.existing_issues.complete ? '（超過掃描上限，只能選發布並標記）' : ''}</li>
							<li>{im.pending_changesets?.would_fail ?? 0} 份待審提案套用時會失敗</li>
							<li>{im.undo_records?.not_applicable_after ?? 0} 筆可回復紀錄會無法回復</li>
						</ul>
					{:else}
						<p class="hint">回到舊版規則同樣可能讓現有議題不符合；請選擇發布方式。</p>
					{/if}
					<label><input type="radio" name="fd-mode" value="block" bind:group={pub.mode} disabled={!!pub.impact?.existing_issues && !pub.impact.existing_issues.complete} data-testid="fd-mode-block" /> 有違規就不發布</label>
					<label><input type="radio" name="fd-mode" value="accept_and_mark" bind:group={pub.mode} data-testid="fd-mode-mark" /> 發布並標記不符的議題（這些單下次修改時要補齊）</label>
				{/if}
				<div class="row" style="gap:8px;margin-top:12px">
					<button class="btn pri" onclick={publish} disabled={busy} data-testid="fd-publish-confirm">{busy ? '發布中…' : '發布'}</button>
					<button class="btn" onclick={() => (pub = null)}>取消</button>
				</div>
			</div>
		</div>
	{/if}
{:else if !loadError}
	<div class="sync"><span class="spin"></span>載入表單…</div>
{/if}

<style>
	.hdr {
		gap: 8px;
		margin-bottom: 6px;
	}
	.tabs {
		gap: 6px;
		margin: 8px 0;
		border-bottom: 1px solid var(--line);
		padding-bottom: 6px;
	}
	.tab {
		background: none;
		border: 0;
		border-bottom: 2px solid transparent;
		padding: 4px 10px;
		color: var(--dim);
		cursor: pointer;
		font: inherit;
	}
	.tab.on {
		color: var(--fg);
		border-bottom-color: var(--act);
		font-weight: 600;
	}
	.tab.sm {
		padding: 2px 8px;
	}
	.rule {
		border-bottom: 1px solid var(--line);
		padding: 6px 0;
	}
	.rule .row {
		gap: 8px;
	}
	.rule.bad,
	.fcard.bad {
		outline: 1px solid var(--bad);
		border-radius: 6px;
	}
	.redit {
		padding: 6px 0 4px 16px;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.fd3 {
		display: grid;
		grid-template-columns: 200px minmax(0, 1fr) 260px;
		gap: 12px;
		align-items: start;
	}
	.libf {
		display: flex;
		justify-content: space-between;
		align-items: center;
		padding: 3px 6px;
		border-radius: 6px;
		cursor: grab;
	}
	.libf.used {
		color: var(--dim);
		cursor: default;
	}
	.sec.on {
		border-color: var(--act);
	}
	.sec .row {
		gap: 4px;
	}
	.showif {
		margin: 6px 0;
		padding: 6px;
		background: var(--bg2);
		border-radius: 6px;
	}
	.cgrid {
		display: grid;
		gap: 6px;
		margin-top: 8px;
	}
	.fcard {
		border: 1px solid var(--line2);
		border-radius: 7px;
		padding: 5px 8px;
		background: var(--bg2);
		cursor: grab;
		min-width: 0;
	}
	.fcard.sel {
		border-color: var(--act);
	}
	.drop {
		padding: 10px;
		border: 1px dashed var(--line2);
		border-radius: 6px;
		text-align: center;
		margin-top: 6px;
	}
	.f {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin: 6px 0;
	}
	.try2 {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 12px;
		margin-top: 8px;
	}
	.vrow {
		gap: 8px;
		padding: 4px 0;
		border-bottom: 1px solid var(--line);
	}
	.impact {
		margin: 6px 0 10px;
		padding-left: 18px;
	}
	.modal label {
		display: block;
		margin: 4px 0;
	}
	.err {
		color: var(--bad);
		font-size: 12.5px;
	}
	@media (max-width: 1100px) {
		.fd3 {
			grid-template-columns: 1fr;
		}
	}
</style>

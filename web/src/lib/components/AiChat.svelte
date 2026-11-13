<script lang="ts">
	// 右欄 AI 助理 (contract 05 §6.19, §3.13). The built-in AI acts as the
	// person's own agent (`oxledger-ai@<login>`): it reads and writes only
	// through the delegations set up here, with the same policies as any agent.
	// The page in the middle is sent as a reference, never as text.
	import { page } from '$app/state';
	import { onDestroy } from 'svelte';
	import type { AiStatus, ConversationView, MessageView, TemplateSpec } from '$api/types';
	import { api, ApiError, newOperationId } from '$lib/api';
	import { INPUT_HINT, MAX_INPUT, pageRef, pageRefLabel, quotaText, renderMd, sseParser, stopTail, UNAVAILABLE, WRITE_REASON, writeLine, type Part } from '$lib/aichat';
	import { TIME_WARNING } from '$lib/aiteam';
	import { refreshCounts } from '$lib/counts.svelte';
	import { describeError } from '$lib/errors';
	import { fmtShort } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { stepped } from '$lib/stepup.svelte';
	import { toast } from '$lib/toast.svelte';

	let { status, onstatus }: { status: AiStatus; onstatus: () => Promise<void> } = $props();

	type Msg = { id: string; role: string; parts: Part[]; live?: boolean; tail?: string | null };
	let convs = $state<ConversationView[]>([]);
	let conv = $state<ConversationView | null>(null);
	let msgs = $state<Msg[]>([]);
	let loadingMsgs = $state(false);
	let text = $state('');
	let runId = $state<string | null>(null);
	let problem = $state<{ text: string; action?: 'new' | 'reset' } | null>(null);
	let providerAsk = $state<{ id: string; label: string } | null>(null);
	let busy = $state(false);
	let noRef = $state(false);
	let leaving = $state<{ host: string; href: string } | null>(null);
	let confirmDelete = $state(false);

	const a = $derived(status.assistant);
	const ready = $derived(!!a && a.delegated_projects > 0 && a.context_state === 'active');
	const ref = $derived(pageRef(page.url.pathname));
	$effect(() => {
		void ref?.kind;
		noRef = false;
	});

	async function loadConvs() {
		try {
			convs = (await api('GET /ai/conversations', { query: { limit: 20 } })).items.slice(0, 20);
			if (!conv && convs.length) await open(convs[0]);
		} catch {
			convs = [];
		}
	}
	$effect(() => {
		if (ready) loadConvs();
	});
	async function open(c: ConversationView) {
		conv = c;
		problem = null;
		providerAsk = null;
		loadingMsgs = true;
		try {
			const l = await api('GET /ai/conversations/{id}/messages', { path: { id: c.id } });
			conv = l.conversation;
			msgs = l.items.map(fromView);
		} catch (e) {
			problem = { text: describeError(e).title };
		} finally {
			loadingMsgs = false;
		}
	}
	const fromView = (m: MessageView): Msg => ({ id: m.id, role: m.role, parts: (m.parts ?? []) as Part[] });
	async function newConv() {
		try {
			const c = await api('POST /ai/conversations', { body: {}, operationId: newOperationId() });
			convs = [c, ...convs].slice(0, 20);
			conv = c;
			msgs = [];
			problem = null;
			providerAsk = null;
		} catch (e) {
			problem = { text: describeError(e).title };
		}
	}
	const convLabel = (c: ConversationView) => c.title || fmtShort(c.created_at);

	// ---- send and stream ----
	async function send(ack?: string) {
		const t = text.trim();
		// Enter and the button both land here: one send at a time.
		if (!t || runId || busy) return;
		busy = true;
		problem = null;
		try {
			if (!conv) await newConv();
			if (!conv) return;
			const r = await api('POST /ai/conversations/{id}/messages', {
				path: { id: conv.id },
				body: { text: t, page_context: ref && !noRef ? [ref] : [], ack_provider_id: ack ?? null },
				operationId: newOperationId()
			});
			providerAsk = null;
			msgs = [...msgs, { id: `u-${r.run_id}`, role: 'user', parts: [{ type: 'text', text: t }] }];
			text = '';
			stream(r.run_id, r.stream_url);
		} catch (e) {
			onSendError(e);
		} finally {
			busy = false;
		}
	}
	function onSendError(e: unknown) {
		if (!(e instanceof ApiError)) return (problem = { text: describeError(e).title });
		const d = (e.body?.details ?? {}) as Record<string, unknown>;
		switch (e.code) {
			case 'ai_provider_changed': {
				const p = (d.provider ?? {}) as { id?: string; name?: string; model?: string };
				providerAsk = { id: p.id ?? '', label: `${p.name ?? ''}${p.model ? `（${p.model}）` : ''}` };
				return;
			}
			case 'quota_exceeded':
				return (problem = { text: quotaText(d.scope as string, d.retry_at as string) });
			case 'ai_unavailable':
				return (problem = { text: 'AI 暫時無法使用；資料與操作正常。' });
			case 'ai_conversation_read_only':
				return (problem = { text: '這個對話已封存（AI 的工作脈絡已更換），只能查看。', action: 'new' });
			case 'context_unavailable':
				return (problem = { text: 'AI 助理的權限需要重新設定（有專案的權限改變了）。舊對話會封存。', action: 'reset' });
			case 'ai_run_in_progress':
				return (problem = { text: '上一個回覆還在進行中。' });
			default:
				problem = { text: describeError(e).title };
		}
	}

	let abort: AbortController | null = null;
	let gone = false;
	onDestroy(() => {
		gone = true;
		abort?.abort();
	});
	async function stream(id: string, url: string) {
		runId = id;
		const liveId = `a-${id}`;
		msgs = [...msgs, { id: liveId, role: 'assistant', parts: [], live: true }];
		// Found by id each time: notices are inserted before it (§6.19), and
		// the conversation cannot change while a reply streams.
		const liveMsg = () => msgs.find((x) => x.id === liveId);
		const push = (p: Part) => {
			const m = liveMsg();
			if (!m) return;
			const last = m.parts[m.parts.length - 1];
			if (p.type === 'text' && last?.type === 'text') last.text += p.text;
			else m.parts.push(p);
		};
		let lastId: number | null = null;
		let finished = false;
		for (let attempt = 0; attempt < 2 && !finished; attempt++) {
			abort = new AbortController();
			try {
				const headers: Record<string, string> = { Accept: 'text/event-stream' };
				if (lastId !== null) headers['Last-Event-ID'] = String(lastId);
				const res = await fetch(url, { headers, credentials: 'same-origin', signal: abort.signal });
				if (!res.ok || !res.body) throw new Error(`stream ${res.status}`);
				const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
				const feed = sseParser();
				for (;;) {
					const { value, done } = await reader.read();
					if (done) break;
					for (const ev of feed(value)) {
						if (ev.id !== null) lastId = ev.id;
						const d = ev.data ? JSON.parse(ev.data) : {};
						switch (ev.event) {
							case 'text.delta':
								push({ type: 'text', text: d.text ?? '' });
								break;
							case 'tool.started':
								push({ type: 'tool_progress', tool: d.tool, summary: d.summary, ok: null });
								break;
							case 'tool.finished': {
								const tp = [...(liveMsg()?.parts ?? [])].reverse().find((x) => x.type === 'tool_progress' && x.tool === d.tool && x.ok == null);
								if (tp && tp.type === 'tool_progress') tp.ok = !!d.ok;
								break;
							}
							case 'write':
								push({ type: 'write', ...d });
								refreshCounts();
								break;
							case 'notice': {
								// Before the reply, after any earlier notices (in arrival order).
								const at = msgs.findIndex((x) => x.id === liveId);
								const n: Msg = { id: `n-${id}-${ev.id}`, role: 'notice', parts: [{ type: 'notice', kind: d.kind, text: d.text }] };
								msgs = at < 0 ? [...msgs, n] : [...msgs.slice(0, at), n, ...msgs.slice(at)];
								break;
							}
							case 'run.finished': {
								finished = true;
								const m = liveMsg();
								if (m) m.tail = stopTail(d.stop_reason ?? (d.state === 'failed' ? 'interrupted' : null));
								break;
							}
							case 'resync':
								finished = true;
								if (conv) await open(conv);
								break;
						}
					}
					if (finished) break;
				}
			} catch (e) {
				if (abort?.signal.aborted) break;
				void e;
			}
		}
		if (gone) return;
		if (!finished && conv) await open(conv);
		const m = liveMsg();
		if (m) m.live = false;
		runId = null;
		abort = null;
		onstatus();
	}
	async function stop() {
		if (!runId) return;
		try {
			await api('POST /ai/runs/{id}/cancel', { path: { id: runId }, body: {} });
		} catch (e) {
			problem = { text: describeError(e).title };
		}
	}
	function keydown(e: KeyboardEvent) {
		if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
			e.preventDefault();
			send();
		}
	}

	async function del() {
		if (!conv) return;
		const id = conv.id;
		confirmDelete = false;
		try {
			await api('DELETE /ai/conversations/{id}', { path: { id } });
			convs = convs.filter((c) => c.id !== id);
			conv = null;
			msgs = [];
			toast('已刪除對話', 'ok');
		} catch (e) {
			problem = { text: describeError(e).title };
		}
	}

	// ---- set-up, extend, reset (each needs a step-up) ----
	let setup = $state(false);
	let picked = $state<string[]>([]);
	let readTime = $state(true);
	let timeAck = $state(false);
	let setupError = $state<unknown>(null);
	async function doSetup() {
		busy = true;
		setupError = null;
		// Fixed per attempt: when the step-up retries the whole function, what
		// already went through is replayed, not done twice.
		const opAssistant = newOperationId();
		const opFor = new Map(picked.map((pid) => [pid, newOperationId()]));
		try {
			const done = await stepped(async () => {
				const s = a ?? (await api('POST /ai/assistant', { body: {}, operationId: opAssistant })).assistant;
				if (!s) throw new Error('沒有建立 AI 助理');
				const t = (await api('GET /ai-team/templates')).find((x) => x.template === 'personal') as TemplateSpec;
				for (const pid of picked) {
					await api('POST /delegations', {
						operationId: opFor.get(pid),
						body: {
							agent_principal_id: s.principal,
							project_id: pid,
							proposal_ops: t.proposal_ops.filter((o) => (t.policies.commands[o] ?? 'off') !== 'off'),
							writable_fields: t.writable_fields.filter((f) => (t.policies.fields[f] ?? 'off') !== 'off'),
							expires_in_days: 30,
							policies: t.policies,
							read_own_time: readTime,
							read_own_time_ack: readTime ? timeAck : null
						}
					});
				}
				return true;
			}, '設定 AI 助理可以使用的專案需要確認身分');
			if (!done) return;
			setup = false;
			toast('AI 助理已可以使用這些專案', 'ok');
			await onstatus();
		} catch (e) {
			setupError = e;
		} finally {
			busy = false;
		}
	}
	async function extend() {
		try {
			if (await stepped(() => api('POST /ai/assistant/extend', { body: {} }), '延長 AI 助理的權限需要確認身分')) {
				toast('已延長 AI 助理的權限', 'ok');
				await onstatus();
			}
		} catch (e) {
			problem = { text: describeError(e).title };
		}
	}
	async function reset() {
		if (!a) return;
		try {
			const r = await stepped(() => api('POST /me/agent-runtimes/{id}/new-context', { path: { id: a.runtime_id }, body: { carry_over_delegations: true } }), '重新設定 AI 助理需要確認身分');
			if (!r) return;
			toast('已重新設定；舊對話已封存', 'ok');
			problem = null;
			conv = null;
			msgs = [];
			await onstatus();
			await loadConvs();
		} catch (e) {
			problem = { text: describeError(e).title };
		}
	}

	// External links open only after a confirmation (§6.19, V23).
	function clickLink(e: MouseEvent) {
		const el = (e.target as HTMLElement).closest('a[data-ext]') as HTMLAnchorElement | null;
		if (!el) return;
		e.preventDefault();
		const href = el.dataset.href ?? '';
		if (!/^https?:\/\//.test(href)) return;
		leaving = { host: el.dataset.ext ?? '', href };
	}
	const expireDay = $derived(a?.delegations_expire_at ? new Date(a.delegations_expire_at).toLocaleDateString('zh-TW', { month: 'numeric', day: 'numeric', timeZone: 'Asia/Taipei' }) : '');
	const origin = typeof location === 'undefined' ? 'http://localhost' : location.origin;
</script>

<div class="aic" data-testid="ai-chat">
	{#if !ready && a?.context_state !== 'quarantined'}
		<div class="csec" data-testid="ai-setup">
			<b>AI 助理還沒有可以使用的專案。</b>
			<p class="muted" style="font-size:13px">設定 AI 助理可以使用的專案後，它才能讀寫 oxledger 的資料；沒有設定時只能聊天。</p>
			{#if !setup}
				<button class="btn pri sm" onclick={() => ((setup = true), (picked = []), (timeAck = false))} data-testid="ai-setup-open">設定</button>
			{:else}
				{#each session.projects as p (p.id)}
					<label class="row" style="gap:6px"><input type="checkbox" value={p.id} bind:group={picked} data-testid="ai-setup-{p.key}" /> {p.key}・{p.name}</label>
				{/each}
				<p class="hint">預設照你的權限直接寫入；記工時、改工時要你核准。期限 30 天，之後可延長。</p>
				<label class="row" style="gap:6px"><input type="checkbox" bind:checked={readTime} onchange={() => (timeAck = false)} data-testid="ai-setup-time" /> 讓 AI 助理讀我自己的工時</label>
				{#if readTime}
					<div class="warnbox">
						<b>⚠ {TIME_WARNING.title}</b>
						{#each TIME_WARNING.body as p (p)}<p>{p}</p>{/each}
						<label class="row" style="gap:6px"><input type="checkbox" bind:checked={timeAck} data-testid="ai-setup-time-ack" /> {TIME_WARNING.ack}</label>
					</div>
				{/if}
				{#if setupError}<div class="banner b">{describeError(setupError).title}</div>{/if}
				<div class="row" style="gap:6px;margin-top:6px">
					<button class="btn pri sm" onclick={doSetup} disabled={busy || !picked.length || (readTime && !timeAck)} data-testid="ai-setup-save">完成設定</button>
					<button class="btn sm" onclick={() => (setup = false)}>取消</button>
				</div>
			{/if}
		</div>
	{:else}
		<div class="ahead">
			<select aria-label="對話" value={conv?.id ?? ''} onchange={(e) => { const c = convs.find((x) => x.id === e.currentTarget.value); if (c) open(c); else newConv(); }} disabled={!!runId} data-testid="ai-conv">
				{#if !conv}<option value="">新對話</option>{/if}
				{#each convs as c (c.id)}<option value={c.id}>{convLabel(c)}{c.state === 'read_only' ? '（已封存）' : ''}</option>{/each}
				{#if conv}<option value="__new">＋ 新對話</option>{/if}
			</select>
			<span style="flex:1"></span>
			{#if conv}<button class="btn sm ghost" onclick={() => (confirmDelete = true)} disabled={!!runId} title="刪除對話" data-testid="ai-delete">刪除</button>{/if}
			<a class="btn sm ghost" href="/ai-team/{encodeURIComponent(a?.handle ?? '')}" title="AI 助理可以使用的專案">設定</a>
		</div>
		{#if confirmDelete}
			<div class="banner w" data-testid="ai-delete-ask">刪除後無法復原。AI 已經做的變更不會被撤回，仍可在活動頁復原。<span class="sp"></span><button class="btn sm pri" onclick={del} data-testid="ai-delete-confirm">刪除</button><button class="btn sm" onclick={() => (confirmDelete = false)}>取消</button></div>
		{/if}
		{#if a?.context_state === 'quarantined'}
			<div class="banner b" data-testid="ai-reset">AI 助理的權限需要重新設定（有專案的權限改變了）。舊對話會封存。<span class="sp"></span><button class="btn sm pri" onclick={reset}>重新設定</button></div>
		{:else if a?.expiring_soon}
			<div class="banner w" data-testid="ai-expiring">AI 助理對 {a.delegated_projects} 個專案的權限將在 {expireDay} 到期<span class="sp"></span><button class="btn sm" onclick={extend}>延長</button></div>
		{/if}

		<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
		<div class="amsgs" onclick={clickLink} data-testid="ai-messages">
			{#if loadingMsgs}
				{#each [1, 2, 3] as i (i)}<div class="skel"></div>{/each}
			{:else if !msgs.length}
				<div class="empty">問 AI 助理關於你委派專案的事，例如「整理 AUTH 這週的新單」。</div>
			{/if}
			{#each msgs as m (m.id)}
				{#if m.role === 'notice'}
					{#each m.parts as p, i (i)}{#if p.type === 'notice'}<div class="anotice" data-testid="ai-notice">{p.text}</div>{/if}{/each}
				{:else}
					<div class="amsg {m.role}" data-testid="ai-msg" data-role={m.role}>
						{#if m.role === 'assistant'}<div class="awho"><span class="agname">{a?.handle ?? 'oxledger-ai'}</span><span class="agent-label">agent</span></div>{/if}
						{#each m.parts as p, i (i)}
							{#if p.type === 'text'}
								{#if m.role === 'user'}<div class="atext">{p.text}</div>{:else}<div class="atext md" data-testid="ai-text">{@html renderMd(p.text, origin)}</div>{/if}
							{:else if p.type === 'tool_progress'}
								<div class="atool" data-testid="ai-tool">{p.summary}{p.ok === false ? '（失敗）' : ''}</div>
							{:else if p.type === 'write'}
								<div class="card awrite" data-testid="ai-write">
									<div>{writeLine(p)}</div>
									{#if p.reason && WRITE_REASON[p.reason]}<div class="hint">{WRITE_REASON[p.reason]}</div>{/if}
									<div class="row" style="gap:6px;margin-top:4px">
										{#if p.outcome === 'applied'}
											{#each [...new Set((p.effects ?? []).map((x) => x.key))] as k (k)}<a class="btn sm" href="/issue/{k}">查看</a>{/each}
											{#if a}<a class="btn sm" href="/agents/{a.principal}/activity" data-testid="ai-undo">復原</a>{/if}
										{:else if p.changeset_id}
											<a class="btn sm pri" href="/cs/{p.changeset_id}" data-testid="ai-review">前往審閱</a>
										{/if}
									</div>
								</div>
							{:else if p.type === 'unavailable'}
								<div class="dim">{UNAVAILABLE}</div>
							{:else if p.type === 'notice'}
								<div class="anotice">{p.text}</div>
							{/if}
						{/each}
						{#if m.live}<span class="caret" aria-hidden="true"></span>{/if}
						{#if m.tail}<div class="dim" style="font-size:12px" data-testid="ai-tail">{m.tail}</div>{/if}
					</div>
				{/if}
			{/each}
		</div>

		{#if problem}
			<div class="banner w" data-testid="ai-problem">
				{problem.text}
				{#if problem.action === 'new'}<button class="btn sm" onclick={newConv}>開新對話</button>{/if}
				{#if problem.action === 'reset'}<button class="btn sm" onclick={reset}>重新設定</button>{/if}
			</div>
		{/if}
		{#if providerAsk}
			<div class="banner i" data-testid="ai-provider-changed">
				AI 服務已換成〈{providerAsk.label}〉。繼續這個對話時，先前的對話內容會送到新服務。
				<button class="btn sm pri" onclick={() => send(providerAsk!.id)}>繼續送出</button>
				<button class="btn sm" onclick={newConv}>開新對話</button>
			</div>
		{/if}
		{#if conv?.state === 'read_only'}
			<div class="banner w">這個對話已封存（AI 的工作脈絡已更換），只能查看。<button class="btn sm" onclick={newConv}>開新對話</button></div>
		{:else}
			<div class="ainput">
				{#if ref && !noRef}<span class="chip c-dim" data-testid="ai-scope">目前頁面：{pageRefLabel(ref)} <button class="x" aria-label="不附帶目前頁面" onclick={() => (noRef = true)}>×</button></span>{/if}
				<textarea bind:value={text} rows="3" maxlength={MAX_INPUT} placeholder="問 AI 助理…（Enter 送出，Shift+Enter 換行）" onkeydown={keydown} aria-label="訊息" data-testid="ai-input"></textarea>
				<div class="row" style="gap:6px">
					<span class="dim" style="font-size:12px">{text.length}/{MAX_INPUT}</span>
					<span style="flex:1"></span>
					{#if runId}
						<button class="btn sm" onclick={stop} data-testid="ai-stop">停止</button>
					{:else}
						<button class="btn sm pri" onclick={() => send()} disabled={busy || !text.trim()} data-testid="ai-send">送出</button>
					{/if}
				</div>
				<div class="hint" style="font-size:11.5px">{INPUT_HINT}</div>
			</div>
		{/if}
	{/if}
	{#if leaving}
		<div class="modal-bg">
			<div class="modal" role="dialog" aria-modal="true" aria-label="前往外部網站" data-testid="ai-leave">
				<h3>即將前往 {leaving.host}</h3>
				<p class="muted" style="font-size:13px;word-break:break-all">網址：{leaving.href}</p>
				<div class="row" style="justify-content:flex-end;gap:6px">
					<button class="btn" onclick={() => (leaving = null)}>取消</button>
					<button class="btn pri" onclick={() => { window.open(leaving!.href, '_blank', 'noopener,noreferrer'); leaving = null; }} data-testid="ai-leave-go">前往</button>
				</div>
			</div>
		</div>
	{/if}
</div>

<style>
	.aic { display: flex; flex-direction: column; flex: 1; min-height: 0; }
	.ahead { display: flex; gap: 6px; align-items: center; padding: 6px 12px; border-bottom: 1px solid var(--line); }
	.ahead select { max-width: 200px; }
	.amsgs { flex: 1; overflow: auto; padding: 8px 12px; display: flex; flex-direction: column; gap: 8px; }
	.amsg { font-size: 13px; max-width: 100%; }
	.amsg.user { align-self: flex-end; background: var(--act-a); border-radius: 8px; padding: 6px 9px; max-width: 85%; white-space: pre-wrap; }
	.amsg.assistant { align-self: flex-start; }
	.awho { display: flex; gap: 4px; align-items: center; font-size: 12px; margin-bottom: 2px; }
	.atool { font-size: 12px; color: var(--dim); }
	.anotice { align-self: center; background: var(--card2); color: var(--dim); font-size: 12px; border-radius: 6px; padding: 4px 8px; text-align: center; }
	.awrite { margin: 4px 0; padding: 6px 9px; }
	.md :global(p) { margin: 0 0 6px; }
	.md :global(ul), .md :global(ol) { margin: 0 0 6px; padding-left: 18px; }
	.md :global(table) { border-collapse: collapse; font-size: 12px; margin: 0 0 6px; }
	.md :global(th), .md :global(td) { border: 1px solid var(--line); padding: 2px 6px; }
	.md :global(code) { font-family: var(--mono, monospace); background: var(--card2); padding: 0 3px; border-radius: 3px; }
	.md :global(pre) { white-space: pre-wrap; background: var(--card2); padding: 6px; border-radius: 6px; }
	.ainput { border-top: 1px solid var(--line); padding: 8px 12px; display: flex; flex-direction: column; gap: 4px; }
	.ainput textarea { width: 100%; resize: vertical; }
	.x { border: 0; background: none; color: inherit; cursor: pointer; padding: 0 2px; }
	.skel { height: 34px; border-radius: 6px; background: var(--card2); animation: pulse 1.2s infinite; }
	.caret { display: inline-block; width: 7px; height: 14px; background: var(--dim); animation: pulse 1s infinite; vertical-align: text-bottom; }
	@keyframes pulse { 50% { opacity: 0.3; } }
	.warnbox { border: 2px solid var(--warn, #c77700); border-radius: 8px; padding: 6px 8px; margin: 6px 0; font-size: 12.5px; }
	.warnbox p { margin: 4px 0; }
</style>

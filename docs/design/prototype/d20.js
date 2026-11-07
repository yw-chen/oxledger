// oxledger 原型：D20 agent 直接寫入（設計 02 §16）。政策、command_submit、執行紀錄、配額與熔斷、undo、活動頁。
'use strict';

const RANK = {direct:0, review:1, off:2};
const worse = (a, b) => RANK[a] >= RANK[b] ? a : b;
const cmdName = k => (CMD_KINDS.find(c => c[0] === k) || [k, k])[1];
let EXEC_SEQ = 0;

// ---------- 政策（§16.3）：off > review > direct；欄位政策也套用 create_issue 的初始欄位 ----------
function policyDecision(dg, ops){
  const pol = dg.policy; let mode = 'direct'; const why = [];
  for (const o of ops){
    const cm = pol.commands[o.kind] || 'off';
    if (cm !== 'direct') why.push(`${cmdName(o.kind)}：${MODE_LABEL[cm]}`);
    mode = worse(mode, cm);
    const fields = o.kind === 'update_fields' || o.kind === 'create_issue' ? Object.keys(o.fields || {}).filter(f => POL_FIELDS.includes(f)) : [];
    for (const f of fields){ const fm = pol.fields[f] || 'off'; if (fm !== 'direct') why.push(`${fieldName(f)}：${MODE_LABEL[fm]}`); mode = worse(mode, fm); }
  }
  return {mode, why:[...new Set(why)]};
}

// ---------- 執行（direct、核准套用共用；每次都有 execution） ----------
function runOps(ops, meta){
  const ex = {id:'ex' + (++EXEC_SEQ), at:clock(), t:Date.now(), actor:meta.actor, onBehalf:meta.onBehalf || null, approver:meta.approver || null, via:meta.via, ctx:meta.ctx || null, csNo:meta.csNo || null, title:meta.title || '', changes:[], created:[], notes:[], dups:[], trans:[], reverts:null, revertedBy:null, undone:{}};
  const effects = []; const bind = {};
  const suffix = meta.via === 'changeset' ? (meta.csNo ? `（提案 #${meta.csNo}）` : `（在${meta.where || ' AI 面板'}核准）`) : '';
  const ev = (is, text) => is.events.push({at:ex.at, actor:meta.actor, via:meta.via, approver:meta.approver || null, exec:ex.id, text:text + suffix});
  for (const o of ops){
    if (o.kind !== 'create_issue') continue;
    const n = Math.max(...Object.keys(D.I).filter(k => k.startsWith(o.project + '-')).map(k => +k.split('-')[1])) + 1;
    const key = o.project + '-' + n;
    const is = D.I[key] = {key, id:'0199…' + (500 + n), project:o.project, tracker:o.tracker || 'Bug', subject:o.fields.subject, description:o.fields.description || '', status:'新建', assignee:o.fields.assignee || null, priority:o.fields.priority || 'Normal', cf:{severity:o.fields['cf:severity'] || null, estimate:null, customer:null}, notes:[], events:[], lock:0, created:'2026-11-07', isPrivate:false, indexed:false, author:meta.actor, onBehalf:meta.onBehalf, origin:{}};
    Object.keys(o.fields).forEach(f => is.origin[f] = ex.id);
    ev(is, '建立');
    bind[o.bind] = key; ex.created.push(key); effects.push({key, kind:'created', lock:0});
    scheduleIndex(key);
  }
  for (const o of ops){
    if (o.kind === 'create_issue') continue;
    const key = o.kind === 'duplicate_of' ? (bind[o.source] || o.source) : (bind[o.target] || o.target);   // duplicate_of 改的是 source（修正：原本取到 target）
    const is = D.I[key]; is.origin = is.origin || {}; const parts = [];
    if (o.kind === 'update_fields'){
      for (const [f, val] of Object.entries(o.fields)){
        const old = getField(is, f);
        if (same(old, val)) continue;   // 沒變的欄位不算寫入，也不進 undo
        ex.changes.push({key, f, before:old, after:val, prevOrigin:is.origin[f] || null});
        setField(is, f, val); is.origin[f] = ex.id; parts.push(`${fieldName(f)} ${fmtVal(f, old)} → ${fmtVal(f, val)}`);
      }
    } else if (o.kind === 'transition'){ ex.trans.push({key, from:is.status, to:o.to}); parts.push(`狀態 ${is.status} → ${o.to}`); is.status = o.to; is.origin.status = ex.id; }
    else if (o.kind === 'add_note'){ const id = 'n' + Date.now() + Math.random().toString(16).slice(2, 5); is.notes.push({id, author:meta.actor, onBehalf:meta.onBehalf, approver:meta.approver, via:meta.via, at:ex.at, text:o.text, private:false}); ex.notes.push({key, id}); parts.push('新增 note'); }
    else if (o.kind === 'duplicate_of'){
      is.duplicateOf = o.target; is.dupRel = ex.id; const t = D.I[o.target]; (t.duplicates = t.duplicates || []).push(key);
      ex.dups.push({source:key, target:o.target}); parts.push(`標為 ${o.target} 的重複`);
      t.lock++; ev(t, `${key} 標為本單的重複`); effects.push({key:o.target, kind:'updated', lock:t.lock});
    }
    if (!parts.length) continue;
    is.lock++; is.indexed = false; scheduleIndex(key);
    ev(is, parts.join('；'));
    const e = effects.find(x => x.key === key); if (e) e.lock = is.lock; else effects.push({key, kind:'updated', lock:is.lock});
  }
  D.execs.push(ex);
  return {effects, exec:ex};
}

// ---------- 配額與熔斷（§16.6） ----------
function ctlOf(c){ return D.ctl[c.id] = D.ctl[c.id] || {suspended:false, reason:null, at:null, audBlocks:[], perIssue:{}, minute:[]}; }
function suspend(c, reason){ const ctl = ctlOf(c); ctl.suspended = true; ctl.reason = reason; ctl.at = clock(); }
function noteBlocked(c, code, text){ D.blocked.push({at:clock(), t:Date.now(), ctx:c.id, code, text}); }

// ---------- MCP：command_submit（§16.4） ----------
function commandSubmit(a){
  const c = MCP._ctx();
  const fp = JSON.stringify([a.command, !!a.require_review]);
  const prev = D.opsLog[a.operation_id];
  if (prev){ if (prev.fp !== fp) throw new McpError('idempotency_key_reused'); return Object.assign({}, prev.res, {replayed:true}); }
  const ctl = ctlOf(c);
  if (ctl.suspended) throw new McpError('agent_suspended', {scope:'context', reason:ctl.reason});
  const ops = a.command.ops.map(o => Object.assign({}, o));
  for (const o of ops){
    const proj = opProject(o);
    if (!MCP._canRead(c, proj)) throw new McpError('delegation_unavailable', {project:proj});
    if (!canPublishTo(Object.keys(c.R), proj)){
      const now = Date.now(); ctl.audBlocks = ctl.audBlocks.filter(t => now - t < 3600e3); ctl.audBlocks.push(now);
      noteBlocked(c, 'audience_expansion_forbidden', `想寫到 ${proj}（讀者比 ${Object.keys(c.R).join('、')} 多）`);
      if (ctl.audBlocks.length >= 3){ suspend(c, '1 小時內 3 次想把內容寫到讀者較多的專案'); throw new McpError('agent_suspended', {scope:'context', reason:ctl.reason}); }
      throw new McpError('audience_expansion_forbidden', {kind:'cross_project', destination:proj, sources:Object.keys(c.R)});
    }
    if (o.target && o.kind !== 'duplicate_of'){ const is = MCP._issue(c, o.target); if (o.kind === 'update_fields'){ o.before = {}; Object.keys(o.fields).forEach(f => o.before[f] = getField(is, f)); } if (o.kind === 'transition') o.from = is.status; }
    if (o.kind === 'duplicate_of'){ MCP._issue(c, o.source); MCP._issue(c, o.target); }
  }
  const project = opProject(ops[0]);
  const dg = c.delegations.find(d => d.project === project && d.state === 'active');
  const dec = policyDecision(dg, ops);
  if (dec.mode === 'off'){ noteBlocked(c, 'forbidden_op', dec.why.join('、')); throw new McpError('forbidden_op', {reasons:dec.why}); }
  let res;
  if (dec.mode === 'review' || a.require_review){
    let group = a.group_id || null;
    if (!group && a.group_label){ group = 'grp-' + (++GROUP_SEQ); D.groups.push({id:group, label:a.group_label, ctx:c.id}); }
    const cs = makeChangeset({ctx:c, project, title:a.command.title, ops, pre:a.command.preconditions, evidence:(a.command.evidence || []).map(e => Object.assign({}, e)), group});
    cs._op = a.operation_id;
    res = {outcome:'review_required', reason:dec.mode === 'review' ? 'policy' : 'requested', policy_reasons:dec.why.length ? dec.why : undefined, changeset_id:cs.id, version:1, proposal_hash:curVer(cs).hash, group_id:group || undefined, review_url:`${ORIGIN}/changesets/${cs.id}?version=1`, expires_at:'2026-11-14T06:32:00Z', next_actions:['ask_human'], _no:cs.no};
  } else {
    const now = Date.now();
    const keys = [...new Set(ops.map(o => o.target || o.source).filter(Boolean))];
    for (const k of keys){
      const arr = ctl.perIssue[k] = (ctl.perIssue[k] || []).filter(t => now - t < 600e3);
      if (arr.length >= 10){ suspend(c, `同一張 ${k} 在 10 分鐘內被寫入超過 10 次`); noteBlocked(c, 'agent_suspended', ctl.reason); throw new McpError('agent_suspended', {scope:'context', reason:ctl.reason}); }
    }
    ctl.minute = ctl.minute.filter(t => now - t < 60e3);
    if (ctl.minute.length >= 10) throw new McpError('rate_limited', {retry_after_ms:60e3 - (now - ctl.minute[0])});
    for (const [k, lv] of Object.entries(a.command.preconditions || {})) if (D.I[k] && D.I[k].lock !== lv) throw new McpError('conflict', {key:k, expected:lv, current:D.I[k].lock});
    for (const o of ops){
      if (o.kind === 'transition' && !(WORKFLOW[D.I[o.target].status] || []).includes(o.to)) throw new McpError('invalid_transition', {from:D.I[o.target].status, allowed:WORKFLOW[D.I[o.target].status]});
      if (o.kind === 'update_fields' && o.fields.assignee && !D.P[opProject(o)].assignable.includes(o.fields.assignee)) throw new McpError('invalid_value', {field:'assignee'});
    }
    const r = runOps(ops, {actor:c.agent, onBehalf:'u1', via:'agent_direct', ctx:c.id, title:a.command.title});
    keys.forEach(k => ctl.perIssue[k].push(now)); ctl.minute.push(now);
    res = {outcome:'applied', execution_id:r.exec.id, effects:r.effects.map(e => ({key:e.key, kind:e.kind, lock_version:e.lock})), search:{consistent_after:'c1.3.' + (++XID)}, next_actions:[]};
  }
  D.opsLog[a.operation_id] = {fp, res};
  return res;
}

// ---------- 顯示 ----------
function actorHtml(id, approver){
  const ag = AGENTS[id];
  if (!ag) return esc(uname(id));
  return `<span class="agname" title="${esc(ag.client)}，代表 ${esc(uname(ag.onBehalf))}（委派者）">${esc(ag.handle)}</span> <span class="chip c-prop" style="padding:0 5px">agent</span>${approver ? ` <span class="dim">· ${esc(uname(approver))} 核准</span>` : ''}`;
}
function viaChip(via){ return ({agent_direct:'<span class="chip c-prop">直接寫入</span>', changeset:'<span class="chip c-act">核准套用</span>', human_direct:'<span class="chip c-dim">人</span>', undo:'<span class="chip c-warn">回復</span>'})[via] || ''; }
function execLines(ex){
  const L = [];
  ex.created.forEach(k => L.push(`建立 ${keyLink(k)} <span class="muted">${esc(D.I[k] ? D.I[k].subject : '')}</span>`));
  const byKey = {}; ex.changes.forEach(c => (byKey[c.key] = byKey[c.key] || []).push(c));
  Object.entries(byKey).forEach(([k, cs]) => L.push(`${keyLink(k)} ${cs.map(c => `${fieldName(c.f)} <s class="dim">${esc(fmtVal(c.f, c.before))}</s> → <b>${esc(fmtVal(c.f, c.after))}</b>`).join('；')}`));
  ex.trans.forEach(t => L.push(`${keyLink(t.key)} 狀態 ${esc(t.from)} → <b>${esc(t.to)}</b>`));
  ex.notes.forEach(n => { const note = (D.I[n.key].notes || []).find(x => x.id === n.id); L.push(`${keyLink(n.key)} 新增 note：「${esc(note ? note.text.slice(0, 60) : '')}」${note && note.retracted ? ' <span class="chip c-dim">已撤回</span>' : ''}`); });
  ex.dups.forEach(d => L.push(`${keyLink(d.source)} 標為 ${keyLink(d.target)} 的重複`));
  return L.join('<br>');
}
function todaySummary(c){
  const keys = new Set();
  D.execs.filter(e => e.ctx === c.id && e.via === 'agent_direct' && !e.revertedBy).forEach(e => [...e.created, ...e.changes.map(x => x.key), ...e.trans.map(x => x.key), ...e.notes.map(x => x.key), ...e.dups.map(x => x.source)].forEach(k => { if (D.I[k] && !D.I[k].isPrivate) keys.add(k); }));
  return keys.size;
}

// ---------- undo（§16.7） ----------
const same = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
function undoItems(ex){
  const items = [];
  for (const c of ex.changes){
    const is = D.I[c.key]; const cur = getField(is, c.f); const org = (is.origin || {})[c.f];
    let ok = true, why = '';
    if (ex.undone['f:' + c.key + c.f]){ ok = false; why = '已回復'; }
    else if (org !== ex.id){ ok = false; why = same(cur, c.after) ? `目前值相同，但之後 ${esc(lastSetter(is, c.f))} 又設過一次（是別人的決定，不動）` : `之後已被改成「${esc(fmtVal(c.f, cur))}」`; }
    items.push({id:'f:' + c.key + c.f, kind:'field', c, ok, why, auto:true, label:`${keyLink(c.key)} ${fieldName(c.f)}：${esc(fmtVal(c.f, c.after))} → <b>${esc(fmtVal(c.f, c.before))}</b>`});
  }
  for (const n of ex.notes){
    const note = D.I[n.key].notes.find(x => x.id === n.id);
    items.push({id:'n:' + n.id, kind:'note', n, ok:!!note && !note.retracted, why:note && note.retracted ? '已撤回' : '', auto:true, label:`${keyLink(n.key)} 撤回 note（事件保留，本文隱藏）`});
  }
  for (const d of ex.dups){
    const ok = D.I[d.source].duplicateOf === d.target && D.I[d.source].dupRel === ex.id;
    items.push({id:'d:' + d.source, kind:'dup', d, ok, why:ok ? '' : '這個關係已不是這次建立的', auto:true, label:`移除 ${keyLink(d.source)} → ${keyLink(d.target)} 的重複關係`});
  }
  for (const t of ex.trans){
    const is = D.I[t.key]; const ok = is.status === t.to && (WORKFLOW[is.status] || []).includes(t.from) && !ex.undone['t:' + t.key];
    items.push({id:'t:' + t.key, kind:'trans', t, ok, why:ok ? '' : is.status !== t.to ? `狀態已變成「${esc(is.status)}」` : `「${esc(t.to)}」不能轉回「${esc(t.from)}」`, auto:false, label:`${keyLink(t.key)} 轉回「${esc(t.from)}」（走一般 workflow）`});
  }
  for (const k of ex.created){
    const is = D.I[k]; const ok = (WORKFLOW[is.status] || []).includes('已拒絕') && !ex.undone['c:' + k];
    items.push({id:'c:' + k, kind:'created', k, ok, why:ok ? '' : `目前「${esc(is.status)}」不能直接關閉`, auto:false, label:`${keyLink(k)} 關閉為誤建（不刪除，轉到「已拒絕」）`});
  }
  return items;
}
function lastSetter(is, f){ const e = is.events.slice().reverse().find(x => x.text.includes(fieldName(f))); return e ? uname(e.actor) : '其他人'; }
function applyUndo(ex, ids){
  const done = [], skipped = [];
  const items = undoItems(ex).filter(i => ids.includes(i.id));
  const hx = {id:'ex' + (++EXEC_SEQ), at:clock(), t:Date.now(), actor:'u1', via:'undo', ctx:ex.ctx, title:`回復 ${AGENTS[ex.actor] ? AGENTS[ex.actor].handle : ''} 的 ${ex.id}`, changes:[], created:[], notes:[], dups:[], trans:[], reverts:ex.id, revertedBy:null, undone:{}};
  const evs = {};
  for (const it of items){
    if (!it.ok){ skipped.push(it); continue; }  // 交易內重驗（原型：重算一次）
    if (it.kind === 'field'){ const is = D.I[it.c.key]; setField(is, it.c.f, it.c.before); is.origin[it.c.f] = it.c.prevOrigin; (evs[it.c.key] = evs[it.c.key] || []).push(`${fieldName(it.c.f)} ${fmtVal(it.c.f, it.c.after)} → ${fmtVal(it.c.f, it.c.before)}`); }
    if (it.kind === 'note'){ const note = D.I[it.n.key].notes.find(x => x.id === it.n.id); note.retracted = {by:'u1', at:hx.at}; (evs[it.n.key] = evs[it.n.key] || []).push('撤回 agent 的 note'); }
    if (it.kind === 'dup'){ const s = D.I[it.d.source], t = D.I[it.d.target]; s.duplicateOf = null; s.dupRel = null; t.duplicates = (t.duplicates || []).filter(x => x !== it.d.source); t.lock++; t.events.push({at:hx.at, actor:'u1', via:'human_direct', text:`移除 ${it.d.source} 的重複關係（回復 ${ex.id}）`}); (evs[it.d.source] = evs[it.d.source] || []).push(`移除和 ${it.d.target} 的重複關係`); }
    if (it.kind === 'trans'){ const is = D.I[it.t.key]; is.status = it.t.from; is.origin.status = 'human'; (evs[it.t.key] = evs[it.t.key] || []).push(`狀態 ${it.t.to} → ${it.t.from}`); }
    if (it.kind === 'created'){ const is = D.I[it.k]; (evs[it.k] = evs[it.k] || []).push(`狀態 ${is.status} → 已拒絕（關閉為誤建）`); is.status = '已拒絕'; }
    ex.undone[it.id] = hx.id; done.push(it);
  }
  Object.entries(evs).forEach(([k, parts]) => { const is = D.I[k]; is.lock++; scheduleIndex(k); is.events.push({at:hx.at, actor:'u1', via:'human_direct', exec:hx.id, text:parts.join('；') + `（回復 ${AGENTS[ex.actor] ? AGENTS[ex.actor].handle : ''} 的變更）`}); });
  if (done.length){ hx.summary = done.map(i => i.label).join('<br>'); D.execs.push(hx); }
  if (undoItems(ex).every(i => !i.ok)) ex.revertedBy = hx.id;
  return {done, skipped};
}

// ---------- 活動頁（§16.10） ----------
function pgActivity(ctxId){
  const c = D.contexts.find(x => x.id === ctxId);
  if (!c) return `<div class="page"><h1>找不到這個工作脈絡</h1></div>`;
  const ag = AGENTS[c.agent]; const ctl = ctlOf(c);
  const F = UI.actF || 'all';
  const rows = [];
  D.execs.filter(e => e.ctx === c.id).forEach(e => rows.push({t:e.t, kind:e.via === 'undo' ? 'undo' : e.via, e}));
  D.blocked.filter(b => b.ctx === c.id).forEach(b => rows.push({t:b.t, kind:'blocked', b}));
  D.changesets.filter(cs => cs.ctx === c.id && cs.state === 'proposed').forEach(cs => rows.push({t:0, kind:'review', cs}));
  rows.sort((a, b) => b.t - a.t);
  const show = rows.filter(r => F === 'all' || (F === 'direct' && r.kind === 'agent_direct') || (F === 'review' && ['review','changeset'].includes(r.kind)) || (F === 'blocked' && r.kind === 'blocked') || (F === 'undo' && r.kind === 'undo'));
  const row = r => {
    if (r.kind === 'blocked') return `<div class="act-row"><span class="dim mono">${esc(r.b.at)}</span><span class="chip c-bad">被擋</span><span class="mono dim">${esc(r.b.code)}</span><span>${esc(r.b.text)}</span></div>`;
    if (r.kind === 'review') return `<div class="act-row"><span class="dim mono">—</span><span class="chip c-prop">待你核准</span><span>提案 #${r.cs.no} ${esc(r.cs.title)}</span><span style="flex:1"></span><a class="btn sm prop" href="#/cs/${r.cs.id}">審閱</a></div>`;
    const e = r.e;
    if (e.via === 'undo') return `<div class="act-row"><span class="dim mono">${esc(e.at)}</span>${viaChip('undo')}<span>${actorHtml('u1')} ${esc(e.title)}<div class="dim" style="font-size:12.5px">${e.summary || ''}</div></span></div>`;
    const items = undoItems(e); const can = items.some(i => i.ok);
    return `<div class="act-row"><span class="dim mono">${esc(e.at)}</span>${viaChip(e.via)}<div style="flex:1;min-width:0"><div>${esc(e.title || '')} <span class="dim mono">${e.id}</span>${e.revertedBy ? ' <span class="chip c-dim">已回復</span>' : ''}</div><div style="font-size:13px;margin-top:2px">${execLines(e)}</div></div>${can ? `<button class="btn sm" data-a="undo-open" data-ex="${e.id}">回復…</button>` : ''}</div>`;
  };
  const tab = (k, l) => `<button class="btn sm ${F === k ? 'pri' : ''}" data-a="act-f" data-v="${k}">${l}</button>`;
  const times = D.execs.filter(e => e.ctx === c.id && e.via === 'agent_direct').map(e => e.at);
  return `<div class="page">
    <div class="row"><a href="#/agents" class="dim" style="text-decoration:none">← Agent 委派</a></div>
    <div class="row"><h1 style="margin:0">${actorHtml(c.agent)} 的活動</h1><span style="flex:1"></span>${ctl.suspended ? `<button class="btn pri" data-a="resume" data-id="${c.id}">恢復寫入</button>` : `<button class="btn" data-a="pause" data-id="${c.id}">暫停寫入</button>`}</div>
    <p class="sub">${esc(c.clientName)}，代表 ${esc(uname(ag.onBehalf))}。直接寫入不用你逐筆核准，事後在這裡檢視與回復；回復前會先預覽，已被別人後續修改的欄位不會動。</p>
    ${ctl.suspended ? `<div class="banner w">寫入已暫停（${esc(ctl.at)}）：${esc(ctl.reason)}。讀取照常。</div>` : ''}
    <div class="row" style="margin:10px 0">${tab('all', '全部')}${tab('direct', '直接寫入')}${tab('review', '送審')}${tab('blocked', '被擋')}${tab('undo', '回復')}<span style="flex:1"></span>${times.length ? `<button class="btn sm bad" data-a="incident-open" data-id="${c.id}">事故回復…</button>` : ''}</div>
    <div class="card">${show.length ? show.map(row).join('') : '<span class="muted">還沒有活動。</span>'}</div>
  </div>`;
}

function undoModal(ex, items, opts){
  const sel = UI.undoSel;
  return `<div class="modal-bg"><div class="modal" style="max-width:640px"><h3>${opts.title}</h3><p class="muted">${opts.sub}</p>
    <div style="max-height:46vh;overflow:auto">${items.map(i => `<label class="row" style="gap:8px;align-items:flex-start;margin:6px 0;${i.ok ? '' : 'opacity:.6'}"><input type="checkbox" data-a="undo-sel" data-id="${i.id}" ${i.ok ? '' : 'disabled'} ${sel.has(i.id) ? 'checked' : ''} style="margin-top:3px"><span>${i.label}${i.ok ? (i.auto ? '' : ' <span class="chip c-warn">要你選</span>') : `<div class="c-warn" style="font-size:12.5px">不回復：${i.why}</div>`}</span></label>`).join('')}</div>
    <div class="row" style="margin-top:12px"><span class="hint">回復是你的新操作（human_direct），原紀錄保留 agent 作者。</span><span style="flex:1"></span><button class="btn" data-a="modal-x">取消</button><button class="btn pri" data-a="${opts.ok}" ${sel.size ? '' : 'disabled'}>回復所選（${sel.size}）</button></div></div></div>`;
}

// 事故回復：凍結清單，每張 issue 由新到舊，每筆各自處理，可部分成功；要選擇的項目不批次
function incidentPlan(c, fromAt){
  return D.execs.filter(e => e.ctx === c.id && e.via === 'agent_direct' && e.at >= fromAt && !e.revertedBy).sort((a, b) => b.t - a.t);
}

// ---------- 動作 ----------
function actD20(a, el){
  const d = el.dataset;
  switch (a){
    case 'act-f': UI.actF = d.v; render(); return true;
    case 'pause': { const c = D.contexts.find(x => x.id === d.id); suspend(c, `${uname('u1')} 手動暫停`); toast('已暫停這個 agent 的寫入；讀取照常', 'w'); render(); return true; }
    case 'resume': { const c = D.contexts.find(x => x.id === d.id); const ctl = ctlOf(c); ctl.suspended = false; ctl.reason = null; ctl.audBlocks = []; ctl.perIssue = {}; ctl.minute = []; toast('已恢復寫入', 'ok'); render(); return true; }
    case 'undo-open': { const ex = D.execs.find(x => x.id === d.ex); const items = undoItems(ex); UI.undoSel = new Set(items.filter(i => i.ok && i.auto).map(i => i.id)); UI.modal = {kind:'undo', ex:ex.id}; renderModal(); return true; }
    case 'undo-sel': { if (UI.undoSel.has(d.id)) UI.undoSel.delete(d.id); else UI.undoSel.add(d.id); renderModal(); return true; }
    case 'undo-ok': { const ex = D.execs.find(x => x.id === UI.modal.ex); const r = applyUndo(ex, [...UI.undoSel]); UI.modal = null; renderModal(); toast(`已回復 ${r.done.length} 項${r.skipped.length ? `；${r.skipped.length} 項在確認時已被改過，沒有動` : ''}`, r.skipped.length ? 'w' : 'ok'); render(); return true; }
    case 'incident-open': { const c = D.contexts.find(x => x.id === d.id); const ctl = ctlOf(c); const first = D.execs.filter(e => e.ctx === c.id && e.via === 'agent_direct').map(e => e.at).sort()[0]; UI.modal = {kind:'incident', ctx:c.id, from:first, paused:ctl.suspended, res:null}; renderModal(); return true; }
    case 'incident-pause': { const c = D.contexts.find(x => x.id === UI.modal.ctx); suspend(c, `${uname('u1')} 在事故回復前暫停`); UI.modal.paused = true; renderModal(); render(); return true; }
    case 'incident-ok': {
      const m = UI.modal; const c = D.contexts.find(x => x.id === m.ctx);
      const plan = incidentPlan(c, m.from); const res = {ok:0, partial:0, manual:[], skipped:[]};
      for (const ex of plan){
        const items = undoItems(ex);
        const auto = items.filter(i => i.ok && i.auto).map(i => i.id);
        items.filter(i => i.ok && !i.auto).forEach(i => res.manual.push({ex:ex.id, label:i.label}));
        items.filter(i => !i.ok && !ex.undone[i.id]).forEach(i => res.skipped.push({ex:ex.id, label:i.label, why:i.why}));
        if (auto.length){ const r = applyUndo(ex, auto); if (r.skipped.length) res.partial++; else res.ok++; }
      }
      m.res = res; renderModal(); render(); return true; }
  }
  return false;
}
function renderModalD20(m){
  if (m.kind === 'undo'){ const ex = D.execs.find(x => x.id === m.ex); return undoModal(ex, undoItems(ex), {title:`回復 ${AGENTS[ex.actor] ? AGENTS[ex.actor].handle : ''} 的 ${ex.id}？`, sub:`${esc(ex.at)} ${esc(ex.title || '')}。先預覽：只回復「目前值仍是 agent 寫的、而且之後沒人再設過」的欄位。`, ok:'undo-ok'}); }
  if (m.kind === 'incident'){
    const c = D.contexts.find(x => x.id === m.ctx);
    const opts = [...new Set(D.execs.filter(e => e.ctx === c.id && e.via === 'agent_direct').map(e => e.at))].sort();
    const plan = incidentPlan(c, m.from);
    if (m.res) return `<div class="modal-bg"><div class="modal" style="max-width:640px"><h3>事故回復結果</h3>
      <p>${m.res.ok} 筆完整回復、${m.res.partial} 筆部分回復。</p>
      ${m.res.skipped.length ? `<div class="k">沒有動（之後有人改過）</div>${m.res.skipped.map(s => `<div style="font-size:13px">${s.label} <span class="c-warn">${s.why}</span></div>`).join('')}` : ''}
      ${m.res.manual.length ? `<div class="k" style="margin-top:8px">要你逐筆處理（狀態轉回、關閉誤建的單）</div>${m.res.manual.map(s => `<div style="font-size:13px">${s.label} <a href="#/agents/activity/${c.id}" data-a="modal-x">到活動頁</a></div>`).join('')}` : ''}
      <div class="row" style="margin-top:12px"><span style="flex:1"></span><button class="btn pri" data-a="modal-x">完成</button></div></div></div>`;
    return `<div class="modal-bg"><div class="modal" style="max-width:640px"><h3>事故回復：${esc(AGENTS[c.agent].handle)}</h3>
      ${m.paused ? '<div class="banner o">寫入已暫停，回復期間 agent 不會再改。</div>' : `<div class="banner w">建議先暫停這個 agent，避免回復時它又寫入。<span class="sp"></span><button class="btn sm" data-a="incident-pause">先暫停</button></div>`}
      <label class="f" style="max-width:280px"><span>回復這個時間之後的直接寫入</span><select id="inc-from" data-a="inc-from">${opts.map(t => `<option ${t === m.from ? 'selected' : ''}>${t}</option>`).join('')}</select></label>
      <p class="muted">清單在按下時固定：${plan.length} 筆執行。每張 issue 由新到舊、每筆各自處理，可能部分成功。欄位、note、重複關係自動回復；狀態轉回與「關閉誤建的單」會列出來讓你逐筆處理。</p>
      <div style="max-height:30vh;overflow:auto;font-size:13px">${plan.map(e => `<div class="act-row"><span class="dim mono">${esc(e.at)}</span><div>${execLines(e)}</div></div>`).join('')}</div>
      <div class="row" style="margin-top:12px"><span style="flex:1"></span><button class="btn" data-a="modal-x">取消</button><button class="btn bad" data-a="incident-ok" ${plan.length ? '' : 'disabled'}>回復 ${plan.length} 筆</button></div></div></div>`;
  }
  return null;
}
document.addEventListener('change', e => {
  const t = e.target;
  if (t.dataset.a === 'inc-from' && UI.modal){ UI.modal.from = t.value; renderModal(); }
  if (t.dataset.pol && UI.wiz){ const [kind, k] = t.dataset.pol.split(':'); UI.wiz.policy[kind][k] = t.value; render(); }
  if (t.dataset.dpol){ const [ctx, dg, kind, k] = t.dataset.dpol.split(':'); const c = D.contexts.find(x => x.id === ctx); const g = c.delegations.find(x => x.id === dg); g.policy[kind][k] = t.value; g.policy.version++; toast(`已更新 ${g.project} 的操作設定（policy_version ${g.policy.version}）`, 'ok'); render(); }
});

// ---------- 委派精靈的操作表（§16.10） ----------
function policyTable(pol, attr){
  const sel = (kind, k) => `<select ${attr(kind, k)} style="width:auto">${['direct','review','off'].map(m => `<option value="${m}" ${pol[kind][k] === m ? 'selected' : ''}>${MODE_LABEL[m]}</option>`).join('')}</select>`;
  return `<table class="t"><tr><th>操作</th><th>agent 可以</th></tr>${CMD_KINDS.map(([k, l]) => `<tr><td>${l}${k === 'update_fields' || k === 'create_issue' ? ' <span class="hint">（欄位另見下表）</span>' : ''}</td><td>${sel('commands', k)}</td></tr>`).join('')}</table>
    <details style="margin-top:8px" ${UI.polOpen ? 'open' : ''}><summary class="dim" data-a="pol-open">逐欄位設定（修改與建立時都套用）</summary><table class="t" style="margin-top:6px"><tr><th>欄位</th><th>agent 可以</th></tr>${POL_FIELDS.map(f => `<tr><td>${fieldName(f)}</td><td>${sel('fields', f)}</td></tr>`).join('')}</table></details>`;
}

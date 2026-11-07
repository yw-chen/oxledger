// oxledger 原型：里程碑的 agent 寫入（U41）與健康度可調門檻（U43）。設計 02 §17.9a、§17.12。
// 使用者 2026-11-07：U41「現在開放，預設直接寫」；U43「加可調門檻」；U40「不管 redmine 匯入，在使用上註明」。
'use strict';

// ---------- 政策：新操作與新欄位（§17.9、§17.9a） ----------
const VKINDS = ['create_version', 'update_version'];
CMD_KINDS.push(['create_version', '建立里程碑'], ['update_version', '修改／關閉里程碑']);
POL_FIELDS.push('milestone', 'start_date', 'due_date');
const NEW_POL = {commands:['create_version', 'update_version'], fields:['milestone']};   // 日期不是新欄位（0001 就有）
const MANAGE_VERSIONS = {AUTH:['u1'], WEB:['u1'], OPS:[]};   // 原型：alice 在 AUTH、WEB 有 manage_versions

// 既有委派（U39 之前建的）：日期照原設定（直接寫入），里程碑欄位與兩個新操作「不會自動加」
function legacyPolicies(d){
  (d.contexts || []).forEach(c => (c.delegations || []).forEach(g => {
    if (!g.policy || g.policy._u39) return;
    g.policy._u39 = true; g.policy._dg = g.id;
    ['start_date', 'due_date'].forEach(f => { if (!g.policy.fields[f]) g.policy.fields[f] = 'direct'; });
    NEW_POL.fields.forEach(f => delete g.policy.fields[f]);
    NEW_POL.commands.forEach(k => delete g.policy.commands[k]);
  }));
}
function addVerData(d){
  Object.values(d.MS || {}).forEach(xs => xs.forEach(m => { m.lock = m.lock || 0; m.origin = m.origin || {}; m.createdBy = m.createdBy || 'u1'; m.log = m.log || []; }));
  d.sched = d.sched || {org:{days:7, pct:50, lock:0}, proj:{AUTH:{inherit:true, days:null, pct:null, lock:0}, WEB:{inherit:true, days:null, pct:null, lock:0}, OPS:{inherit:true, days:null, pct:null, lock:0}}};
  legacyPolicies(d);
  return d;
}
const _initialData3 = initialData;
initialData = function(){ return addVerData(_initialData3()); };
addVerData(D);
// 新委派（精靈、新工作脈絡）用 defaultPolicy()：此時 CMD_KINDS／POL_FIELDS 已含新項目，預設全部直接寫入；標記成新制
const _defaultPolicy = defaultPolicy;
defaultPolicy = function(){ const p = _defaultPolicy(); p._u39 = true; return p; };

const missingPol = p => p ? [...NEW_POL.commands.filter(k => !p.commands[k]).map(k => ['commands', k]), ...NEW_POL.fields.filter(f => !p.fields[f]).map(f => ['fields', f])] : [];
const _polSummary = polSummary;
polSummary = function(p){
  const miss = missingPol(p);
  return _polSummary(p) + (miss.length && p._dg ? ` <button class="btn sm" data-a="pol-addnew" data-dg="${p._dg}" title="${miss.map(([kind, k]) => kind === 'commands' ? cmdName(k) : fieldName(k)).join('、')}">有 ${miss.length} 個新操作可以加入</button>` : '');
};
policyTable = function(pol, attr){
  const sel = (kind, k) => pol[kind][k] ? `<select ${attr(kind, k)} style="width:auto">${['direct','review','off'].map(m => `<option value="${m}" ${pol[kind][k] === m ? 'selected' : ''}>${MODE_LABEL[m]}</option>`).join('')}</select>`
    : `<span class="dim">未加入這份委派</span>${pol._dg ? ` <button class="btn sm" data-a="pol-addone" data-dg="${pol._dg}" data-kind="${kind}" data-k="${k}">加入（直接寫入）</button>` : ''}`;
  return `<table class="t"><tr><th>操作</th><th>agent 可以</th></tr>${CMD_KINDS.map(([k, l]) => `<tr><td>${l}${k === 'update_fields' || k === 'create_issue' ? ' <span class="hint">（欄位另見下表）</span>' : ''}${k === 'update_version' ? ' <span class="hint">（含鎖定、關閉；刪除只限人）</span>' : ''}</td><td>${sel('commands', k)}</td></tr>`).join('')}</table>
    <details style="margin-top:8px" ${UI.polOpen ? 'open' : ''}><summary class="dim" data-a="pol-open">逐欄位設定（修改與建立時都套用）</summary><table class="t" style="margin-top:6px"><tr><th>欄位</th><th>agent 可以</th></tr>${POL_FIELDS.map(f => `<tr><td>${fieldName(f)}</td><td>${sel('fields', f)}</td></tr>`).join('')}</table></details>`;
};
const findDg = id => { for (const c of D.contexts) { const g = c.delegations.find(x => x.id === id); if (g) return g; } return null; };

const _fieldName2 = fieldName;
fieldName = function(f){ return ({start_date:'開始日', due_date:'到期日'})[f] || _fieldName2(f); };

// ---------- 健康度（§17.12，U43 可調門檻） ----------
function schedTh(p){
  const S = D.sched, pr = S.proj[p] || {inherit:true};
  return pr.inherit ? {days:S.org.days, pct:S.org.pct, source:'org'} : {days:pr.days, pct:pr.pct, source:'project'};
}
const _msStats = msStats;
msStats = function(m, project){
  const s = _msStats(m, project);
  s.reasons = [];
  if (s.overdue.length) s.reasons.push('有單逾期');
  if (s.late.length) s.reasons.push('有單晚於里程碑');
  const th = schedTh(project); s.th = th;
  const open = s.total - s.done;
  if (m.status !== 'closed' && th.days != null && th.pct != null && s.total > 0 && s.left <= th.days && open / s.total * 100 > th.pct){
    s.reasons.push(`離日期 ${s.left} 天、還有 ${Math.round(open / s.total * 100)}% 未完成（門檻 ${th.days} 天／${th.pct}%）`);
    if (s.health === 'ok') s.health = 'risk';
  }
  return s;
};

// ---------- 里程碑頁：歷史、agent 標記、門檻設定 ----------
const viaTag = e => e.via === 'agent_direct' ? ' <span class="chip c-prop">直接寫入</span>' : e.via === 'changeset' ? ' <span class="chip c-act">核准套用</span>' : e.via === 'undo' ? ' <span class="chip c-warn">回復</span>' : '';
const _pgMilestones = pgMilestones;
pgMilestones = function(key){
  let h = _pgMilestones(key);
  h = h.replace('有單的里程碑不能刪，只能關閉。改日期會檢查已排入的單。',
    '你看得到的單還在裡面時不能刪，只能關閉。改日期會檢查已排入的單。agent 依委派設定可以建立、修改、關閉里程碑（預設直接寫入），<b>不能刪除</b>。里程碑只屬於一個專案，不能跨專案共用；從 Redmine 搬資料時，共用版本請自行在各專案建立。');
  const list = (D.MS[key] || []).slice().sort((a, b) => dnum(a.date) - dnum(b.date));
  const hist = list.filter(m => m.log.length).map(m => `<div style="margin:4px 0"><b>${esc(m.name)}</b>${m.log.slice(-4).reverse().map(e => `<div class="act-row" style="font-size:12.5px"><span class="dim mono">${esc(e.at || '')}</span>${viaTag(e)}<span>${actorHtml(e.by, e.approver)} ${esc(e.t)}</span>${e.exec && canUndoV(e.exec) ? `<button class="btn sm" data-a="undo-open" data-ex="${e.exec}">回復…</button>` : ''}</div>`).join('')}</div>`).join('');
  const S = D.sched, pr = S.proj[key] = S.proj[key] || {inherit:true, days:null, pct:null, lock:0}; const th = schedTh(key);
  const F = UI.thForm && UI.thForm.p === key ? UI.thForm : (UI.thForm = {p:key, inherit:pr.inherit, days:pr.inherit ? S.org.days : pr.days, pct:pr.inherit ? S.org.pct : pr.pct, lock:pr.lock});
  const can = (MANAGE_VERSIONS[key] || []).includes(ME);
  const thCard = `<div class="card"><div class="k" style="margin-bottom:6px">健康度門檻 <span class="hint">目前：${th.days == null || th.pct == null ? '已停用「接近日期」這條' : `離日期 ≤ ${th.days} 天且未完成 &gt; ${th.pct}%`}（${th.source === 'org' ? '跟組織一樣' : '本專案設定'}）</span></div>
    <p class="hint" style="margin-top:0">有單逾期、或到期日晚於里程碑，一律標「有風險」；另外這條可調：接近里程碑日期、還有很多沒完成時也標黃。只影響顯示與首頁提醒，不影響任何寫入。</p>
    <div class="row" style="gap:10px;flex-wrap:wrap">
      <label class="row" style="gap:4px"><input type="radio" name="th-mode" data-th="inherit" value="1" ${F.inherit ? 'checked' : ''} ${can ? '' : 'disabled'}> 跟組織一樣（${S.org.days} 天／${S.org.pct}%，系統管理員設定）</label>
      <label class="row" style="gap:4px"><input type="radio" name="th-mode" data-th="inherit" value="0" ${F.inherit ? '' : 'checked'} ${can ? '' : 'disabled'}> 本專案：離日期 ≤ <input type="number" min="0" max="90" data-th="days" value="${F.days ?? ''}" style="width:64px" ${F.inherit || !can ? 'disabled' : ''}> 天，且未完成 &gt; <input type="number" min="0" max="100" data-th="pct" value="${F.pct ?? ''}" style="width:64px" ${F.inherit || !can ? 'disabled' : ''}> %</label>
      <span class="hint">留空＝停用這條</span>
      <span style="flex:1"></span>${can ? `<button class="btn pri" data-a="th-save" data-p="${key}">儲存</button><button class="btn sm ghost" data-a="th-race" data-p="${key}" title="情境：別人剛改過這個專案的門檻">模擬別人剛改過</button>` : '<span class="hint">需要 manage_versions</span>'}</div></div>`;
  return h.replace('<div class="card"><div class="k" style="margin-bottom:6px">新增里程碑</div>', `${hist ? `<div class="card"><div class="k" style="margin-bottom:4px">最近變更</div>${hist}</div>` : ''}${thCard}<div class="card"><div class="k" style="margin-bottom:6px">新增里程碑</div>`);
};
const _healthChip = healthChip;
const healthTitle = s => s && s.reasons && s.reasons.length ? s.reasons.join('；') : '';
// 時程與里程碑表格：健康度 chip 加上「為什麼」（title）
const _pgTimeline = pgTimeline;
pgTimeline = function(key){
  let h = _pgTimeline(key);
  (D.MS[key] || []).forEach(m => { const s = msStats(m, key); if (s.reasons.length) h = h.replace(`title="${esc(m.name)} ${m.date}"`, `title="${esc(m.name)} ${m.date}｜${esc(healthTitle(s))}"`); });
  const extra = (D.MS[key] || []).filter(m => m.status !== 'closed').map(m => [m, msStats(m, key)]).filter(([m, s]) => s.health === 'risk' && !s.overdue.length && !s.late.length);
  if (extra.length) h = h.replace('<div class="card tl">', `<div class="banner w" style="display:block">${extra.map(([m, s]) => `<div><b>${esc(m.name)}</b>（${m.date}）：${esc(s.reasons.join('；'))}</div>`).join('')}<div class="hint">門檻可在「里程碑」分頁調整。</div></div><div class="card tl">`);
  return h;
};

// ---------- 版本命令的執行（direct、核准套用、undo 共用；§17.9a） ----------
const vProject = o => o.kind === 'create_version' ? o.project : Object.keys(D.MS).find(p => D.MS[p].some(m => m.id === o.target));
const nameKey = s => (s || '').normalize('NFC').trim().toLowerCase();
function vCheck(o){
  const p = vProject(o); const xs = D.MS[p] || [];
  const name = o.kind === 'create_version' ? o.fields.name : (o.fields || {}).name;
  if (name){ const hit = xs.find(m => nameKey(m.name) === nameKey(name) && m.id !== o.target); if (hit) return {code:'version_name_taken', details:{version_id:hit.id, name:hit.name}}; }
  if (o.kind === 'update_version'){ const m = msOf(o.target); if (o.lock != null && m.lock !== o.lock) return {code:'conflict', details:{key:m.name, expected:o.lock, current:m.lock}}; }
  return null;
}
const _runOps = runOps;
runOps = function(ops, meta){
  if (!ops.some(o => VKINDS.includes(o.kind))) return _runOps(ops, meta);
  const o = ops[0]; const p = vProject(o);
  const ex = {id:'ex' + (++EXEC_SEQ), at:clock(), t:Date.now(), actor:meta.actor, onBehalf:meta.onBehalf || null, approver:meta.approver || null, via:meta.via, ctx:meta.ctx || null, csNo:meta.csNo || null, title:meta.title || '', changes:[], created:[], notes:[], dups:[], trans:[], vchanges:[], vcreated:[], reverts:null, revertedBy:null, undone:{}};
  const logE = (m, t) => m.log.push({at:ex.at, by:meta.actor, approver:meta.approver || null, via:meta.via, exec:ex.id, t:t + (meta.via === 'changeset' ? (meta.csNo ? `（提案 #${meta.csNo}）` : `（在${meta.where || ' AI 面板'}核准）`) : '')});
  let m;
  if (o.kind === 'create_version'){
    m = {id:'ms-' + Date.now(), name:o.fields.name, date:o.fields.effective_date, status:'open', desc:o.fields.description || '', lock:0, origin:{}, createdBy:meta.actor, onBehalf:meta.onBehalf || null, log:[]};
    ['name', 'date', 'desc', 'status'].forEach(f => m.origin[f] = ex.id);
    D.MS[p].push(m); ex.vcreated.push(m.id); logE(m, `建立（${m.date}）`);
  } else {
    m = msOf(o.target); const parts = [];
    const map = {name:'name', effective_date:'date', description:'desc', status:'status'};
    for (const [k, val] of Object.entries(o.fields)){
      const f = map[k]; if (same(m[f], val)) continue;
      ex.vchanges.push({ms:m.id, f, before:m[f], after:val, prevOrigin:m.origin[f] || null});
      parts.push(`${vfName(f)} ${vfVal(f, m[f])} → ${vfVal(f, val)}`); m[f] = val; m.origin[f] = ex.id;
    }
    if (parts.length){ m.lock++; logE(m, parts.join('；')); }
  }
  D.execs.push(ex);
  return {effects:[{key:m.name, version_id:m.id, project:p, kind:o.kind === 'create_version' ? 'created' : 'updated', lock:m.lock}], exec:ex};
};
const vfName = f => ({name:'名稱', date:'日期', desc:'描述', status:'狀態'})[f];
const vfVal = (f, v) => f === 'status' ? (msStatusLabel[v] || v).replace(/（.*）/, '') : (v || '（空）');

// 人在里程碑頁直接改：算人寫的（來源換成人、lock_version + 1）
document.addEventListener('change', e => {
  const t = e.target;
  if (t.dataset.ms){ const m = msOf(t.dataset.id); if (m){ m.lock++; m.origin[t.dataset.ms === 'date' ? 'date' : 'status'] = 'human'; const L = m.log[m.log.length - 1]; if (L && !L.via){ L.via = 'human_direct'; L.t = (t.dataset.ms === 'date' ? '日期 → ' : '狀態 → ') + (t.dataset.ms === 'date' ? t.value : vfVal('status', t.value)); } render(); } }
  if (t.dataset.th){ const F = UI.thForm; if (t.dataset.th === 'inherit') F.inherit = t.value === '1'; else F[t.dataset.th] = t.value === '' ? null : +t.value; render(); }
});

// ---------- undo（§17.9a、§17.13） ----------
function canUndoV(exId){ const ex = D.execs.find(x => x.id === exId); return ex && ex.via !== 'undo' && undoItems(ex).some(i => i.ok); }
const _undoItems = undoItems;
undoItems = function(ex){
  const items = _undoItems(ex);
  // issue 的里程碑還原＝新指定：原里程碑要仍是進行中（R1）
  items.forEach(i => { if (i.kind === 'field' && i.c.f === 'milestone' && i.ok && i.c.before){ const m = msOf(i.c.before); if (!m || m.status !== 'open'){ i.ok = false; i.why = `原里程碑「${esc(m ? m.name : '?')}」已鎖定或關閉，不能再排入（value_no_longer_valid）`; } } });
  for (const c of ex.vchanges || []){
    const m = msOf(c.ms); let ok = true, why = '';
    if (ex.undone['v:' + c.ms + c.f]){ ok = false; why = '已回復'; }
    else if (!m){ ok = false; why = '里程碑已刪除'; }
    else if (m.origin[c.f] !== ex.id){ ok = false; why = same(m[c.f], c.after) ? '目前值相同，但之後別人又設過一次（是別人的決定，不動）' : `之後已被改成「${esc(vfVal(c.f, m[c.f]))}」`; }
    else if (c.f === 'name' && (D.MS[vProject({kind:'update_version', target:c.ms})] || []).some(x => x.id !== c.ms && nameKey(x.name) === nameKey(c.before))){ ok = false; why = '舊名稱已被別的里程碑使用'; }
    items.push({id:'v:' + c.ms + c.f, kind:'vfield', c, ok, why, auto:true, label:`里程碑「${esc(m ? m.name : '')}」${vfName(c.f)}：${esc(vfVal(c.f, c.after))} → <b>${esc(vfVal(c.f, c.before))}</b>`});
  }
  for (const id of ex.vcreated || []){
    const m = msOf(id); const p = vProject({kind:'update_version', target:id});
    let ok = !!m && !ex.undone['vc:' + id], why = m ? '' : '已刪除';
    if (ok && ['name', 'date', 'desc', 'status'].some(f => m.origin[f] !== ex.id)){ ok = false; why = '建立之後有人改過（changed_since）'; }
    if (ok && msStats(m, p).total){ ok = false; why = '還有你看得到的單在裡面（in_use）'; }
    items.push({id:'vc:' + id, kind:'vcreated', id, ok, why, auto:true, label:`刪除 agent 建立的里程碑「${esc(m ? m.name : id)}」`});
  }
  return items;
};
const _applyUndo = applyUndo;
applyUndo = function(ex, ids){
  const vids = ids.filter(i => i.startsWith('v'));
  const r = _applyUndo(ex, ids.filter(i => !i.startsWith('v')));
  if (!vids.length) return r;
  const hx = {id:'ex' + (++EXEC_SEQ), at:clock(), t:Date.now(), actor:ME, via:'undo', ctx:ex.ctx, title:`回復 ${AGENTS[ex.actor] ? AGENTS[ex.actor].handle : ''} 的 ${ex.id}`, changes:[], created:[], notes:[], dups:[], trans:[], vchanges:[], vcreated:[], reverts:ex.id, revertedBy:null, undone:{}};
  const done = [];
  for (const it of undoItems(ex).filter(i => vids.includes(i.id))){
    if (!it.ok){ r.skipped.push(it); continue; }
    if (it.kind === 'vfield'){ const m = msOf(it.c.ms); m[it.c.f] = it.c.before; m.origin[it.c.f] = it.c.prevOrigin; m.lock++; m.log.push({at:hx.at, by:ME, via:'undo', exec:hx.id, t:`${vfName(it.c.f)} ${vfVal(it.c.f, it.c.after)} → ${vfVal(it.c.f, it.c.before)}（回復 agent 的變更）`}); }
    if (it.kind === 'vcreated'){ const p = vProject({kind:'update_version', target:it.id}); const m = msOf(it.id); m.deleted = true; m.status = 'closed'; D.MS[p] = D.MS[p].filter(x => x.id !== it.id); (D.MS._deleted = D.MS._deleted || []).push(m); }   // 軟刪除：歷史仍查得到名稱
    ex.undone[it.id] = hx.id; done.push(it); r.done.push(it);
  }
  if (done.length){ hx.summary = done.map(i => i.label).join('<br>'); D.execs.push(hx); }
  if (undoItems(ex).every(i => !i.ok)) ex.revertedBy = hx.id;
  return r;
};
const _execLines = execLines;
execLines = function(ex){
  const L = [_execLines(ex)].filter(Boolean);
  (ex.vcreated || []).forEach(id => { const m = msOf(id); L.push(`建立里程碑「<b>${esc(m ? m.name : id)}</b>」${m ? `（${m.date}）` : ''}${m && m.deleted ? ' <span class="chip c-dim">已刪除</span>' : ''}`); });
  (ex.vchanges || []).forEach(c => { const m = msOf(c.ms); L.push(`里程碑「${esc(m ? m.name : '')}」${vfName(c.f)} <s class="dim">${esc(vfVal(c.f, c.before))}</s> → <b>${esc(vfVal(c.f, c.after))}</b>`); });
  return L.join('<br>');
};

// ---------- 送審（政策是「需要核准」時）：ChangeSet 目標可以是里程碑 ----------
const _opProject = opProject;
opProject = o => VKINDS.includes(o.kind) ? vProject(o) : _opProject(o);
const _opTargets = opTargets;
opTargets = v => _opTargets({ops:v.ops.filter(o => !VKINDS.includes(o.kind))});
const _validate = validate;
validate = function(cs){
  const r = _validate(cs);
  curVer(cs).ops.filter(o => VKINDS.includes(o.kind)).forEach(o => { const e = vCheck(o); if (e){ if (e.code === 'conflict'){ r.checks.precondition = 'conflict'; r.blocking.push({code:'conflict', field:'里程碑', msg:`里程碑「${e.details.key}」在提案後被改過（第 ${e.details.expected} → ${e.details.current} 版），請 agent 重新提案`}); } else r.blocking.push({code:e.code, field:'名稱', msg:`同專案已有「${e.details.name}」`}); r.status = 'blocked'; } });
  return r;
};
const _reviewModes = reviewModes;
reviewModes = function(cs){ const r = _reviewModes(cs); r.why = r.why.map(w => w || '里程碑操作'); return r; };
const vOpLine = o => o.kind === 'create_version'
  ? `<div><span class="chip c-prop">建立里程碑</span> ${esc(o.project)}：<b>${esc(o.fields.name)}</b> <span class="dim">日期</span> ${esc(o.fields.effective_date || '未定')}${o.fields.description ? `<div class="muted">${esc(o.fields.description)}</div>` : ''}</div>`
  : `<div><span class="chip c-prop">修改里程碑</span> <b>${esc((msOf(o.target) || {}).name || o.target)}</b>：${Object.entries(o.fields).map(([k, v]) => `${({name:'名稱', effective_date:'日期', description:'描述', status:'狀態'})[k]} → <b>${esc(k === 'status' ? vfVal('status', v) : v)}</b>`).join('；')}</div>`;
const _opLines = opLines;
opLines = function(cs, v, compact){ const vo = v.ops.filter(o => VKINDS.includes(o.kind)); return vo.length ? vo.map(vOpLine).join('') : _opLines(cs, v, compact); };
const _csTargetsLabel = csTargetsLabel;
csTargetsLabel = function(cs){ const o = curVer(cs).ops.find(x => VKINDS.includes(x.kind)); return o ? `<span class="dim">里程碑</span> ${esc(vProject(o))} ${esc(o.kind === 'create_version' ? o.fields.name : (msOf(o.target) || {}).name || '')}` : _csTargetsLabel(cs); };

// ---------- MCP：command_submit 接受版本命令；version_list（§17.9、§17.10） ----------
const _commandSubmit = commandSubmit;
commandSubmit = function(a){
  const ops = a.command.ops;
  if (!ops.some(o => VKINDS.includes(o.kind))) return _commandSubmit(a);
  const c = MCP._ctx();
  const fp = JSON.stringify([a.command, !!a.require_review]);
  const prev = D.opsLog[a.operation_id];
  if (prev){ if (prev.fp !== fp) throw new McpError('idempotency_key_reused'); return Object.assign({}, prev.res, {replayed:true}); }
  if (ops.length !== 1) throw new McpError('invalid_request', {reason:'里程碑命令要單獨一份文件、只有一個 op'});
  const ctl = ctlOf(c);
  if (ctl.suspended) throw new McpError('agent_suspended', {scope:'context', reason:ctl.reason});
  const o = Object.assign({}, ops[0]); const proj = vProject(o);
  if (!proj || !MCP._canRead(c, proj)) throw new McpError(o.kind === 'update_version' ? 'not_found' : 'delegation_unavailable', {project:proj || o.project});
  if (!canPublishTo(Object.keys(c.R), proj)) throw new McpError('audience_expansion_forbidden', {kind:'cross_project', destination:proj, sources:Object.keys(c.R)});
  if (!(MANAGE_VERSIONS[proj] || []).includes(ME)) throw new McpError('forbidden_op', {reasons:['委派者在 ' + proj + ' 沒有 manage_versions']});
  if (o.kind === 'update_version') o.lock = (a.command.preconditions || {})[o.target];
  const dg = c.delegations.find(d => d.project === proj && d.state === 'active');
  const dec = policyDecision(dg, [o]);
  if (dec.mode === 'off'){ noteBlocked(c, 'forbidden_op', dec.why.join('、')); throw new McpError('forbidden_op', {reasons:dec.why.map(w => w.replace('不允許', '未加入或不允許'))}); }
  const err = vCheck(o); if (err) throw new McpError(err.code, err.details);
  let res;
  if (dec.mode === 'review' || a.require_review){
    const cs = makeChangeset({ctx:c, project:proj, title:a.command.title, ops:[o], pre:{}, evidence:[], group:null});
    cs._op = a.operation_id;
    res = {outcome:'review_required', reason:dec.mode === 'review' ? 'policy' : 'requested', policy_reasons:dec.why.length ? dec.why : undefined, changeset_id:cs.id, version:1, proposal_hash:curVer(cs).hash, review_url:`${ORIGIN}/changesets/${cs.id}?version=1`, next_actions:['ask_human'], _no:cs.no};
  } else {
    const now = Date.now();
    if (o.kind === 'create_version'){ ctl.vcreate = (ctl.vcreate || []).filter(t => now - t < 3600e3); if (ctl.vcreate.length >= 10) throw new McpError('quota_exceeded', {limit:'每小時建立 10 個里程碑'}); }
    else { ctl.vupd = ctl.vupd || {}; const arr = ctl.vupd[o.target] = (ctl.vupd[o.target] || []).filter(t => now - t < 600e3); if (arr.length >= 5){ suspend(c, `同一個里程碑在 10 分鐘內被修改超過 5 次`); noteBlocked(c, 'agent_suspended', ctl.reason); throw new McpError('agent_suspended', {scope:'context', reason:ctl.reason}); } }
    const r = runOps([o], {actor:c.agent, onBehalf:'u1', via:'agent_direct', ctx:c.id, title:a.command.title});
    if (o.kind === 'create_version') ctl.vcreate.push(now); else ctl.vupd[o.target].push(now);
    res = {outcome:'applied', execution_id:r.exec.id, effects:r.effects.map(e => ({version:e.key, version_id:e.version_id, kind:e.kind, lock_version:e.lock})), next_actions:[]};
  }
  D.opsLog[a.operation_id] = {fp, res};
  return res;
};
MCP.command_submit = a => commandSubmit(a);
function nextVersion(p){
  const open = (D.MS[p] || []).filter(m => m.status === 'open');
  const od = open.filter(m => m.date && dnum(m.date) < dnum(TODAY));
  const up = open.filter(m => m.date && dnum(m.date) >= dnum(TODAY)).sort((a, b) => dnum(a.date) - dnum(b.date) || nameKey(a.name).localeCompare(nameKey(b.name)));
  const brief = m => ({id:m.id, name:m.name, effective_date:m.date});
  if (od.length) return {outcome:'ambiguous', reason:'overdue_open_exists', candidates:[...od.map(m => Object.assign(brief(m), {overdue:true})), ...up.filter(m => up[0] && m.date === up[0].date).map(brief)]};
  if (!up.length) return {outcome:'none', undated:open.filter(m => !m.date).map(brief)};
  const same0 = up.filter(m => m.date === up[0].date);
  return same0.length > 1 ? {outcome:'ambiguous', reason:'same_date', candidates:same0.map(brief)} : {outcome:'resolved', version:brief(up[0])};
}
MCP.version_list = function(a){
  const c = MCP._ctx(); const p = a.project;
  if (!MCP._canRead(c, p)) throw new McpError('not_found', {project:p});
  let xs = (D.MS[p] || []).filter(m => (a.status || ['open', 'locked']).includes(m.status));
  if (a.name) xs = xs.filter(m => nameKey(m.name) === nameKey(a.name));
  const out = {today:TODAY, versions:xs.map(m => { const s = msStats(m, p); return {id:m.id, name:m.name, effective_date:m.date, status:m.status, lock_version:m.lock, stats:{total:s.total, closed:s.done, open:s.total - s.done, overdue:s.overdue.length, late_for_version:s.late.length}, health:s.health === 'risk' ? 'at_risk' : s.health === 'ok' ? 'on_track' : s.health, health_reasons:s.reasons}; })};
  if (a.next) out.next = nextVersion(p);
  return out;
};
const _describe = MCP.describe;
MCP.describe = function(a){
  const r = _describe(a);
  if (r.unsupported){ r.unsupported = r.unsupported.map(x => x === 'fixed_version_id' ? 'version_delete（刪除里程碑只限人）' : x).concat(['relative_date']); r.fields.fixed_version_id = {type:'version_ref', options_source:'version_list(status: open)'}; r.fields.start_date = {type:'date'}; r.fields.due_date = {type:'date'}; r.operations = r.operations.concat(VKINDS); }
  return r;
};

// ---------- 主控台劇本（§17.11 用法 5 改寫；新增 12、13） ----------
SCRIPTS[5] = async function(){
  const v = await call('version_list', {project:'AUTH', next:true}); if (stopIfErr(v)) return;
  const n = v.res.next;
  if (n.outcome !== 'resolved'){ say(n.outcome === 'ambiguous' ? `「下個版本」有歧義：${n.candidates.map(x => `${esc(x.name)}（${x.effective_date}${x.overdue ? '，已過期但還開著' : ''}）`).join('、')}。要排進哪一個？我不自己挑。` : '目前沒有排了日期的進行中里程碑，要排進哪一個？'); return; }
  const ver = n.version;
  const g = await call('get_many', {keys:['AUTH-125', 'AUTH-126'], fields:['lock_version', 'fixed_version']}); if (stopIfErr(g)) return;
  const done = [], same0 = [], fail = [];
  for (const it of g.res.items){
    if (it.error){ fail.push(`${it.key} 我看不到`); continue; }
    if (D.I[it.key].milestone === ver.id){ same0.push(it.key); continue; }
    const p = await call('command_submit', {operation_id:'v5-' + it.key, command:{title:`${it.key} 排進 ${ver.name}`, ops:[{kind:'update_fields', target:it.key, fields:{milestone:ver.id}}], preconditions:{[it.key]:it.lock_version}}});
    if (p.err){ if (p.err.code === 'forbidden_op'){ say(`你的委派還沒加入「里程碑」欄位（${esc((p.err.details.reasons || []).join('、'))}）。這是 U39 之後新增的欄位，既有委派不會自動加入；請到 <a href="#/agents">Agent 委派</a> 按「有新操作可以加入」。其餘的我先不動。`); return; } stopD20(p); return; }
    p.res.outcome === 'applied' ? done.push(it.key) : fail.push(`${it.key} 已送審（提案 #${p.res._no}）`);
  }
  say(`下個版本是 <b>${esc(ver.name)}</b>（${ver.effective_date}）。${done.length ? `已把 ${done.map(keyLink).join('、')} 排進去（直接寫入）。` : ''}${same0.length ? `${same0.join('、')} 本來就在 ${esc(ver.name)}。` : ''}${fail.length ? fail.join('；') + '。' : ''}`);
};
USAGES.splice(5, 0,
  {id:12, label:'12 建里程碑', text:'在 AUTH 建一個里程碑「v2.2 管理後台」，日期 1/15'},
  {id:13, label:'13 延後里程碑', text:'AUTH 的 v2.0 延到 11/27'});
SCRIPTS[12] = async function(){
  const v = await call('version_list', {project:'AUTH', name:'v2.2 管理後台', status:['open', 'locked', 'closed']}); if (stopIfErr(v)) return;
  if (v.res.versions.length){ say(`AUTH 已經有「${esc(v.res.versions[0].name)}」了，我不另外建同名的。`); return; }
  const p = await call('command_submit', {operation_id:'v12-a', command:{title:'建立里程碑 v2.2 管理後台', ops:[{kind:'create_version', project:'AUTH', fields:{name:'v2.2 管理後台', effective_date:'2027-01-15', description:'管理後台第一版'}}]}});
  if (p.err){ if (p.err.code === 'forbidden_op'){ say(`你的委派還沒加入「建立里程碑」（${esc((p.err.details.reasons || []).join('、'))}）。這是新操作，既有委派不會自動加入；請到 <a href="#/agents">Agent 委派</a> 按「有新操作可以加入」後再叫我一次。`); return; } if (p.err.code === 'version_name_taken'){ say(`名稱撞到既有的「${esc(p.err.details.name)}」，我不自動改名。要用既有的，還是換個名稱？`); return; } stopD20(p); return; }
  submitted(p, `已建立里程碑 <b>v2.2 管理後台</b>（2027-01-15），在 <a href="#/project/AUTH/milestones">AUTH 里程碑</a>`, `已提案建立里程碑（#${p.res._no}）。`);
};
SCRIPTS[13] = async function(){
  const v = await call('version_list', {project:'AUTH', name:'v2.0 SSO 上線'}); if (stopIfErr(v)) return;
  const m = v.res.versions[0]; if (!m){ say('AUTH 沒有進行中的 v2.0。'); return; }
  const p = await call('command_submit', {operation_id:'v13-a', command:{title:'v2.0 延到 11/27', ops:[{kind:'update_version', target:m.id, fields:{effective_date:'2026-11-27'}}], preconditions:{[m.id]:m.lock_version}}});
  if (p.err){ if (p.err.code === 'forbidden_op'){ say(`你的委派還沒加入「修改／關閉里程碑」。請到 <a href="#/agents">Agent 委派</a> 加入後再叫我一次。`); return; } if (p.err.code === 'conflict'){ say('v2.0 在我讀取後被改過，沒有寫入；我重讀後再問你。'); return; } stopD20(p); return; }
  const s = msStats(msOf(m.id), 'AUTH');
  submitted(p, `已把 <b>${esc(m.name)}</b> 從 ${m.effective_date} 延到 2026-11-27。單的日期不會自動跟著改${s.late.length ? `；仍有 ${s.late.map(i => keyLink(i.key)).join('、')} 晚於新日期` : ''}`, `已提案（#${p.res._no}）。`);
};

// ---------- 右欄內建 AI：建立、延後里程碑 ----------
const _aiAnswer2 = aiAnswer2;
aiAnswer2 = function(q, s){
  const proj = s.kind === 'project' ? s.key : s.kind === 'issue' ? D.I[s.key].project : 'AUTH';
  const md = q.match(/(\d{1,2})\/(\d{1,2})/); const date = md ? `${+md[1] >= 11 ? 2026 : 2027}-${md[1].padStart(2, '0')}-${md[2].padStart(2, '0')}` : null;
  if (/(建|新增).*里程碑/.test(q)){
    const nm = (q.match(/[「"](.+?)[」"]/) || [])[1];
    if (!nm || !date) return {t:'要建哪個里程碑？請說名稱和日期，例如：建立里程碑「v2.2 管理後台」1/15。'};
    return aiVPropose({kind:'create_version', project:proj, fields:{name:nm, effective_date:date}}, `在 ${proj} 建立里程碑「${esc(nm)}」（${date}）：`);
  }
  if (/(延|改).*(到|成)/.test(q) && /v\d/i.test(q) && date){
    const m = (D.MS[proj] || []).find(x => x.status !== 'closed' && q.toLowerCase().includes(x.name.split(' ')[0].toLowerCase()));
    if (!m) return {t:`${proj} 找不到這個里程碑。`};
    return aiVPropose({kind:'update_version', target:m.id, fields:{effective_date:date}, lock:m.lock}, `把「${esc(m.name)}」從 ${m.date} 改到 ${date}（單的日期不會自動跟著改）：`);
  }
  return _aiAnswer2(q, s);
};
function aiVPropose(o, lead){
  const err = vCheck(o); if (err) return {t:err.code === 'version_name_taken' ? `已經有「${esc(err.details.name)}」了，不另外建同名的。` : '里程碑剛被改過，請再問一次。'};
  const id = 'vp' + (++AI_SEQ); const m = {t:lead, vop:o, vid:id};
  if ((UI.aiMode || 'review') === 'direct'){ const r = runOps([o], {actor:'ag-ai', onBehalf:'u1', via:'agent_direct', ctx:'ctx-ai', title:'內建 AI：里程碑'}); m.vexec = r.exec.id; }
  return m;
}
const _chatPanel = chatPanel;
chatPanel = function(s){
  (D.aiLog || []).forEach(m => {
    if (!m.vop || m._vt) return; m._vt = m.t;
  });
  (D.aiLog || []).forEach(m => {
    if (!m.vop) return;
    const st = m.vexec ? `<span class="chip c-ok">${m.vdirect === false ? '已套用' : '已寫入'}</span>${canUndoV(m.vexec) ? ` <button class="btn sm" data-a="undo-open" data-ex="${m.vexec}">回復…</button>` : ''}` : m.vdrop ? '<span class="chip c-dim">已略過</span>' : `<button class="btn sm pri" data-a="ai-vapply" data-m="${m.id}">核准並套用</button><button class="btn sm ghost" data-a="ai-vdrop" data-m="${m.id}">不要</button>`;
    m.t = `${m._vt}<div class="aicard"><div style="font-size:12.5px">${vOpLine(m.vop)}</div><div class="row" style="margin-top:6px;gap:6px">${st}</div></div>`;
  });
  let h = _chatPanel(s);
  if (s.kind === 'project') h = h.replace('<button class="btn sm" data-a="ai-chip" data-q="里程碑進度與風險">', '<button class="btn sm" data-a="ai-chip" data-q="建立里程碑「v2.2 管理後台」1/15">建立里程碑「v2.2 管理後台」1/15</button><button class="btn sm" data-a="ai-chip" data-q="v2.0 延到 11/27">v2.0 延到 11/27</button><button class="btn sm" data-a="ai-chip" data-q="里程碑進度與風險">');
  return h;
};

// ---------- 動作 ----------
const _actTeam2 = actTeam;
actTeam = function(a, el){
  const d = el.dataset;
  switch (a){
    case 'pol-addnew': { const g = findDg(d.dg); missingPol(g.policy).forEach(([kind, k]) => g.policy[kind][k] = 'direct'); g.policy.version++; toast(`已加入新操作（預設直接寫入，policy_version ${g.policy.version}）；可在「設定」改成需要核准或不允許`, 'ok'); render(); return true; }
    case 'pol-addone': { const g = findDg(d.dg); g.policy[d.kind][d.k] = 'direct'; g.policy.version++; toast('已加入', 'ok'); render(); return true; }
    case 'th-save': {
      const p = d.p, F = UI.thForm, pr = D.sched.proj[p];
      if (pr.lock !== F.lock){ toast('別人剛改過這個專案的門檻（conflict），已載入最新值，請再確認一次', 'bad'); UI.thForm = null; render(); return true; }
      if (!F.inherit && ((F.days != null && (F.days < 0 || F.days > 90)) || (F.pct != null && (F.pct < 0 || F.pct > 100)))){ toast('天數 0–90、百分比 0–100', 'bad'); return true; }
      Object.assign(pr, F.inherit ? {inherit:true, days:null, pct:null} : {inherit:false, days:F.days, pct:F.pct}); pr.lock++; UI.thForm = null;
      toast('已儲存健康度門檻', 'ok'); render(); return true;
    }
    case 'th-race': { const pr = D.sched.proj[d.p]; pr.lock++; Object.assign(pr, {inherit:false, days:3, pct:80}); toast('（情境）Carol 剛把門檻改成 3 天／80%', 'w'); return true; }
    case 'ai-vapply': { const m = D.aiLog.find(x => x.id === d.m); const e = vCheck(m.vop); if (e){ toast(e.code === 'conflict' ? '里程碑剛被改過，請重新問 AI' : '同名里程碑已存在', 'bad'); return true; } const r = runOps([m.vop], {actor:'ag-ai', onBehalf:'u1', approver:'u1', via:'changeset', ctx:'ctx-ai', where:'右欄 AI 對話', title:'內建 AI：里程碑'}); m.vexec = r.exec.id; m.vdirect = false; toast('已套用', 'ok'); render(); return true; }
    case 'ai-vdrop': { const m = D.aiLog.find(x => x.id === d.m); m.vdrop = true; render(); return true; }
  }
  return _actTeam2(a, el);
};
// 刪除里程碑：只看你看得到的單（R5）
const _actTeam3 = actTeam;
actTeam = function(a, el){
  if (a === 'ms-del'){ const m = msOf(el.dataset.id); if (msStats(m, el.dataset.p).total){ toast('還有你看得到的單在裡面，請先移走（version_in_use）', 'bad'); return true; } }
  return _actTeam3(a, el);
};

// ---------- 首頁小工具：健康度原因 ----------
const _wTeam2 = wTeam;
wTeam = function(it){
  let h = _wTeam2(it);
  if (it.id === 'milestone') Object.entries(D.MS).forEach(([p, xs]) => xs.forEach(m => { const s = msStats(m, p); if (s.reasons.length) h = h.replace(`<b>${p} ${esc(m.name)}</b></a>`, `<b>${p} ${esc(m.name)}</b></a><span class="dim" title="${esc(healthTitle(s))}" style="font-size:11.5px;margin-left:4px">ⓘ</span>`); }));
  return h;
};

// ---------- 流程 ----------
FLOWS.unshift(
  {id:'f27', t:'agent 建立、修改里程碑', ref:'使用者 2026-11-07；U41；02 §17.9a', steps:['主控台按「12 建里程碑」：既有委派還沒加入新操作 → forbidden_op，agent 不繞過','Agent 委派 → AUTH 的操作欄按「有 3 個新操作可以加入」（預設直接寫入）','再按「12 建里程碑」→ 直接建立 v2.2；AUTH 里程碑頁「最近變更」顯示 claude-code@alice〔agent〕','按「13 延後里程碑」→ v2.0 延到 11/27；按「回復…」把日期改回來','按「5 改下個版本」→ version_list 解析出 v2.0，逐張排入','右欄（AUTH 時程頁）點「建立里程碑…」→ 核准並套用；或切「直接寫入」'], go:'/agents', agent:true},
  {id:'f28', t:'里程碑健康度門檻', ref:'使用者 2026-11-07；U43；02 §17.12', steps:['AUTH → 里程碑：「健康度門檻」區塊，預設跟組織一樣（7 天／50%）','改成本專案 14 天／30% → 儲存 → v2.0 因「接近日期且未完成多」標 At risk，時程頁上方說明原因','按「模擬別人剛改過」再儲存 → conflict，載入最新值','兩格留空＝停用這條；只剩逾期與晚於里程碑兩條規則'], go:'/project/AUTH/milestones'},
);

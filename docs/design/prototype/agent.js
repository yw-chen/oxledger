// oxledger 原型：外部 agent（MCP）主控台、流程導覽、情境控制
'use strict';

// ---------- MCP 工具（照設計 02 §8.8、03 §3.6 簡化） ----------
const NEXT = {
  invalid_value:['fix_input'], ambiguous_reference:['fix_input','ask_human'], invalid_transition:['fix_input','ask_human'],
  conflict:['refresh_and_revise'], precondition_failed:['refresh_and_revise'], version_superseded:['refresh_and_revise'],
  not_found:['fix_input','stop'], audience_expansion_forbidden:['ask_human','stop'], unsupported_operation:['ask_human','stop'],
  delegation_unavailable:['ask_human','stop'], context_unavailable:['ask_human','stop'], unauthenticated:['ask_human'],
  temporarily_unavailable:['retry_same_request'],
  agent_suspended:['ask_human','stop'], forbidden_op:['ask_human','stop'], rate_limited:['wait_and_retry'], quota_exceeded:['ask_human','stop'], idempotency_key_reused:['fix_input'], invalid_value:['fix_input'],
};
class McpError extends Error { constructor(code, details){ super(code); this.code = code; this.details = details || {}; } }
function mErr(code, message, details){ return {code, message, next_actions:NEXT[code] || ['stop'], details:details || {}}; }

const MCP = {
  _ctx(){
    const c = activeCtx();
    if (!c || !c.tokens.some(t => t.state === 'active')) throw new McpError('unauthenticated');
    if (!ctxUsable(c)) throw new McpError('context_unavailable', {kind:'quarantined', project:ctxBrokenBy(c)[0]});
    return c;
  },
  _canRead(c, project){ return c.delegations.some(d => d.project === project && d.state === 'active'); },
  _issue(c, key){ const is = D.I[key]; if (!is || !MCP._canRead(c, is.project) || is.isPrivate) throw new McpError('not_found', {key}); return is; },
  describe(a){
    const c = MCP._ctx();
    if (a.scope === 'context'){
      const R = Object.keys(c.R); const inter = intersectAud(R);
      return {context_id:c.id, source_projects:R, projects:R.map(p => ({project:p, readable:MCP._canRead(c, p), public_content:subset(D.P[p].audience, inter) ? 'same_or_narrower' : 'blocked'}))};
    }
    if (!MCP._canRead(c, a.project)) throw new McpError('not_found', {project:a.project});
    return {project:a.project, tracker:a.tracker || 'Bug', fields:{subject:{required:true, type:'string', max:255}, description:{type:'text'}, priority_id:{options:PRIORITIES}, assigned_to_id:{options_source:'resolve(kind:principal, purpose:assignee)'}, 'cf:Severity':{type:'list', options:SEVERITIES}, 'cf:估時':{type:'float'}},
      initial_status:'新建', is_private_default:false, operations:['create_issue','update_fields','transition','add_note','duplicate_of'],
      unsupported:['fixed_version_id','parent_id','project_id（搬專案）','delete_issue','per_item 執行模式']};
  },
  search(a){
    const c = MCP._ctx();
    if (a.project && !MCP._canRead(c, a.project)) throw new McpError('not_found', {project:a.project});
    const r = search({q:a.q || '', filter:a.filter || null, project:a.project, limit:a.limit || 5, viewer:'agent'});
    return {outcome:r.lagging && a.consistent_after ? 'search_lagging' : 'ok', complete:!r.more, items:r.items.map((x, i) => {
      const it = {key:x.is.key, subject:x.is.subject, status:x.is.status, rank:i + 1};
      if (a.fields && a.fields.includes('lock_version')) it.lock_version = x.is.lock;
      if (a.fields && a.fields.includes('assignee')) it.assignee = x.is.assignee ? U(x.is.assignee).login : null;
      if (a.explain !== 'none' && x.ev.length) it.evidence = x.ev.slice(0, 1).map(e => ({field:e.field, text:e.quote || e.text, strength:e.strength, citation_id:citationFor(x.is.key, e.field, e.quote || e.text)}));
      return it; }), next_cursor:r.more ? 'cur1.…' : null};
  },
  get(a){
    const c = MCP._ctx(); const is = MCP._issue(c, a.key);
    const out = {key:is.key, id:is.id, subject:is.subject, status:is.status, assignee:is.assignee ? uname(is.assignee) : null, priority:is.priority, lock_version:is.lock};
    if (a.fields && a.fields.includes('description')) out.description = is.description;
    out['cf:Severity'] = is.cf.severity;
    if ((a.include || []).includes('actions')) out.actions = {transitions:(WORKFLOW[is.status] || []), editable_fields:['assigned_to_id','priority_id','cf:Severity','cf:估時','subject','description'], notes:{can_add:true, can_set_private:true}, duplicate_of:{can_propose:true}};
    if ((a.include || []).includes('recent_notes')) out.recent_notes = is.notes.filter(n => !n.private).slice(-(a.note_limit || 3)).map(n => ({author:uname(n.author), at:n.at, text:n.text, citation_id:citationFor(is.key, 'note', n.text)}));
    if ((a.include || []).includes('recent_events')){ out.status_since = '2026-11-04T10:10+08:00'; out.last_activity_at = '2026-11-06T09:05+08:00'; }
    if (a.citations) out.citations = a.citations.map(f => ({field:f, citation_id:citationFor(is.key, f, is[f] || ''), text:is[f]}));
    return out;
  },
  get_many(a){ return {items:a.keys.map(k => { try { const g = MCP.get({key:k, fields:a.fields}); return {key:k, subject:g.subject, description:g.description, lock_version:g.lock_version}; } catch (e){ return {key:k, error:'not_found'}; } })}; },
  resolve(a){
    const c = MCP._ctx();
    const p = a.project; if (!MCP._canRead(c, p)) throw new McpError('not_found', {project:p});
    const cand = USERS.filter(u => D.P[p].assignable.includes(u.id) && (u.login.startsWith(a.text.toLowerCase()) || u.name.toLowerCase().startsWith(a.text.toLowerCase())));
    if (cand.length > 1) throw new McpError('ambiguous_reference', {candidates:cand.map(u => ({id:u.id, login:u.login, name:u.name}))});
    if (!cand.length) throw new McpError('not_found', {text:a.text});
    return {id:cand[0].id, login:cand[0].login, name:cand[0].name};
  },
  command_submit(a){ return commandSubmit(a); },
  changeset_propose(a){
    const c = MCP._ctx();
    const ops = a.proposal.ops.map(o => Object.assign({}, o));
    for (const o of ops){
      const proj = opProject(o);
      if (!MCP._canRead(c, proj)) throw new McpError('delegation_unavailable', {project:proj});
      if (!canPublishTo(Object.keys(c.R), proj)) throw new McpError('audience_expansion_forbidden', {kind:'cross_project', destination:proj, sources:Object.keys(c.R)});
      if (o.target){ const is = MCP._issue(c, o.target); if (o.kind === 'update_fields'){ o.before = {}; Object.keys(o.fields).forEach(f => o.before[f] = getField(is, f)); } if (o.kind === 'transition') o.from = is.status; }
      if (o.kind === 'duplicate_of'){ MCP._issue(c, o.source); MCP._issue(c, o.target); }
    }
    let group = a.group_id || null;
    if (a.group_label){ group = 'grp-' + (++GROUP_SEQ); D.groups.push({id:group, label:a.group_label, ctx:c.id}); }
    const project = opProject(ops[0]);
    const cs = makeChangeset({ctx:c, project, title:a.proposal.title, ops, pre:a.proposal.preconditions, evidence:(a.proposal.evidence || []).map(e => Object.assign({}, e)), group});
    const val = validate(cs);
    const t = opTargets(curVer(cs));
    return {changeset_id:cs.id, version:1, proposal_hash:curVer(cs).hash, state:'proposed', group_id:group || undefined, targets:t.map(k => ({key:k})),
      validation:{status:val.status, blocking:val.blocking.map(b => ({code:b.code, field:b.field}))}, review_url:`${ORIGIN}/changesets/${cs.id}?version=1`, expires_at:'2026-11-14T06:32:00Z', next_actions:val.status === 'blocked' ? ['fix_input'] : ['ask_human'], _no:cs.no};
  },
  changeset_get(a){
    MCP._ctx();
    const cs = D.changesets.find(x => x.id === a.changeset_id); if (!cs) throw new McpError('not_found');
    const e = cs.executions[cs.executions.length - 1];
    const val = validate(cs);
    return {changeset_id:cs.id, version:cs.cur, state:cs.state, validation:{status:val.status}, last_execution:e ? {status:e.status, error:e.error ? e.error.code : undefined, effects:e.effects, search:e.token ? {consistent_after:e.token} : undefined} : null};
  },
  changeset_list(a){ MCP._ctx(); return {items:D.changesets.filter(c => c.group === a.group_id).map(c => ({changeset_id:c.id, targets:opTargets(curVer(c)), state:c.state}))}; },
  changeset_revise(a){
    const c = MCP._ctx();
    const cs = D.changesets.find(x => x.id === a.changeset_id);
    if (a.expected_current_version !== cs.cur) throw new McpError('version_superseded');
    const v = curVer(cs);
    const ops = a.ops.map(o => Object.assign({}, o));
    for (const o of ops){ if (o.target){ const is = D.I[o.target]; if (o.kind === 'update_fields'){ o.before = {}; Object.keys(o.fields).forEach(f => o.before[f] = getField(is, f)); } if (o.kind === 'transition') o.from = is.status; } }
    const nv = {v:cs.versions.length + 1, ops, pre:a.preconditions, evidence:v.evidence, src:Object.keys(c.R), srcEpoch:Object.assign({}, c.R), by:'agent', at:clock()};
    nv.hash = hashOf(nv); cs.versions.push(nv); cs.cur = nv.v; cs.state = 'proposed';
    return {changeset_id:cs.id, version:nv.v, state:'proposed', validation:{status:validate(cs).status}, review_url:`${ORIGIN}/changesets/${cs.id}?version=${nv.v}`, next_actions:['ask_human']};
  },
  operation_get(a){ const L = D.opsLog[a.operation_id]; if (L) return {operation_id:a.operation_id, status:'committed', result:L.res}; const cs = D.changesets.find(x => x._op === a.operation_id); return cs ? {operation_id:a.operation_id, status:'committed', result:{changeset_id:cs.id, version:1, state:cs.state}} : {operation_id:a.operation_id, status:'not_found'}; },
};

// ---------- 主控台狀態 ----------
const AG = {log:[], busy:false, last:{}, asks:null};
const J = o => JSON.stringify(o, (k, v) => k.startsWith('_') ? undefined : v, 1).replace(/\n\s*/g, ' ').replace(/\{ /g, '{').replace(/ \}/g, '}');
const sleep = ms => new Promise(r => setTimeout(r, ms));
function say(t){ AG.log.push({k:'a', t}); renderAgent(); }
function sys(t){ AG.log.push({k:'sys', t}); renderAgent(); }
function user(t){ AG.log.push({k:'u', t}); renderAgent(); }
async function call(tool, args){
  const entry = {k:'call', tool, args, res:null, err:null};
  AG.log.push(entry); renderAgent();
  await sleep(450);
  try { entry.res = MCP[tool](args); } catch (e){ if (e instanceof McpError) entry.err = mErr(e.code, ({context_unavailable:'這個 agent 的工作脈絡已停用', not_found:'找不到，或不在委派範圍內', ambiguous_reference:'名稱對應到多個對象', audience_expansion_forbidden:'目的地的讀者比來源多', delegation_unavailable:'沒有這個專案的有效委派', unauthenticated:'token 無效', version_superseded:'版本已不是目前版本', agent_suspended:'這個 agent 的寫入已暫停', forbidden_op:'委派設定不允許這個操作', rate_limited:'寫入太快', conflict:'這張單在你讀取後被改過', invalid_transition:'目前不能轉到這個狀態'})[e.code] || e.code, e.details); else throw e; }
  renderAgent(); if (window.render) render();
  return entry;
}
function ask(opts){ return new Promise(res => { AG.asks = {opts, res}; renderAgent(); }); }
const link = (id, v) => `<a href="#/cs/${id}${v ? '?version=' + v : ''}">${ORIGIN}/changesets/${id}</a>`;

// ---------- 用法劇本（設計 02 §14） ----------
const USAGES = [
  {id:1, label:'1 回報 bug', text:'幫我在 AUTH 回報：iPad 上用 Safari 從公司入口登入，幾分鐘後就被登出，Chrome 沒事'},
  {id:2, label:'2 找重複', text:'AUTH-91 跟哪些舊單重複？'},
  {id:3, label:'3 分流這週', text:'把 AUTH 這週的單分流完'},
  {id:4, label:'4 卡在哪', text:'AUTH-123 現在卡在哪、誰在處理？'},
  {id:5, label:'5 改下個版本', text:'把 AUTH-125、AUTH-126 改成下個版本'},
  {id:6, label:'6 指派＋轉狀態', text:'把 AUTH-91 指派給 Bob，改成處理中'},
  {id:7, label:'7 核准了沒？', text:'剛才那份核准了沒？'},
  {id:8, label:'8 補 note', text:'跟 Bob 說 AUTH-123 需要 Safari 版本號'},
  {id:9, label:'9 跨專案', text:'WEB-88 這個 bug 在 AUTH 也要追'},
  {id:10, label:'逾時重送', text:'（模擬）幫 AUTH-126 設 Severity Medium，送出時網路逾時'},
  {id:11, label:'失控（熔斷）', text:'（模擬：agent 被 issue 內容誤導，反覆改 AUTH-130 的優先度）'},
];
async function runUsage(id){
  if (AG.busy) return; AG.busy = true; AG.asks = null;
  const u = USAGES.find(x => x.id === id); user(u.text);
  try { await SCRIPTS[id](); } catch (e){ say('（原型錯誤）' + e.message); console.error(e); }
  AG.busy = false; renderAgent();
}
function stopIfErr(e){
  if (!e.err) return false;
  const c = e.err.code;
  if (c === 'context_unavailable') say(`oxledger 回報這個 agent 的工作脈絡已停用（${e.err.details.project || ''} 的權限設定變更）。我不能讀也不能提案，也不會自己建新委派繞過。請到 <a href="#/agents">Agent 委派</a> 建立新工作脈絡。`);
  else if (c === 'unauthenticated') say('token 已失效，請在 oxledger 發新 token 給我。');
  else if (c === 'not_found') say(`找不到，或不在我被委派的範圍內（${esc(J(e.err.details))}）。我不會猜它存不存在。`);
  else if (c === 'delegation_unavailable') say(`我沒有 ${e.err.details.project} 的有效委派。需要的話請到 <a href="#/agents">Agent 委派</a> 加入 ${e.err.details.project}。`);
  else say(`錯誤 ${c}，next_actions：${e.err.next_actions.join('、')}。`);
  return true;
}
// D20：command_submit 的結果，依 outcome 回報（applied 就是完成；review_required 才請人核准）
const ACT = () => `<a href="#/agents/activity/${activeCtx().id}">活動頁</a>`;
function submitted(p, appliedText, reviewText){
  if (p.res.outcome === 'applied'){ AG.last.exec = p.res.execution_id; AG.last.cs = null; say(appliedText + `（直接寫入，不需要核准；可在 ${ACT()} 檢視或回復）`); }
  else { AG.last.cs = p.res.changeset_id; AG.last.exec = null; say(reviewText + `${p.res.reason === 'policy' ? `委派設定要求核准（${(p.res.policy_reasons || []).join('、')}）。` : ''}<b>尚未套用</b>；請到 ${link(p.res.changeset_id)} 審閱。`); }
}
function stopD20(e){
  if (!e.err) return false;
  const c = e.err.code;
  if (c === 'agent_suspended'){ say(`oxledger 暫停了我的寫入：${esc(e.err.details.reason || '')}。我會停下來，不換 token 或新脈絡繞過；請到 ${ACT()} 檢查並決定要不要恢復。`); return true; }
  if (c === 'forbidden_op'){ say(`你的委派設定不允許這個操作（${esc((e.err.details.reasons || []).join('、'))}）。我不改用其他操作繞過；需要的話請到 <a href="#/agents">Agent 委派</a> 調整設定。`); return true; }
  if (c === 'conflict'){ say(`${e.err.details.key} 在我讀取後被改過（第 ${e.err.details.expected} → ${e.err.details.current} 版），<b>沒有寫入</b>。我會重讀再決定，不會強制覆蓋。`); return true; }
  return stopIfErr(e);
}
const SCRIPTS = {
  async 1(){
    const [d, s] = await Promise.all([call('describe', {scope:'project_tracker', project:'AUTH', tracker:'Bug'}), call('search', {project:'AUTH', tracker:'Bug', q:'Safari 登入', limit:5, fields:['key','subject','status'], explain:'brief'})]);
    if (stopIfErr(d) || stopIfErr(s)) return;
    const c = s.res.items.slice(0, 2);
    say(`找到 ${c.length} 張可能相關：${c.map(x => `<b>${x.key}</b>「${esc(x.subject)}」（${x.status}）`).join('、')}。症狀不完全一樣（你的是「幾分鐘後被登出」），在目前可見範圍內不像重複。要怎麼做？`);
    const nk = c[0] ? c[0].key : 'AUTH-123';
    const ch = await ask(['建一張新單', '補一則 note 到 ' + nk]);
    const ev = c[0] && c[0].evidence ? [{key:c[0].key, field:c[0].evidence[0].field, quote:c[0].evidence[0].text, citation_id:c[0].evidence[0].citation_id}] : [];
    let p;
    if (ch === 0) p = await call('command_submit', {operation_id:'7f3e…a1', command:{title:'回報：iPad Safari 登入幾分鐘後被登出', ops:[{kind:'create_issue', bind:'new1', project:'AUTH', tracker:'Bug', fields:{subject:'iPad Safari 登入幾分鐘後被登出', description:'從公司入口用 iPad Safari 登入成功，幾分鐘後被登出回到登入頁；Chrome 沒有這個問題。\n可能和 AUTH-123 的 SameSite 設定有關。', priority:'High', 'cf:severity':'High'}}], preconditions:{}, evidence:ev}});
    else p = await call('command_submit', {operation_id:'7f3e…a2', command:{title:'補充 iPad 登出的症狀', ops:[{kind:'add_note', target:nk, text:'補充：iPad Safari 從公司入口登入後，幾分鐘就被登出（Chrome 正常）。', private:false}], preconditions:{[nk]:D.I[nk].lock}, evidence:[]}});
    if (stopD20(p)) return;
    const made = p.res.effects ? p.res.effects.find(x => x.kind === 'created') : null;
    submitted(p, made ? `已建立 <b>${keyLink(made.key)}</b>「iPad Safari 登入幾分鐘後被登出」。搜尋可能還要一下才找得到，這不代表寫入失敗` : `已在 ${nk} 補上 note`, `已建立提案 #${p.res._no}。`);
  },
  async 2(){
    const g = await call('get', {key:'AUTH-91', fields:['subject','description','tracker','lock_version'], include:['actions']}); if (stopIfErr(g)) return;
    const s = await call('search', {q:'Safari 登入 導向', project:'AUTH', tracker:'Bug', limit:6, fields:['key','subject','status','lock_version'], explain:'brief'}); if (stopIfErr(s)) return;
    const cands = s.res.items.filter(x => x.key !== 'AUTH-91').slice(0, 2);
    const m = await call('get_many', {keys:cands.map(x => x.key), fields:['description','lock_version']}); if (stopIfErr(m)) return;
    say(`在目前條件與可見範圍內的候選：${cands.map(x => `<b>${x.key}</b>「${esc(x.subject)}」`).join('、')}。<b>AUTH-123</b> 的描述「Safari 每次都會發生」和 AUTH-91 的重新導向循環最接近，但不能確定是同一個原因。要把 AUTH-91 標成 AUTH-123 的重複嗎？（只建立關係，不會關單）`);
    const ch = await ask(['標成 AUTH-123 的重複', '先不要']);
    if (ch === 1){ say('好，不動。'); return; }
    const p = await call('command_submit', {operation_id:'9b21…c4', command:{title:'AUTH-91 可能是 AUTH-123 的重複', ops:[{kind:'duplicate_of', source:'AUTH-91', target:'AUTH-123'}], preconditions:{'AUTH-91':D.I['AUTH-91'].lock, 'AUTH-123':D.I['AUTH-123'].lock}, evidence:[{key:'AUTH-123', field:'description', quote:'Safari 每次都會發生'}]}});
    if (stopD20(p)) return;
    submitted(p, '已把 AUTH-91 標為 AUTH-123 的重複（只建立關係，沒有關單）', `已提案（#${p.res._no}）。`);
  },
  async 3(){
    say('我用的條件：<b>AUTH、本週建立（11/02–11/08）、狀態新建、未指派</b>。');
    const s = await call('search', {project:'AUTH', filter:[{f:'status', op:':', v:'新建'}, {f:'assignee', op:':', v:'none'}], limit:20, fields:['key','subject','assignee','lock_version'], explain:'none'}); if (stopIfErr(s)) return;
    const keys = s.res.items.map(x => x.key).filter(k => D.I[k]._triage !== undefined);
    const d = await call('describe', {scope:'project_tracker', project:'AUTH', tracker:'Bug'}); if (stopIfErr(d)) return;
    const m = await call('get_many', {keys:keys.slice(0, 10), fields:['description'], include:['actions','recent_notes'], note_limit:2}); if (stopIfErr(m)) return;
    const who = {'AUTH-125':'u4','AUTH-127':'u2','AUTH-129':'u2','AUTH-130':'u5','AUTH-132':'u2','AUTH-126':'u4'};
    let gid = null, n = 0, applied = 0, review = 0; const skipped = [];
    for (const k of keys){
      const t = D.I[k]._triage;
      if (!t || !t.p){ skipped.push(k); continue; }
      let ops;
      if (k === 'AUTH-128') ops = [{kind:'update_fields', target:k, fields:{priority:t.p, 'cf:severity':t.sev}}, {kind:'add_note', target:k, text:'建議併入下一輪 i18n 工作一起處理（登入頁還有 3 個未翻譯字串）。', private:false}];
      else { ops = [{kind:'update_fields', target:k, fields:Object.assign({priority:t.p, 'cf:severity':t.sev}, who[k] ? {assignee:who[k]} : {})}]; if (t.sev === 'Critical') ops.push({kind:'transition', target:k, to:'處理中'}); }
      const args = {operation_id:'tri-' + k, command:{title:`分流 ${k}`, ops, preconditions:{[k]:D.I[k].lock}, evidence:[{key:k, field:'description', quote:D.I[k].description.slice(0, 20)}]}};
      if (!gid) args.group_label = '本週分流 11/02–11/08'; else args.group_id = gid;
      const p = await call('command_submit', args);
      if (p.err && ['conflict','invalid_transition','forbidden_op'].includes(p.err.code)){ skipped.push(k + '（' + p.err.code + '）'); continue; }
      if (stopD20(p)) return;
      if (p.res.outcome === 'applied') applied++; else { review++; gid = gid || p.res.group_id; }
      n++;
    }
    AG.last.group = gid; AG.last.cs = null;
    say(`分流 ${n} 張：${applied ? `<b>${applied} 張已直接寫入</b>（可在 ${ACT()} 檢視或回復）` : ''}${review ? `${applied ? '；' : ''}<b>${review} 份要你核准</b>，請到 <a href="#/review?group=${gid}">${ORIGIN}/changesets?group=${gid}</a> 審閱，核准前<b>還不算分流完</b>` : ''}。${skipped.length ? `沒有動：${skipped.join('、')}（根據不足或資料已變）。` : ''}`);
  },
  async 4(){
    const g = await call('get', {key:'AUTH-123', fields:['subject','status','assignee','status_since','last_activity_at'], include:['recent_notes','recent_events'], note_limit:5}); if (stopIfErr(g)) return;
    const notes = g.res.recent_notes || []; const last = notes[notes.length - 1];
    say(`AUTH-123 指派給 <b>${esc(g.res.assignee)}</b>，在「${g.res.status}」停留 3 天（11/04 起）。最新可見活動是 ${esc(last.at)} ${esc(last.author)} 的 note：「${esc(last.text)}」〔${last.citation_id}〕。\n我只看得到非私有內容；不能只因久未更新就說被誰卡住。`);
  },
  async 5(){
    const d = await call('describe', {scope:'project_tracker', project:'AUTH', tracker:'Bug'}); if (stopIfErr(d)) return;
    say('oxledger 第一版<b>不支援指定版本</b>（describe 的 unsupported 有 fixed_version_id）。我不會用到期日或其他欄位代替，也不建假提案。');
  },
  async 6(){
    const [g, r] = await Promise.all([call('get', {key:'AUTH-91', include:['actions']}), call('resolve', {text:'bob', kind:'principal', project:'AUTH', purpose:'assignee'})]);
    if (stopIfErr(g)) return;
    let uid;
    if (r.err && r.err.code === 'ambiguous_reference'){
      const c = r.err.details.candidates;
      say(`「Bob」有 ${c.length} 位可以指派：${c.map(x => `${x.name}（${x.login}）`).join('、')}。要指派給哪一位？`);
      const ch = await ask(c.map(x => `${x.name}（${x.login}）`)); uid = c[ch].id;
    } else if (stopIfErr(r)) return; else uid = r.res.id;
    const p = await call('command_submit', {operation_id:'as-91', command:{title:'AUTH-91 指派並開始處理', ops:[{kind:'update_fields', target:'AUTH-91', fields:{assignee:uid}}, {kind:'transition', target:'AUTH-91', to:'處理中'}], preconditions:{'AUTH-91':g.res.lock_version}, evidence:[]}});
    if (stopD20(p)) return;
    submitted(p, `AUTH-91 已指派給 ${esc(uname(uid))}，狀態改成處理中`, `已提案（#${p.res._no}）。`);
  },
  async 7(){
    if (AG.last.exec && !AG.last.group){ say('剛才那筆是<b>直接寫入</b>，不需要核准，已經生效。你可以在 ' + ACT() + ' 檢視或回復。'); return; }
    if (AG.last.group){
      const l = await call('changeset_list', {group_id:AG.last.group}); if (stopIfErr(l)) return;
      const by = {}; l.res.items.forEach(i => by[i.state] = (by[i.state] || 0) + 1);
      say(`分組裡：${Object.entries(by).map(([s, n]) => `${({proposed:'待審', succeeded:'已套用', failed:'失敗', rejected:'已拒絕', canceled:'已取消', approved:'確認中'})[s] || s} ${n}`).join('、')}。${by.proposed ? '還有待審的，<b>這週還沒分流完</b>。' : ''}`);
      if (!AG.last.cs) return;
    }
    if (!AG.last.cs){ say('這個對話裡我還沒有送出過提案。'); return; }
    const g = await call('changeset_get', {changeset_id:AG.last.cs}); if (stopIfErr(g)) return;
    const st = g.res.state, ex = g.res.last_execution;
    if (st === 'succeeded'){
      const created = (ex.effects || []).find(e => e.kind === 'created');
      if (created){
        const gg = await call('get', {key:created.key, fields:['subject','description'], citations:['subject','description']});
        say(`已核准並套用：建立了 <b>${created.key}</b>「${esc(gg.res.subject)}」。可以直接引用〔${gg.res.citations[0].citation_id}〕。搜尋可能還要一下才找得到（consistent_after ${ex.search.consistent_after}），這不代表寫入失敗。`);
      } else say(`已核准並套用：${(ex.effects || []).map(e => e.key).join('、')} 已更新。`);
    } else if (st === 'failed' && ex && ex.error === 'conflict'){
      say('套用時發生衝突：那張單在我提案後被別人改過，沒有任何變更被套用。要我讀最新資料、建一個修訂版嗎？');
      const ch = await ask(['重讀並建立修訂版', '不用，我在 oxledger 處理']);
      if (ch === 1){ say('好，你可以在審閱頁的「檢視差異」逐欄決定。'); return; }
      const cs = D.changesets.find(c => c.id === AG.last.cs); const v = curVer(cs); const k = opTargets(v)[0];
      const gg = await call('get', {key:k, include:['actions']}); if (stopIfErr(gg)) return;
      const ops = v.ops.filter(o => o.kind !== 'transition' || (gg.res.actions.transitions || []).includes(o.to)).map(o => { const c = Object.assign({}, o); delete c.before; delete c.from; return c; });
      const rv = await call('changeset_revise', {changeset_id:cs.id, expected_current_version:cs.cur, ops, preconditions:{[k]:gg.res.lock_version}}); if (stopIfErr(rv)) return;
      say(`已建立第 ${rv.res.version} 版（依 ${k} 第 ${gg.res.lock_version} 版的資料），尚未套用；請到 ${link(cs.id, rv.res.version)} 再審一次。`);
    } else if (st === 'proposed') say(`還沒核准（第 ${g.res.version} 版，檢查狀態 ${g.res.validation.status}）。連結：${link(AG.last.cs)}。我不能替你核准。`);
    else if (st === 'approved') say('你按了核准，但結果還在確認中。請回 oxledger 的提案頁用原請求重送；我不能替你重送核准，也不會說「背景會自動完成」。');
    else say(`提案目前是 ${st}。`);
  },
  async 8(){
    const g = await call('get', {key:'AUTH-123', include:['actions']}); if (stopIfErr(g)) return;
    const p = await call('command_submit', {operation_id:'nt-123', command:{title:'請 Bob 提供 Safari 版本號', ops:[{kind:'add_note', target:'AUTH-123', text:'@bob 可以補一下重現時的 Safari 版本號嗎？（例如 17.1、17.4）', private:false}], preconditions:{'AUTH-123':g.res.lock_version}, evidence:[]}});
    if (stopD20(p)) return;
    submitted(p, '已在 AUTH-123 留言請 Bob 補 Safari 版本號（以 claude-code@alice 的身分）', `已提案一則公開 note（#${p.res._no}）。`);
  },
  async 9(){
    const d = await call('describe', {scope:'context'}); if (stopIfErr(d)) return;
    const web = d.res.projects.find(p => p.project === 'WEB');
    if (!web || !web.readable){ say('我沒有 WEB 的委派，讀不到 WEB-88。要追這張單，請到 <a href="#/agents/new?ctx=' + activeCtx().id + '">Agent 委派 → 加入專案</a> 把 WEB 加進來（畫面上會先告訴你加入後我能寫到哪裡）。'); return; }
    const g = await call('get', {key:'WEB-88', fields:['subject','description']}); if (stopIfErr(g)) return;
    const p = await call('command_submit', {operation_id:'x-88', command:{title:'在 AUTH 追 WEB-88', ops:[{kind:'create_issue', bind:'n1', project:'AUTH', tracker:'Bug', fields:{subject:'追蹤 WEB-88：結帳頁 Safari 登入狀態遺失', description:'WEB-88 回報 Safari 進結帳頁時被要求重新登入，可能是 SSO cookie 問題，在 AUTH 追根因。', priority:'High'}}], preconditions:{}, evidence:[{key:'WEB-88', field:'description', quote:'和 AUTH 的 SSO cookie 有關的可能性高'}]}});
    if (stopD20(p)) return;
    const made = p.res.effects ? p.res.effects.find(x => x.kind === 'created') : null;
    submitted(p, made ? `已在 AUTH 建立追蹤單 ${keyLink(made.key)}` : '', `已提案在 AUTH 建立追蹤單（#${p.res._no}）。`);
    say('要不要我也在 WEB-88 留言說 AUTH 在追？');
    const ch = await ask(['好，在 WEB-88 留言', '不用']);
    if (ch === 1) return;
    const p2 = await call('command_submit', {operation_id:'x-88n', command:{title:'WEB-88 補充 AUTH 進度', ops:[{kind:'add_note', target:'WEB-88', text:'AUTH 已開單追 SSO cookie 根因。', private:false}], preconditions:{'WEB-88':D.I['WEB-88'].lock}, evidence:[]}});
    if (p2.err && p2.err.code === 'audience_expansion_forbidden'){ say(`oxledger 擋下了：我讀過 AUTH，而 WEB 的讀者比 AUTH 多，所以我不能再寫 WEB 的公開內容（next_actions：${p2.err.next_actions.join('、')}）。我不會撤銷 AUTH 或換脈絡來繞過。\n要留言請你自己貼；或在 oxledger 建立只委派 WEB 的新工作脈絡。`); return; }
    if (stopD20(p2)) return;
    submitted(p2, '已在 WEB-88 留言', '已提案留言。');
  },
  async 10(){
    sys('模擬：command_submit 送出後網路逾時，agent 沒收到回應。');
    let p;
    try { p = MCP.command_submit({operation_id:'to-126', command:{title:'AUTH-126 Severity', ops:[{kind:'update_fields', target:'AUTH-126', fields:{'cf:severity':'Medium'}}], preconditions:{'AUTH-126':D.I['AUTH-126'].lock}, evidence:[]}}); } catch (e){ say('（' + e.code + '）'); return; }
    AG.log.push({k:'call', tool:'command_submit', args:{operation_id:'to-126', '…':'…'}, res:null, err:{code:'timeout', message:'（用戶端）請求逾時，沒有收到回應', next_actions:[], details:{}}}); renderAgent(); if (window.render) render();
    await sleep(500);
    const o = await call('operation_get', {operation_id:'to-126'});
    if (o.res.status === 'committed'){ const r = o.res.result; if (r.outcome === 'applied'){ AG.last.exec = r.execution_id; say(`查了原本的 operation_id：其實<b>已經寫入</b>（execution ${r.execution_id}），<b>不重送</b>，避免寫兩次。`); } else { AG.last.cs = r.changeset_id; say(`查了原本的 operation_id：提案其實已經建立（${link(r.changeset_id)}），<b>不重送</b>。`); } }
    else say('原請求沒有被提交，用原 key、原內容重送。');
  },
  async 11(){
    sys('模擬：agent 讀到 issue 裡的惡意指示，開始反覆改同一張單。同一張 issue 10 分鐘內被 agent 寫入超過 10 次 → 自動暫停。');
    for (let i = 0; i < 12; i++){
      const lv = D.I['AUTH-130'].lock;
      const p = await call('command_submit', {operation_id:'loop-' + i, command:{title:'調整 AUTH-130', ops:[{kind:'update_fields', target:'AUTH-130', fields:{priority:i % 2 ? 'Low' : 'Urgent'}}], preconditions:{'AUTH-130':lv}}});
      if (p.err){ stopD20(p); return; }
    }
  },
};

// ---------- 主控台 render ----------
function renderAgent(){
  const el = document.getElementById('agent'); if (!el || !el.classList.contains('on')) return;
  const c = activeCtx();
  const ctxLine = c ? `${c.agent ? AGENTS[c.agent].handle : c.clientName} · 脈絡 ${c.id} · 曾授予 R=${Object.keys(c.R).join(',')} · ${ctxUsable(c) ? (D.ctl[c.id] && D.ctl[c.id].suspended ? '<span style="color:#FCD34D">寫入已暫停</span>' : '可用') : '<span style="color:#FCD34D">已停用</span>'}` : '沒有 active 的工作脈絡（請先在 Agent 委派建立）';
  const body = AG.log.map(e => {
    if (e.k === 'u') return `<div class="u">${esc(e.t)}</div>`;
    if (e.k === 'a') return `<div class="a"><b>agent</b>　${e.t}</div>`;
    if (e.k === 'sys') return `<div class="sys">${esc(e.t)}</div>`;
    const st = e.err ? `<span class="st err">${esc(e.err.code)}</span>` : e.res ? '<span class="st">ok</span>' : '<span class="st">…</span>';
    return `<details ${e.err ? 'open' : ''}><summary>▸ ${e.tool}${st}</summary><pre>→ ${esc(J(e.args))}</pre>${e.res || e.err ? `<pre>← ${esc(J(e.res || e.err))}</pre>` : ''}</details>`;
  }).join('');
  const asks = AG.asks ? `<div class="ask">${AG.asks.opts.map((o, i) => `<button data-ask="${i}">${esc(o)}</button>`).join('')}</div>` : '';
  el.innerHTML = `<h3 style="background:#0B1220;color:#F1F5F9">Agent 主控台 <small>模擬外部 agent 透過 MCP 呼叫 oxledger</small><span style="flex:1"></span><button class="btn sm" data-ag="clear" style="background:#142033;color:#F1F5F9">清除</button></h3>
    <div class="ctxline">${ctxLine}</div>
    <div class="usages">${USAGES.map(u => `<button data-use="${u.id}" ${AG.busy ? 'disabled' : ''} title="${esc(u.text)}">${u.label}</button>`).join('')}</div>
    <div class="term">${body || '<div class="sys">點上面的按鈕，用自然語言對 agent 下指令（設計 02 §14 的 9 個主要用法）。每個 MCP 呼叫都能展開看參數與回應。D20 之後 agent 用 command_submit：委派設成「直接寫入」就馬上生效（看 Agent 委派 → 活動），設成「需要核准」才進「待審」。</div>'}${asks}${AG.busy && !AG.asks ? '<div class="sys">…</div>' : ''}</div>`;
  const t = el.querySelector('.term'); el.scrollTop = el.scrollHeight;
}
document.addEventListener('click', e => {
  const u = e.target.closest('[data-use]'); if (u){ runUsage(+u.dataset.use); return; }
  const a = e.target.closest('[data-ask]'); if (a && AG.asks){ const r = AG.asks.res; const i = +a.dataset.ask; user(AG.asks.opts[i]); AG.asks = null; r(i); return; }
  const c = e.target.closest('[data-ag]'); if (c){ AG.log = []; AG.asks = null; renderAgent(); }
});

// ---------- 流程導覽 ----------
const FLOWS = [
  {id:'f18', t:'版面 B · 三欄與內建 AI', ref:'使用者 2026-11-07；前端 Q2、U21', steps:['（已開啟內建 AI）左欄導覽、中間工作區、右欄協作欄','開 AUTH-91：屬性與操作都在中間；右欄 AI 範圍自動是 AUTH-91','點「建議指派與優先度」→ 變更卡片 → 核准並套用；歷史顯示 oxledger-ai@alice〔agent〕· Alice 核准','右欄切「直接寫入」再試一次 → 直接生效，可回復','Ctrl K 輸入一段自然語言 → 「交給右欄 AI」','情境控制取消「內建 AI 開啟」→ 右欄只剩 agent 動態，沒內容就自動收合'], preset:'ai', go:'/issue/AUTH-91'},
  {id:'f19', t:'自訂首頁', ref:'使用者 2026-11-07', steps:['按「自訂首頁」：↑↓ 調順序、寬／窄、✕ 移除','「＋ 新增小工具」：加「存起來的篩選」並改條件、「專案概況」','「需要你決定」標固定，不能移除','按完成 → 存在你的帳號（原型用瀏覽器儲存）；「恢復預設」可還原'], go:'/home'},
  {id:'f1', t:'登入與 session 到期', ref:'02 §15.1', steps:['在情境控制按「session 到期」','隨便點一個頁面 → 跳登入頁，提示會回原頁','輸入 wrong 看錯誤訊息；再輸入任意密碼登入，回到原頁'], go:'/review'},
  {id:'f2', t:'首頁與 ⌘K 意圖列', ref:'01 §6、§15.11', steps:['看「需要你決定」：待核准、規則提醒（可稍後、可停用）','按 Ctrl K，試：AUTH-123、project:AUTH status:新建、foo:bar（語法錯）、/assign AUTH-91 bob（兩位 Bob）、AUTH-123 改成 bob（混合輸入）、一般文字'], go:'/home'},
  {id:'f15', t:'D20 · agent 直接寫入、活動與回復', ref:'02 §16（D20）', steps:['主控台按「3 分流這週」：委派預設「直接寫入」，8 張直接改好','首頁「agent 動態」摘要卡 → 檢視活動：每筆是 claude-code@alice〔agent〕的直接寫入','開 AUTH-127 看歷史（可篩「只看 agent」）；情境控制按「bob 改了 AUTH-127」','回活動頁，對 AUTH-127 那筆按「回復…」：指派被 bob 改過不動，其他欄位可回復','「事故回復…」：選起始時間 → 先暫停 → 回復，看逐筆結果'], go:'/home', agent:true},
  {id:'f16', t:'D20 · 熔斷：agent 失控自動暫停', ref:'02 §16.6', steps:['主控台按「失控（熔斷）」：agent 反覆改 AUTH-130','第 11 次被擋：agent_suspended，agent 停下來','首頁出現「寫入已自動暫停」卡；活動頁看被擋紀錄','回復誤改，再按「恢復寫入」'], go:'/home', agent:true},
  {id:'f17', t:'D20 · 逐項改成需要核准', ref:'02 §16.3、§16.10', steps:['Agent 委派 → AUTH 的「操作」按「設定」','把「新增 note」改成「需要核准」，「指派」改成「不允許」','主控台按「8 補 note」→ review_required，進待審','按「6 指派＋轉狀態」→ forbidden_op，agent 不繞過'], go:'/agents', agent:true},
  {id:'f3', t:'委派 agent、拿 token', ref:'02 §15.2、§16.10', steps:['（已清掉既有脈絡）按「委派給 agent」','選 Claude Code → 選 AUTH（OPS 不能選，看原因）→ 操作表（預設全部直接寫入，可逐項改）→ 確認','確認頁說明 agent 會以 claude-code@alice〔agent〕直接寫入；完成時只問一次密碼；token 與 MCP 設定同頁顯示一次'], preset:'noctx', go:'/agents'},
  {id:'f4', t:'送審路徑：agent 回報 bug → 你核准', ref:'02 §14 用法 1、7；§16.4', steps:['（已把「建立 issue」設成需要核准）主控台按「1 回報 bug」，選「建一張新單」','agent 回 review_required → 點連結 → 核准並套用（1 下）','歷史顯示 claude-code@alice〔agent〕· Alice 核准；回主控台按「7 核准了沒？」','到議題頁看「剛建立・搜尋同步中」卡片'], preset:'reviewCreate', go:'/home', agent:true},
  {id:'f5', t:'送審路徑：分流一週批次核准', ref:'02 §14 用法 3、§15.3', steps:['（已把整個委派設成需要核准）主控台按「3 分流這週」','點分組連結 → 「選取本頁符合的」（含 note 的那份不能勾）','核准並套用所選 → 看每列各自結果與摘要','想看部分失敗：送出前在情境控制按「bob 改了 AUTH-127」'], preset:'reviewAll', go:'/home', agent:true},
  {id:'f6', t:'衝突：建立修訂版並核准套用', ref:'02 §15.4、用法 7', steps:['已預先放一份改 AUTH-123 的提案，且 bob 已改過 AUTH-123','開提案 → 「檢視差異並決定」→ 逐欄選 → 一下完成','想看「預覽之後資料又變了」：預覽後在情境控制再按一次「bob 改了 AUTH-123」'], preset:'conflict', go:'/review'},
  {id:'f7', t:'找重複並標 duplicate', ref:'02 §14 用法 2', steps:['主控台按「2 找重複」→ 選「標成 AUTH-123 的重複」','直接寫入：開 AUTH-91 看關係與歷史；只建立關係，不關單','活動頁可移除這次建立的關係'], go:'/home', agent:true},
  {id:'f8', t:'跨專案與受眾限制', ref:'02 §3.8、§14 用法 9、U14', steps:['主控台按「9 跨專案」：沒有 WEB 委派會請你加入','到 Agent 委派 → 加入專案 → 勾 WEB，看「加入後能寫到哪裡」','再按「9 跨專案」：AUTH 建單可以；選「在 WEB-88 留言」被擋','撤銷 WEB 委派後看：寫入限制不解除'], go:'/agents', agent:true},
  {id:'f9', t:'權限變更 → 脈絡停用 → 建新脈絡', ref:'02 §3.8 M7、§15.2', steps:['（已預先放 1 份待審提案）情境控制按「管理員改了 AUTH 成員」','首頁、待審、提案頁都說明原因；主控台任何呼叫都回 context_unavailable','Agent 委派 → 建立新工作脈絡（密碼一次＋勾選宣告）→ 新 token'], preset:'pending1', go:'/home', agent:true},
  {id:'f10', t:'人直接操作議題', ref:'02 §6.8', steps:['AUTH-91：編輯欄位、轉狀態（只列合法的）、加 note（可私有）、標重複','編輯中在情境控制按「bob 改了 AUTH-91」再儲存 → 不會覆蓋，保留你的輸入','「回報問題」：漏填標題看必填訊息；送出前先找相似的單','agent 視角：看 agent 讀不到私有 note 與受限欄位'], go:'/issue/AUTH-91'},
  {id:'f11', t:'搜尋與 explain', ref:'03 §6 UX-1–12', steps:['點「Safari 登入」：每筆顯示命中欄位與高亮片段','「登出逾時」：只有部分字元相符，標成部分字元相符','「Severity High 且估時>8」：純欄位篩選顯示欄位值，不顯示假片段','情境控制開「慢篩選」再點欄位條件 → 400ms 後進度＋取消 → 到期給一鍵縮小','不顯示總數，只說「還有更多」'], go:'/issues'},
  {id:'f12', t:'套用時逾時：結果確認中', ref:'02 §8.3、§15.5', steps:['情境控制按「下一次核准逾時」','核准任一提案 → 「結果確認中」→ 「伺服器沒收到完整請求，尚未套用」','「用原請求重送」：同一個 operation_id，只套用一次'], preset:'pending1', go:'/review'},
  {id:'f13', t:'管理員：自訂欄位換型別', ref:'03 §5.7、Q-S9、D18', steps:['「影響版本」→ 換型別：建新欄位 → 預檢 → 複製轉換 → 停用舊欄位','看受限欄位可開放給內建角色'], go:'/admin/fields'},
  {id:'f14', t:'錯誤訊息一覽與帳號', ref:'02 §15.5、§15.1', steps:['看每個錯誤的「發生什麼、有沒有改到資料、下一步」','帳號頁：登出其他裝置'], go:'/errors'},
];
function renderGuide(){
  const el = document.getElementById('guide'); if (!el || !el.classList.contains('on')) return;
  el.innerHTML = `<h3>流程導覽 <small>點「開始」會重設資料並帶到起點</small></h3>${FLOWS.map(f => `<div class="flow ${UI.flow === f.id ? 'cur' : ''}"><b>${f.t}</b><div class="ref">${f.ref}</div>${UI.flow === f.id ? `<ol>${f.steps.map(s => `<li>${esc(s)}</li>`).join('')}</ol>` : ''}<button class="btn sm" data-flow="${f.id}">${UI.flow === f.id ? '重新開始' : '開始'}</button></div>`).join('')}
  <div class="flow"><b>不在第一切片</b><p>sprint 規劃、看板、Gantt、mindmap、BPMN、內建 AI、站會摘要：見 <a href="../01-ai-native-ux-sim.html">設計 01 的故事模擬</a>。第一版畫面不顯示這些入口（不支援的不做成可按的按鈕）。</p></div>`;
}
function renderCtl(){
  const el = document.getElementById('ctl');
  el.innerHTML = `<h3>情境控制 <small>模擬別人或系統做的事</small></h3>
    <div class="ctl"><div class="k">別人改了資料</div>
      <button data-sim="bob-123">bob 改了 AUTH-123（改指派給 Dave）</button>
      <button data-sim="note-123">bob 在 AUTH-123 加了一則 note</button>
      <button data-sim="bob-91">bob 改了 AUTH-91（Severity → High）</button>
      <button data-sim="bob-127">bob 改了 AUTH-127（指派給自己）</button>
      <button data-sim="edit-desc">有人改了 AUTH-123 的描述（引用原文失效）</button></div>
    <div class="ctl"><div class="k">權限與身分</div>
      <button data-sim="perm-auth">管理員改了 AUTH 成員（epoch +1）</button>
      <button data-sim="perm-web">管理員改了 WEB 成員（epoch +1）</button>
      <button data-sim="expire" class="${SIM.sessionExpired ? 'on' : ''}">session 到期（下一次動作生效）</button>
      <button data-sim="stepup">讓下一個敏感操作需要密碼</button></div>
    <div class="ctl"><div class="k">系統狀態</div>
      <button data-sim="timeout" class="${SIM.timeoutNext ? 'on' : ''}">下一次核准逾時</button>
      <label><input type="checkbox" data-simt="lag" ${SIM.lag ? 'checked' : ''}> 搜尋索引延遲（約 6 秒）</label>
      <label><input type="checkbox" data-simt="slowFilter" ${SIM.slowFilter ? 'checked' : ''}> 欄位篩選很慢（到期）</label>
      <label><input type="checkbox" data-simt="ai" ${SIM.ai ? 'checked' : ''}> 內建 AI 開啟（前端 Q2）</label>
      <label><input type="checkbox" data-simt="fieldMigrating" ${SIM.fieldMigrating ? 'checked' : ''}> 「估時」欄位維護中</label></div>
    <div class="ctl"><button data-sim="reset">全部重設</button></div>`;
}
function markCtl(){ renderCtl(); }
function bump(key, fn, text){ const is = D.I[key]; fn(is); is.lock++; is.events.push({at:clock(), actor:'u2', via:'human_direct', text}); scheduleIndex(key); }
function simulate(k){
  if (k === 'bob-123') bump('AUTH-123', i => { i.assignee = (i.assignee === 'u5' ? 'u2' : 'u5'); (i.origin = i.origin || {}).assignee = 'human:bob'; }, '指派 → ' + (D.I['AUTH-123'].assignee === 'u5' ? 'Bob Chen（bob）' : 'Dave Ko'));
  if (k === 'note-123') bump('AUTH-123', i => i.notes.push({id:'nb' + Date.now(), author:'u2', at:clock(), text:'Safari 17.4 也會發生。', private:false}), '新增 note');
  if (k === 'bob-91') bump('AUTH-91', i => { i.cf.severity = 'High'; (i.origin = i.origin || {})['cf:severity'] = 'human:bob'; }, 'Severity Medium → High');
  if (k === 'bob-127') bump('AUTH-127', i => { i.assignee = 'u2'; (i.origin = i.origin || {}).assignee = 'human:bob'; }, '指派 → Bob Chen（bob）');
  if (k === 'edit-desc') bump('AUTH-123', i => i.description = '從公司入口登入後又被導回登入頁。Chrome 正常，Safari 17 以後會發生。疑似 SameSite 設定。', '描述已修改');
  if (k === 'perm-auth'){ D.P.AUTH.epoch++; toast('管理員調整了 AUTH 的成員', 'w'); }
  if (k === 'perm-web'){ D.P.WEB.epoch++; toast('管理員調整了 WEB 的成員', 'w'); }
  if (k === 'expire') SIM.sessionExpired = true;
  if (k === 'stepup'){ UI.session.authAt = 0; toast('下一個敏感操作會要求密碼'); }
  if (k === 'timeout') SIM.timeoutNext = !SIM.timeoutNext;
  if (k === 'reset') return startFlow(null);
  if (k.startsWith('bob') || k.startsWith('note') || k === 'edit-desc') toast('別人改了資料：' + ({'bob-123':'AUTH-123', 'note-123':'AUTH-123', 'bob-91':'AUTH-91', 'bob-127':'AUTH-127', 'edit-desc':'AUTH-123'})[k], 'w');
  renderCtl(); render();
}
function seedProposal(kind){
  const c = activeCtx();
  if (kind === 'conflict'){
    const is = D.I['AUTH-123'];
    makeChangeset({ctx:c, project:'AUTH', title:'AUTH-123 提高嚴重度並交給 carol', ops:[{kind:'update_fields', target:'AUTH-123', fields:{priority:'Urgent', 'cf:severity':'Critical', assignee:'u4'}, before:{priority:is.priority, 'cf:severity':is.cf.severity, assignee:is.assignee}}], pre:{'AUTH-123':is.lock}, evidence:[{key:'AUTH-123', field:'description', quote:'Safari 每次都會發生'}]});
    bump('AUTH-123', i => i.assignee = 'u5', '指派 → Dave Ko');
  }
  if (kind === 'pending1'){
    const is = D.I['AUTH-101'];
    makeChangeset({ctx:c, project:'AUTH', title:'AUTH-101 改給 carol 並提高優先度', ops:[{kind:'update_fields', target:'AUTH-101', fields:{assignee:'u4', priority:'High'}, before:{assignee:is.assignee, priority:is.priority}}], pre:{'AUTH-101':is.lock}, evidence:[{key:'AUTH-101', field:'description', quote:'寄信佇列看起來有積壓'}]});
  }
}
function startFlow(id){
  D = initialData(); CS_SEQ = 900; GROUP_SEQ = 950; EXEC_SEQ = 0;
  Object.assign(SIM, {timeoutNext:false, lag:false, slowFilter:false, sessionExpired:false, fieldMigrating:false, ai:false});
  Object.assign(UI, {session:{logged:true, authAt:Date.now() - 2 * 3600e3}, viewAs:'human', sel:new Set(), batch:null, recent:[], modal:null, form:{}, openLock:{}, wiz:null, conflict:null, apply:{}, palette:null, clicks:0, returnTo:null, reminders:{snoozed:{}, off:false}, slow:null, admin:null, newToken:null, loginReason:null, loginErr:false, evF:'all', actF:'all', summaryOff:false, dpolOpen:null, undoSel:new Set(), collabHide:null, aiMode:'review', aiBusy:false});
  AG.log = []; AG.last = {}; AG.asks = null;
  UI.flow = id;
  const f = FLOWS.find(x => x.id === id);
  if (f && f.preset === 'noctx') D.contexts = [];
  if (f && f.preset === 'conflict') seedProposal('conflict');
  if (f && f.preset === 'pending1') seedProposal('pending1');
  if (f && f.preset === 'reviewCreate') D.contexts[0].delegations[0].policy.commands.create_issue = 'review';
  if (f && f.preset === 'reviewAll'){ const p = D.contexts[0].delegations[0].policy; Object.keys(p.commands).forEach(k => p.commands[k] = 'review'); }
  if (f && f.preset === 'ai') SIM.ai = true;
  if (f && f.agent) toggleDrawer('agent', true);
  document.getElementById('clk').textContent = '0';
  renderCtl(); go(f ? f.go : '/home');
}
document.addEventListener('click', e => {
  const f = e.target.closest('[data-flow]'); if (f){ startFlow(f.dataset.flow); return; }
  const s = e.target.closest('[data-sim]'); if (s){ simulate(s.dataset.sim); return; }
  const d = e.target.closest('[data-drawer]'); if (d){ toggleDrawer(d.dataset.drawer); return; }
  if (e.target.id === 'theme'){ const r = document.documentElement; r.dataset.theme = r.dataset.theme === 'light' ? 'dark' : 'light'; }
  if (e.target.id === 'clk-reset'){ UI.clicks = 0; document.getElementById('clk').textContent = '0'; }
});
document.addEventListener('change', e => { const t = e.target.closest('[data-simt]'); if (t){ SIM[t.dataset.simt] = t.checked; if (t.dataset.simt === 'slowFilter') UI.slow = null; renderCtl(); render(); } });
function toggleDrawer(id, force){
  const el = document.getElementById(id); const on = force != null ? force : !el.classList.contains('on');
  el.classList.toggle('on', on); document.querySelector(`[data-drawer="${id}"]`).classList.toggle('on', on);
  if (id === 'agent') renderAgent(); if (id === 'guide') renderGuide(); if (id === 'ctl') renderCtl();
}

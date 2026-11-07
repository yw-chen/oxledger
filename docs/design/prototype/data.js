// oxledger 原型：假資料與領域規則（照設計 02、03 簡化）。不是實作，只為了讓流程點得動。
'use strict';

const NOW_LABEL = '2026-11-07 14:32';
const ORIGIN = 'https://ox.example.com';

const USERS = [
  {id:'u1', login:'alice', name:'Alice Lin', color:'#67E8F9'},
  {id:'u2', login:'bob', name:'Bob Chen', color:'#FCD34D'},
  {id:'u3', login:'bob.wu', name:'Bob Wu', color:'#FDBA74'},
  {id:'u4', login:'carol', name:'Carol Hsu', color:'#6EE7B7'},
  {id:'u5', login:'dave', name:'Dave Ko', color:'#C4B5FD'},
  {id:'u6', login:'erin', name:'Erin Lai', color:'#FDA4AF'},
];
for (let i = 7; i <= 22; i++) USERS.push({id:'u'+i, login:'web'+i, name:'WEB 成員 '+i, color:'#94A3B8'});
const U = id => USERS.find(u => u.id === id);
// D20：agent 當成獨立使用者（一個（委派者, agent client）一個 principal，handle 穩定）
const AGENTS = {
  'ag-claude':{id:'ag-claude', handle:'claude-code@alice', client:'Claude Code（alice-laptop）', onBehalf:'u1'},
  'ag-codex':{id:'ag-codex', handle:'codex@alice', client:'Codex CLI（alice-laptop）', onBehalf:'u1'},
};
const agentFor = client => client === 'codex' ? 'ag-codex' : 'ag-claude';
const CMD_KINDS = [['create_issue','建立 issue'],['update_fields','修改欄位'],['transition','轉換狀態'],['add_note','新增 note'],['duplicate_of','標為重複']];
const POL_FIELDS = ['subject','description','assignee','priority','cf:severity','cf:estimate'];
const MODE_LABEL = {direct:'直接寫入', review:'需要核准', off:'不允許'};
function defaultPolicy(){ const p = {commands:{}, fields:{}, version:1}; CMD_KINDS.forEach(([k]) => p.commands[k] = 'direct'); POL_FIELDS.forEach(f => p.fields[f] = 'direct'); return p; }
const uname = id => { if (!id) return '（未指派）'; if (AGENTS[id]) return AGENTS[id].handle + '〔agent〕'; const u = U(id); if (!u) return id; const dup = USERS.filter(x => x.name.split(' ')[0] === u.name.split(' ')[0]).length > 1; return dup ? `${u.name}（${u.login}）` : u.name; };

const STATUSES = ['新建','處理中','待驗證','已解決','已拒絕'];
const PRIORITIES = ['Low','Normal','High','Urgent'];
const SEVERITIES = ['Low','Medium','High','Critical'];

// 工作流程（alice 在 AUTH／WEB 的 Developer 角色，Bug 與 Feature 相同）
const WORKFLOW = {
  '新建':['處理中','已拒絕'],
  '處理中':['待驗證','新建'],
  '待驗證':['已解決','處理中'],
  '已解決':['處理中'],
  '已拒絕':['新建'],
};

function initialData(){
  const P = {
    WEB:{key:'WEB', name:'網站前台', epoch:7, uniform:true, audience:USERS.slice(0,20).map(u=>u.id), assignable:['u1','u3','u6','u7','u8'], roles:'Developer、Reporter（all）'},
    AUTH:{key:'AUTH', name:'身分驗證', epoch:3, uniform:true, audience:['u1','u2','u3','u4','u5'], assignable:['u1','u2','u3','u4','u5'], roles:'Developer、Manager（all）'},
    OPS:{key:'OPS', name:'維運', epoch:2, uniform:false, audience:['u1','u4','u6','u9'], assignable:['u1','u4','u6'], roles:'Developer（all）、Contractor（own：只看自己的 issue）', whyNot:'這個專案有「只看自己 issue」的角色（Contractor），第一版不能委派給 agent'},
  };
  const I = {};
  let seq = 0;
  const mk = (key, o) => { I[key] = Object.assign({key, id:'0199…'+(400+(++seq)), tracker:'Bug', status:'新建', assignee:null, priority:'Normal', cf:{severity:null, estimate:null, customer:null}, notes:[], events:[], lock:0, created:'2026-10-20', isPrivate:false, indexed:true, author:'u4'}, o); I[key].project = key.split('-')[0]; return I[key]; };

  mk('AUTH-88', {subject:'Safari 登入後 cookie 遺失', description:'iOS 17 Safari 開啟「防止跨網站追蹤」時，登入成功後 session cookie 沒有寫入，重新整理就回到登入頁。', status:'已解決', assignee:'u2', priority:'High', cf:{severity:'High', estimate:6, customer:null}, lock:9, created:'2026-08-14'});
  mk('AUTH-91', {subject:'iOS Safari 重新導向循環', description:'從公司入口點「登入」後，Safari 在 /sso/callback 與 /login 之間來回跳，約 5 次後停在登入頁。', status:'新建', priority:'Normal', cf:{severity:'Medium', estimate:null, customer:null}, lock:2, created:'2026-10-28'});
  mk('AUTH-101', {subject:'密碼重設信延遲 20 分鐘', description:'使用者點「忘記密碼」後，重設信 20 分鐘後才收到；寄信佇列看起來有積壓。', status:'處理中', assignee:'u5', priority:'Normal', cf:{severity:'Medium', estimate:4, customer:null}, lock:5, created:'2026-10-21'});
  mk('AUTH-110', {subject:'MFA 簡訊驗證碼沒收到', description:'部分中華電信門號收不到 MFA 簡訊，重送也一樣。', status:'待驗證', assignee:'u4', priority:'High', cf:{severity:'High', estimate:8, customer:'台北某銀行'}, lock:7, created:'2026-10-02'});
  mk('AUTH-123', {subject:'SSO 登入後跳回登入頁', description:'從公司入口登入後又被導回登入頁。Chrome 正常，Safari 每次都會發生。疑似 SameSite 設定。', status:'處理中', assignee:'u2', priority:'High', cf:{severity:'High', estimate:12, customer:'台中某製造商'}, lock:4, created:'2026-11-03',
    notes:[
      {id:'n1', author:'u2', at:'11-04 10:12', text:'已在 staging 重現，Safari 17.1。', private:false},
      {id:'n2', author:'u5', at:'11-05 16:40', text:'客戶窗口說週五前要給暫時解法（私有：含客戶聯絡資訊）。', private:true},
      {id:'n3', author:'u2', at:'11-06 09:05', text:'等 QA 在 iPad 上再重現一次，確認不是外掛造成。', private:false},
    ],
    events:[
      {at:'11-03 09:20', actor:'u4', via:'human_direct', text:'建立'},
      {at:'11-04 10:10', actor:'u2', via:'human_direct', text:'狀態 新建 → 處理中；指派 → Bob Chen（bob）'},
      {at:'11-04 10:12', actor:'u2', via:'human_direct', text:'新增 note'},
      {at:'11-05 16:40', actor:'u5', via:'human_direct', text:'新增私有 note'},
      {at:'11-06 09:05', actor:'u2', via:'human_direct', text:'新增 note'},
    ]});
  // 本週待分流：新建、未指派
  const tri = [
    ['AUTH-125','登入頁在 Firefox 顯示空白','打開 /login 只有背景色，console 有 CSP 錯誤：refused to load script。', 'High','High'],
    ['AUTH-126','記住我選項無效','勾選「記住我」後，關掉瀏覽器再開仍要登入。', 'Normal','Medium'],
    ['AUTH-127','LDAP 同步把離職帳號重新啟用','每晚同步後，已停用的帳號又被設成啟用。', 'Urgent','Critical'],
    ['AUTH-128','登入錯誤訊息英文沒翻譯','密碼錯誤時顯示 Invalid credentials，不是中文。', 'Low','Low'],
    ['AUTH-129','SSO 回呼網址大小寫敏感','IdP 設定的 callback 大小寫不同時登入失敗。', 'Normal','Medium'],
    ['AUTH-130','API token 列表載入很慢','有 300 把 token 的帳號，列表要 8 秒。', 'Normal','Medium'],
    ['AUTH-131','密碼強度提示和規則不一致','提示說 8 碼即可，實際要求 10 碼。', 'Low','Low'],
    ['AUTH-132','MFA 備用碼可以重複使用','同一組備用碼用過後還能再登入一次。', 'Urgent','Critical'],
    ['AUTH-133','偶爾登入很慢','有時候登入要等很久。', null,null],
    ['AUTH-134','希望支援 passkey','使用者想用 passkey 登入。', null,null],
  ];
  tri.forEach(([k,s,d,p,sev],i)=>mk(k,{subject:s, description:d, created:'2026-11-0'+(3+(i%5)), lock:0, _triage:{p,sev}}));
  mk('WEB-80', {subject:'首頁輪播在手機上跑版', description:'iPhone SE 寬度下輪播圖右側被切掉。', status:'處理中', assignee:'u6', lock:3, created:'2026-10-11'});
  mk('WEB-85', {subject:'購物車數量徽章不同步', description:'在另一個分頁加入商品後，徽章數字不會更新。', status:'新建', lock:1, created:'2026-10-30'});
  mk('WEB-88', {subject:'結帳頁在 Safari 登入狀態遺失', description:'Safari 進入結帳頁時被要求重新登入，和 AUTH 的 SSO cookie 有關的可能性高。', status:'新建', priority:'High', cf:{severity:'High', estimate:null, customer:null}, lock:1, created:'2026-11-05'});
  mk('WEB-90', {subject:'登入按鈕對比不足', description:'深色主題下登入按鈕文字對比只有 2.8:1。', status:'新建', priority:'Low', lock:0, created:'2026-11-06'});
  mk('OPS-12', {subject:'憑證 11/20 到期', description:'*.example.com 憑證 11/20 到期，要排更新。', status:'處理中', assignee:'u4', lock:2, created:'2026-11-01'});

  const contexts = [{
    id:'ctx-1', client:'claude-code', agent:'ag-claude', clientName:'Claude Code（alice-laptop）', state:'active', reason:null,
    R:{AUTH:3}, created:'2026-11-03 10:00',
    delegations:[{id:'dg-1', project:'AUTH', state:'active', template:'分流＋回報', expires:'2026-11-10 10:00', policy:defaultPolicy()}],
    tokens:[{id:'tk-1', last4:'q7Zk', created:'11-03 10:00', state:'active'}],
  }];
  const changesets = [];
  const groups = [];
  return {P, I, contexts, changesets, groups, execs:[], blocked:[], ctl:{}, opsLog:{}};
}

let D = initialData();
let CS_SEQ = 900, OP_SEQ = 0, XID = 48211, GROUP_SEQ = 950;

// ---------- 可見性（給 agent 視角用）----------
// agent：只讀非私有 issue、非私有 note、非受限欄位（客戶名稱是受限欄位，只對 Manager 開放）
const CF_DEFS = [
  {key:'severity', name:'Severity', type:'list', options:SEVERITIES, restricted:false},
  {key:'estimate', name:'估時（小時）', type:'num', restricted:false},
  {key:'customer', name:'客戶名稱', type:'text', restricted:true, roles:'Manager'},
];

// ---------- 受眾 ----------
const subset = (a, b) => a.every(x => b.includes(x));
function intersectAud(projs){ let s = null; projs.forEach(p => { const a = D.P[p].audience; s = s ? s.filter(x => a.includes(x)) : a.slice(); }); return s || []; }
function ctxUsable(ctx){
  if (!ctx || ctx.state !== 'active') return false;
  return Object.entries(ctx.R).every(([p, e]) => D.P[p].epoch === e);
}
function ctxBrokenBy(ctx){ return Object.entries(ctx.R).filter(([p, e]) => D.P[p].epoch !== e).map(([p]) => p); }
function activeCtx(){ return D.contexts.find(c => c.state === 'active'); }
function canPublishTo(srcProjects, dest){ return subset(D.P[dest].audience, intersectAud(srcProjects)); }

// ---------- 提案 ----------
const isShortField = f => ['assignee','priority','cf:severity','cf:estimate','start_date','due_date'].includes(f);
function newCsId(){ return 'cs0199' + (++CS_SEQ); }
function hashOf(v){ let h = 0; const s = JSON.stringify([v.ops, v.pre, v.src]); for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0; return 'sha256:' + (h >>> 0).toString(16).padStart(8, '0') + '…'; }

function makeChangeset({ctx, project, title, ops, pre, evidence, group}){
  const src = Object.keys(ctx.R);
  const v = {v:1, ops, pre:pre || {}, evidence:evidence || [], src, srcEpoch:Object.assign({}, ctx.R), by:'agent', at:NOW_LABEL};
  v.hash = hashOf(v);
  v.evAt = {}; Object.keys(v.pre).forEach(k => v.evAt[k] = D.I[k].events.length);
  const cs = {id:newCsId(), no:CS_SEQ - 859, ctx:ctx.id, project, title, state:'proposed', versions:[v], cur:1, group:group || null,
    expires:'2026-11-14 14:32', executions:[], created:NOW_LABEL};
  D.changesets.push(cs);
  return cs;
}
const curVer = cs => cs.versions[cs.cur - 1];

function opTargets(v){ const t = []; v.ops.forEach(o => { if (o.target && !t.includes(o.target)) t.push(o.target); if (o.source && !t.includes(o.source)) t.push(o.source); }); return t; }
function opProject(o){ return o.kind === 'create_issue' ? o.project : (o.target || o.source).split('-')[0]; }

// 驗證：結構、合法值、權限、workflow、版本前置條件、受眾、evidence
function validate(cs){
  const v = curVer(cs);
  const checks = {schema:'ok', values:'ok', permission:'ok', workflow:'ok', precondition:'ok', audience:'ok'};
  const blocking = [], warnings = [], conflicts = [];
  for (const o of v.ops){
    const proj = opProject(o);
    if (!canPublishTo(v.src, proj)) { checks.audience = 'failed'; blocking.push({code:'audience_expansion_forbidden', field:o.kind, msg:`來源包含 ${v.src.join('、')}，不能寫到讀者較多的 ${proj}`}); }
    if (o.kind === 'transition'){
      const is = D.I[o.target]; let from = is.status;
      // 同一份提案若前面有 transition，依序推
      if (!(WORKFLOW[from] || []).includes(o.to)) { checks.workflow = 'failed'; blocking.push({code:'invalid_transition', field:'status', msg:`目前狀態「${from}」不能轉到「${o.to}」；可以轉到：${(WORKFLOW[from]||[]).join('、')}`}); }
    }
    if (o.kind === 'update_fields' && o.fields.assignee && !D.P[proj].assignable.includes(o.fields.assignee)) { checks.values = 'failed'; blocking.push({code:'invalid_value', field:'assignee', msg:'指派對象不在可指派名單'}); }
  }
  for (const [k, lv] of Object.entries(v.pre)){
    const is = D.I[k];
    if (!is) continue;
    if (is.lock !== lv){ checks.precondition = 'conflict'; conflicts.push(k); }
  }
  for (const e of v.evidence){
    const is = D.I[e.key];
    e.status = (is && (is[e.field] || '').includes(e.quote)) ? 'current' : 'source_changed';
    if (e.status !== 'current') warnings.push({code:'evidence_changed', msg:`根據 ${e.key} 的原文已修改`});
  }
  let status = 'ok';
  if (blocking.length) status = 'blocked'; else if (conflicts.length) status = 'conflict';
  return {status, checks, blocking, warnings, conflicts, checked_at:clock()};
}

function reviewModes(cs){
  const v = curVer(cs), val = validate(cs);
  const modes = ['single'];
  const why = [];
  if (val.status !== 'ok') why.push(val.status === 'conflict' ? '有衝突' : '檢查未通過');
  if (val.warnings.length) why.push('有警告');
  let tr = 0;
  for (const o of v.ops){
    if (o.kind === 'update_fields'){ for (const f of Object.keys(o.fields)) if (!isShortField(f)) why.push('含長文欄位'); }
    else if (o.kind === 'transition'){ tr++; }
    else why.push({create_issue:'建立 issue', add_note:'含 note', duplicate_of:'duplicate 關係'}[o.kind]);
  }
  if (tr > 1) why.push('多次 transition');
  if (cs.state !== 'proposed') why.push('狀態不是待審');
  const ae = applyCheck(cs, cs.cur); if (ae && ['context_unavailable','delegation_unavailable'].includes(ae.code)) why.push('委派或工作脈絡已失效');
  if (!why.length) modes.push('queue');
  return {modes, why:[...new Set(why)]};
}

// 套用（approve-and-apply）
const SIM = {timeoutNext:false, lag:false, slowFilter:false, sessionExpired:false, fieldMigrating:false, ai:false};
function clock(){ const t = Math.floor((Date.now() - T0) / 1000); return '14:' + String(32 + Math.floor(t / 60)).padStart(2, '0') + ':' + String(t % 60).padStart(2, '0'); }
const T0 = Date.now();

function applyCheck(cs, ver){
  const ctx = D.contexts.find(c => c.id === cs.ctx);
  if (ver !== cs.cur) return {code:'version_superseded'};
  if (cs.state === 'succeeded') return {code:'already_applied'};
  if (['rejected','canceled','expired'].includes(cs.state)) return {code:'changeset_not_executable'};
  if (!ctx || ctx.state === 'closed' || !ctxUsable(ctx)) return {code:'context_unavailable', details:ctx && ctx.state === 'closed' ? {closed:true} : {project:ctx ? ctxBrokenBy(ctx)[0] : null}};
  const dg = ctx.delegations.find(d => d.project === cs.project && d.state === 'active');
  if (!dg) return {code:'delegation_unavailable'};
  const val = validate(cs);
  if (val.status === 'conflict') return {code:'conflict', details:{targets:val.conflicts}};
  if (val.status === 'blocked') return {code:val.blocking[0].code, details:val.blocking[0]};
  return null;
}

function executeOps(cs, actorVia){
  // D20：核准套用時 actor＝agent、approver＝人（§16.2）；和直接寫入共用 runOps（d20.js）
  const ctx = D.contexts.find(c => c.id === cs.ctx);
  return runOps(curVer(cs).ops, {actor:ctx.agent, onBehalf:'u1', approver:'u1', via:actorVia, ctx:ctx.id, csNo:cs.no, title:cs.title}).effects;
}

function approveAndApply(cs, ver, opId, mode){
  // 冪等：同 operation_id 回原結果
  const prev = cs.executions.find(e => e.op === opId);
  if (prev){
    if (prev.status === 'queued'){ // 原請求重送 → 繼續執行
      const err = applyCheck(cs, ver);
      if (err){ prev.status = 'failed'; prev.error = err; cs.state = 'failed'; return {status:'failed', exec:prev, error:err}; }
      prev.effects = executeOps(cs, 'changeset'); prev.status = 'succeeded'; prev.token = 'c1.3.' + (++XID); cs.state = 'succeeded';
      return {status:'succeeded', exec:prev};
    }
    return {status:prev.status, exec:prev, error:prev.error};
  }
  const err = applyCheck(cs, ver);
  // 佇列模式：server 重驗資格（衝突時回 conflict，比「不能在佇列核准」更有用）
  if (mode === 'queue' && !(err && err.code === 'conflict') && !reviewModes(cs).modes.includes('queue')) return {status:'rejected', error:{code:'review_mode_not_allowed'}};
  if (err && ['version_superseded','already_applied','changeset_not_executable'].includes(err.code)) return {status:'rejected', error:err};
  if (err && ['context_unavailable','delegation_unavailable'].includes(err.code)) return {status:'rejected', error:err};
  const exec = {id:'ex-' + (++OP_SEQ), op:opId, attempt:cs.executions.length + 1, status:'queued', version:ver, at:clock()};
  cs.executions.push(exec); cs.state = 'approved';
  if (SIM.timeoutNext){ SIM.timeoutNext = false; return {status:'pending', exec}; }
  if (err){ exec.status = 'failed'; exec.error = err; cs.state = 'failed'; return {status:'failed', exec, error:err}; }
  exec.status = 'running';
  exec.effects = executeOps(cs, 'changeset'); exec.status = 'succeeded'; exec.token = 'c1.3.' + (++XID); cs.state = 'succeeded';
  return {status:'succeeded', exec};
}

function reviseByOwner(cs, choices){
  // choices: {key:{field:'proposal'|'current'}}; 產生新版本（owner_revise），沿用 ceiling
  const v = curVer(cs);
  const ops = JSON.parse(JSON.stringify(v.ops)).map(o => {
    if (o.kind === 'update_fields'){
      for (const f of Object.keys(o.fields)) if (choices[o.target] && choices[o.target][f] === 'current') delete o.fields[f];
      if (!Object.keys(o.fields).length) return null;
    }
    if (o.kind === 'transition' && choices[o.target] && choices[o.target].status === 'current') return null;
    return o;
  }).filter(Boolean);
  const pre = {}; Object.keys(v.pre).forEach(k => pre[k] = D.I[k].lock);
  const ctx = D.contexts.find(c => c.id === cs.ctx);
  const nv = {v:cs.versions.length + 1, ops, pre, evidence:JSON.parse(JSON.stringify(v.evidence)), src:Object.keys(ctx.R), srcEpoch:Object.assign({}, ctx.R), by:'owner', at:clock()};
  nv.hash = hashOf(nv);
  cs.versions.push(nv); cs.cur = nv.v; cs.state = 'proposed';
  return nv;
}

// ---------- 欄位工具 ----------
function getField(is, f){ if (f.startsWith('cf:')) return is.cf[f.slice(3)]; return is[f]; }
function setField(is, f, v){ if (f.startsWith('cf:')) is.cf[f.slice(3)] = v; else is[f] = v; }
function fieldName(f){ return {assignee:'指派', priority:'優先度', subject:'標題', description:'描述', status:'狀態', 'cf:severity':'Severity', 'cf:estimate':'估時', 'cf:customer':'客戶名稱', start_date:'開始日', due_date:'到期日'}[f] || f; }
function fmtVal(f, v){ if (v == null || v === '') return '（空）'; if (f === 'assignee') return uname(v); if (f === 'cf:estimate') return v + ' h'; return String(v); }

// ---------- 搜尋 ----------
function scheduleIndex(key){ const is = D.I[key]; if (!is) return; is.indexed = false; setTimeout(() => { if (D.I[key]) { D.I[key].indexed = true; if (window.onIndexed) window.onIndexed(key); } }, SIM.lag ? 6000 : 400); }

const STRUCT_RE = /^(\s*(project|status|tracker|assignee|severity|priority|估時|estimate)\s*(:|>=|<=|>|<)\s*\S+)+\s*$/i;
function parseStructured(q){
  const out = []; const re = /(project|status|tracker|assignee|severity|priority|估時|estimate)\s*(:|>=|<=|>|<)\s*(\S+)/gi; let m;
  while ((m = re.exec(q))) out.push({f:m[1].toLowerCase() === 'estimate' ? '估時' : m[1].toLowerCase(), op:m[2], v:m[3]});
  return out;
}
function matchFilter(is, conds){
  return conds.every(c => {
    if (c.f === 'project') return is.project.toLowerCase() === c.v.toLowerCase();
    if (c.f === 'status') return c.v === 'open' ? !['已解決','已拒絕'].includes(is.status) : is.status === c.v;
    if (c.f === 'tracker') return is.tracker.toLowerCase() === c.v.toLowerCase();
    if (c.f === 'assignee') return c.v === 'none' ? !is.assignee : (U(is.assignee) || {}).login === c.v;
    if (c.f === 'severity') return (is.cf.severity || '').toLowerCase() === c.v.toLowerCase();
    if (c.f === 'priority') return (is.priority || '').toLowerCase() === c.v.toLowerCase();
    if (c.f === '估時'){ const x = is.cf.estimate; if (x == null) return false; const n = +c.v; return c.op === '>' ? x > n : c.op === '>=' ? x >= n : c.op === '<' ? x < n : c.op === '<=' ? x <= n : x === n; }
    return true;
  });
}
function terms(q){ return q.trim().split(/\s+/).filter(Boolean); }
// 每個詞：整串出現＝primary；只有部分 CJK 單字出現＝fallback（D15：單字權重 0.1）
function scoreText(text, ts){
  let score = 0; const hl = []; let anyPrimary = false;
  for (const t of ts){
    const i = text.toLowerCase().indexOf(t.toLowerCase());
    if (i >= 0){ score += 1; hl.push([i, i + t.length, 'p']); anyPrimary = true; continue; }
    for (const ch of [...t]) if (/[一-鿿]/.test(ch)){ const j = text.indexOf(ch); if (j >= 0){ score += 0.1; hl.push([j, j + 1, 'f']); } }
  }
  return {score, hl, primary:anyPrimary};
}
function snippet(text, hl){
  if (!hl.length) return {text:text.slice(0, 60), hl:[]};
  const first = Math.min(...hl.map(h => h[0])); const s = Math.max(0, first - 15); const e = Math.min(text.length, s + 60);
  return {text:(s > 0 ? '…' : '') + text.slice(s, e) + (e < text.length ? '…' : ''), hl:hl.filter(h => h[0] >= s && h[1] <= e).map(h => [h[0] - s + (s > 0 ? 1 : 0), h[1] - s + (s > 0 ? 1 : 0), h[2]]), quote:text.slice(s, e)};
}
// viewer: 'human' | 'agent'
function search({q = '', filter = null, project = null, limit = 10, viewer = 'human'}){
  let conds = filter || [];
  let text = q;
  if (!filter && STRUCT_RE.test(q)){ conds = parseStructured(q); text = ''; }
  const ts = terms(text);
  let pool = Object.values(D.I).filter(is => is.indexed);
  if (viewer === 'agent'){
    const ctx = activeCtx(); const ok = ctx ? ctx.delegations.filter(d => d.state === 'active').map(d => d.project) : [];
    pool = pool.filter(is => ok.includes(is.project) && !is.isPrivate);
  }
  if (project) pool = pool.filter(is => is.project === project);
  pool = pool.filter(is => matchFilter(is, conds));
  let items = pool.map(is => {
    if (!ts.length) return {is, score:0, ev:[]};
    const a = scoreText(is.subject, ts), b = scoreText(is.description, ts);
    const ev = [];
    if (a.hl.length) ev.push({field:'subject', ...snippet(is.subject, a.hl), strength:a.primary ? 'primary' : 'fallback'});
    if (b.hl.length) ev.push({field:'description', ...snippet(is.description, b.hl), strength:b.primary ? 'primary' : 'fallback'});
    return {is, score:a.score * 2 + b.score, ev, strength:(a.primary || b.primary) ? 'primary' : 'fallback'};
  }).filter(x => !ts.length || x.score > 0);
  items.sort((x, y) => y.score - x.score || (y.is.key > x.is.key ? 1 : -1));
  const more = items.length > limit;
  items = items.slice(0, limit);
  const lagging = Object.values(D.I).some(is => !is.indexed);
  return {items, more, conds, text, lagging};
}

function citationFor(key, field, quote){ return 'cit1.' + btoa(unescape(encodeURIComponent(JSON.stringify({k:key, f:field, q:quote.slice(0, 8)})))).replace(/=+$/, '').slice(0, 22) + '…'; }

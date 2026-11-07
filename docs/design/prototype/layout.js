// oxledger 原型：三欄版面（版面 B，2026-11-07 使用者選定）
// 左：導覽；中：工作區（屬性與操作都在這裡）；右：協作欄（外部 agent 動態＋內建 AI）。首頁是可個人自訂的小工具。
'use strict';

// 內建 AI 也是一個 agent principal，走 D20 同一套規則（〔待確認 U21〕）
AGENTS['ag-ai'] = {id:'ag-ai', handle:'oxledger-ai@alice', client:'內建 AI（oxledger）', onBehalf:'u1'};

// ---------- 左：導覽 ----------
function lnav(r){
  const p = r.parts[0], raw = r.raw || '';
  const n = awaiting().length;
  const susp = D.contexts.filter(c => c.state === 'active' && D.ctl[c.id] && D.ctl[c.id].suspended).length;
  const broken = D.contexts.filter(c => c.state === 'active' && !ctxUsable(c)).length;
  const failed = D.changesets.filter(c => c.state === 'failed').length;
  const decide = n + failed + susp + broken;
  const mine = Object.values(D.I).filter(i => i.assignee === 'u1' && !['已解決','已拒絕'].includes(i.status)).length;
  const tri = Object.values(D.I).filter(i => i.project === 'AUTH' && i.status === '新建' && !i.assignee).length;
  const mini = UI.navMini;
  const it = (href, ic, label, on, badge, warn) => `<a href="#${href}" class="${on ? 'on' : ''}" title="${esc(label)}"><span class="ic">${ic}</span>${mini ? '' : `<span class="lb">${esc(label)}</span>`}${badge ? `<span class="badge ${warn ? 'w' : ''}">${badge}</span>` : ''}</a>`;
  const sec = t => mini ? '<div class="sep"></div>' : `<div class="sec">${t}</div>`;
  return `<nav class="lnav ${mini ? 'mini' : ''}">
    <a href="#/new?project=AUTH" class="newbtn" title="回報問題"><span class="ic">＋</span>${mini ? '' : '<span class="lb">回報問題</span>'}</a>
    ${sec('工作')}
    ${it('/home', '⌂', '首頁', p === 'home', decide, susp + broken + failed > 0)}
    ${it('/issues?q=assignee:alice', '◉', '我的工作', raw.includes('assignee:alice'), mine)}
    ${it('/triage', '⇣', 'Triage 收件匣', p === 'triage', tri)}
    ${it('/review', '✓', '待審', p === 'review' || p === 'cs', n)}
    ${it('/issues', '☰', '全部議題', p === 'issues' && !raw.includes('assignee:') || p === 'issue')}
    ${sec('專案')}
    ${Object.values(D.P).map(x => it('/project/' + x.key + '/timeline', '▣', `${x.key} ${x.name}`, raw.startsWith('/project/' + x.key) || raw.endsWith('project:' + x.key))).join('')}
    ${sec('AI 團隊與 agent')}
    ${it('/team', '✦', 'AI 團隊', p === 'team', window.teamState && Object.values(teamState().paused).some(x => x) ? '!' : '', true)}
    ${it('/agents', '⚙', '我的 agent 委派', p === 'agents', susp + broken ? '!' : '', true)}
    ${sec('管理')}
    ${it('/brand', '◐', '品牌與外觀', p === 'brand')}
    ${it('/forms', '▤', '表單設計', p === 'forms')}
    ${it('/admin/fields', '⚒', '其他設定', p === 'admin')}
    <span style="flex:1"></span>
    <button class="btn sm ghost" data-a="nav-mini" title="${mini ? '展開導覽' : '收合成圖示'}">${mini ? '»' : '« 收合'}</button>
  </nav>`;
}

// ---------- 右：協作欄 ----------
function scopeOf(r){
  if (r.parts[0] === 'issue' && D.I[r.parts[1]]) return {kind:'issue', key:r.parts[1]};
  return {kind:'global'};
}
const scopeKey = s => s.kind === 'issue' ? s.key : 'global';
function dynamics(scope){
  const rows = [];
  const touches = (e, k) => [...e.created, ...e.changes.map(x => x.key), ...e.trans.map(x => x.key), ...e.notes.map(x => x.key), ...e.dups.map(x => x.source), ...e.dups.map(x => x.target)].includes(k);
  const execs = D.execs.filter(e => e.via !== 'undo' && AGENTS[e.actor] && (scope.kind === 'global' || touches(e, scope.key))).slice().sort((a, b) => b.t - a.t).slice(0, scope.kind === 'global' ? 6 : 10);
  const pend = D.changesets.filter(c => ['proposed','failed','approved'].includes(c.state) && (scope.kind === 'global' || opTargets(curVer(c)).includes(scope.key)));
  const warn = scope.kind === 'global' ? D.contexts.filter(c => c.state === 'active' && D.ctl[c.id] && D.ctl[c.id].suspended) : [];
  warn.forEach(c => rows.push(`<div class="banner w" style="margin:0 0 8px">${actorHtml(c.agent)} 寫入已暫停<span class="sp"></span><a class="btn sm" href="#/agents/activity/${c.id}">處理</a></div>`));
  if (pend.length) rows.push(`<div class="k" style="margin:2px 0 4px">待你核准</div>${pend.slice(0, 5).map(c => `<div class="dyn"><a href="#/cs/${c.id}">提案 #${c.no}</a> ${csStateChip(c.state)}<div style="font-size:12.5px">${opLines(c, curVer(c), true)}</div></div>`).join('')}${pend.length > 5 ? `<a href="#/review" class="dim">還有 ${pend.length - 5} 份</a>` : ''}`);
  if (execs.length) rows.push(`<div class="k" style="margin:8px 0 4px">已寫入</div>${execs.map(e => `<div class="dyn"><div class="row" style="gap:6px"><span class="dim mono">${esc(e.at)}</span>${viaChip(e.via)}${e.revertedBy ? '<span class="chip c-dim">已回復</span>' : ''}</div><div style="font-size:12.5px">${actorHtml(e.actor, e.approver)}</div><div style="font-size:12.5px;margin-top:2px">${execLines(e)}</div>${undoItems(e).some(i => i.ok) ? `<button class="btn sm" data-a="undo-open" data-ex="${e.id}" style="margin-top:4px">回復…</button>` : ''}</div>`).join('')}`);
  return {count:pend.length + execs.length + warn.length, html:rows.join('')};
}
function collab(r){
  const scope = scopeOf(r);
  const dyn = dynamics(scope);
  const auto = !SIM.ai && !dyn.count;   // 沒有內容就自動收合
  const hide = UI.collabHide != null ? UI.collabHide : auto;
  if (hide) return `<aside class="collab rail"><button class="btn sm ghost" data-a="collab-toggle" data-v="0" title="展開協作欄">◀</button><div class="vlabel">${SIM.ai ? 'AI · ' : ''}agent 動態 ${dyn.count}</div></aside>`;
  return `<aside class="collab">
    <div class="ch"><b>協作</b><span class="dim" style="font-size:12.5px">${scope.kind === 'issue' ? '範圍：' + scope.key : '範圍：全部'}</span><span style="flex:1"></span><button class="btn sm ghost" data-a="collab-toggle" data-v="1" title="收合協作欄">▶</button></div>
    ${SIM.ai ? aiPanel(scope) : ''}
    <div class="csec"><div class="row" style="margin-bottom:4px"><b style="font-size:13px">agent 動態</b><span class="dim" style="font-size:12px">外部 agent 與內建 AI 的寫入、待核准</span></div>
      ${dyn.html || `<p class="muted" style="margin:4px 0">${scope.kind === 'issue' ? '這張單沒有 agent 動態。' : '目前沒有 agent 動態。'}</p>`}
      ${scope.kind === 'global' ? '' : '<a class="dim" href="#/agents" style="font-size:12.5px">全部活動在 Agent 委派 →</a>'}
    </div>
  </aside>`;
}

// ---------- 內建 AI（模擬；前端 Q2 開啟時才出現） ----------
function aiPanel(scope){
  const L = (D.aiLog || []).filter(m => m.scope === scopeKey(scope));
  const chips = scope.kind === 'issue' ? ['摘要這張單', '建議指派與優先度', '起草 note 問重現步驟'] : ['沒人處理的高優先度單有哪些？', '這週 agent 改了什麼？'];
  const mode = UI.aiMode || 'review';
  const msg = m => {
    if (m.k === 'u') return `<div class="aimsg u">${esc(m.t)}</div>`;
    let card = '';
    if (m.ops){
      const st = m.exec ? `<span class="chip c-ok">已套用</span> <span class="dim mono">${m.exec}</span>${undoItems(D.execs.find(e => e.id === m.exec)).some(i => i.ok) ? ` <button class="btn sm" data-a="undo-open" data-ex="${m.exec}">回復…</button>` : ''}`
        : m.dropped ? '<span class="chip c-dim">已略過</span>'
        : `<button class="btn sm pri" data-a="ai-apply" data-m="${m.id}">核准並套用</button><button class="btn sm ghost" data-a="ai-drop" data-m="${m.id}">不要</button>`;
      card = `<div class="aicard"><div style="font-size:12.5px">${m.ops.map(o => o.kind === 'update_fields' ? Object.entries(o.fields).map(([f, v]) => `${keyLink(o.target)} ${fieldName(f)} <s class="dim">${esc(fmtVal(f, getField(D.I[o.target], f) === v && m.exec ? m.before[f] : getField(D.I[o.target], f)))}</s> → <b>${esc(fmtVal(f, v))}</b>`).join('<br>') : `${keyLink(o.target)} 新增 note：「${esc(o.text)}」`).join('<br>')}</div><div class="row" style="margin-top:6px;gap:6px">${st}</div></div>`;
    }
    return `<div class="aimsg a"><div class="who">${actorHtml('ag-ai')}</div><div>${m.t}</div>${card}</div>`;
  };
  return `<div class="csec ai">
    <div class="row" style="margin-bottom:6px"><b style="font-size:13px">內建 AI</b><span style="flex:1"></span>
      <span class="dim" style="font-size:12px">寫入：</span><button class="btn sm ${mode === 'review' ? 'pri' : ''}" data-a="ai-mode" data-v="review">需要核准</button><button class="btn sm ${mode === 'direct' ? 'pri' : ''}" data-a="ai-mode" data-v="direct">直接寫入</button></div>
    <p class="hint" style="margin:0 0 6px">只讀和外部 agent 一樣的範圍（不含私有 note、受限欄位）；寫入走同一套委派規則、配額、活動與回復。預設「需要核准」〔待確認 U21〕。</p>
    <div class="ailog">${L.map(msg).join('') || `<div class="dim" style="font-size:12.5px">${scope.kind === 'issue' ? `問 AI 關於 ${scope.key} 的事，或點下面的建議。` : '問 AI 關於你看得到的議題。'}</div>`}${UI.aiBusy ? '<div class="dim">…</div>' : ''}</div>
    <div class="row" style="gap:4px;margin:6px 0">${chips.map(c => `<button class="btn sm" data-a="ai-chip" data-q="${esc(c)}">${esc(c)}</button>`).join('')}</div>
    <div class="row" style="gap:6px"><input id="ai-in" type="text" placeholder="${scope.kind === 'issue' ? '針對 ' + scope.key + '…' : '問 AI…'}" style="flex:1"><button class="btn pri" data-a="ai-send">送出</button></div>
  </div>`;
}
let AI_SEQ = 0;
function aiAsk(q, scope){
  D.aiLog = D.aiLog || [];
  const sk = scopeKey(scope);
  D.aiLog.push({k:'u', t:q, scope:sk});
  UI.aiBusy = true; render();
  setTimeout(() => { UI.aiBusy = false; D.aiLog.push(Object.assign({k:'a', scope:sk, id:'m' + (++AI_SEQ)}, aiAnswer(q, scope))); render(); }, 600);
}
function aiAnswer(q, scope){
  if (scope.kind === 'issue'){
    const is = D.I[scope.key];
    if (is.isPrivate) return {t:'這是私有議題，內建 AI 不讀（和外部 agent 規則相同）。'};
    const pub = is.notes.filter(n => !n.private && !n.retracted);
    if (/摘要|總結|卡在|狀況/.test(q)){
      const last = pub[pub.length - 1];
      return {t:`${keyLink(is.key)}「${esc(is.subject)}」目前「${esc(is.status)}」，指派 ${esc(uname(is.assignee))}，優先度 ${esc(is.priority)}。描述重點：${esc(is.description.slice(0, 50))}…〔描述〕${last ? `<br>最新公開 note（${esc(last.at)} ${esc(uname(last.author))}）：「${esc(last.text.slice(0, 40))}」〔note〕` : ''}<br><span class="dim">只看非私有內容；不判斷是誰卡住。</span>`};
    }
    if (/指派|優先|分流/.test(q)){
      const f = {};
      if (!is.assignee){ const cand = D.P[is.project].assignable.find(u => u !== 'u1'); if (cand) f.assignee = cand; }
      const sev = is.cf.severity;
      if ((sev === 'High' || sev === 'Critical') && !['High','Urgent'].includes(is.priority)) f.priority = sev === 'Critical' ? 'Urgent' : 'High';
      if (!Object.keys(f).length) return {t:'目前指派與優先度看起來和 Severity 一致，不建議修改。'};
      return aiPropose(is, [{kind:'update_fields', target:is.key, fields:f}], `依 Severity「${esc(sev || '未填')}」${f.assignee ? '和可指派名單' : ''}建議：`);
    }
    if (/note|回覆|問|留言|重現/.test(q))
      return aiPropose(is, [{kind:'add_note', target:is.key, text:'可以補充重現步驟嗎？瀏覽器與版本、發生時間、是否從公司入口登入。'}], '起草了一則公開 note：');
  } else {
    if (/高優先|沒人|未指派/.test(q)){
      const xs = Object.values(D.I).filter(i => !i.isPrivate && !i.assignee && ['High','Urgent'].includes(i.priority) && !['已解決','已拒絕'].includes(i.status));
      return {t:xs.length ? `沒人處理的高優先度單：${xs.map(i => `${keyLink(i.key)}「${esc(i.subject)}」`).join('、')}。` : '目前沒有未指派的高優先度單。'};
    }
    if (/agent|改了什麼/.test(q)){
      const xs = D.execs.filter(e => AGENTS[e.actor] && e.via !== 'undo');
      return {t:xs.length ? `今天 agent 共寫入 ${xs.length} 次，涉及 ${new Set(xs.flatMap(e => [...e.created, ...e.changes.map(c => c.key), ...e.notes.map(n => n.key)])).size} 張單；細節在右欄下方或 Agent 委派的活動頁。` : '今天還沒有 agent 寫入。'};
    }
  }
  return {t:'原型只模擬這幾種：摘要、建議指派與優先度、起草 note、找沒人處理的高優先度單、agent 改了什麼。'};
}
function aiPropose(is, ops, lead){
  const before = {}; ops.forEach(o => { if (o.fields) Object.keys(o.fields).forEach(f => before[f] = getField(is, f)); });
  const m = {t:lead, ops, before};
  if ((UI.aiMode || 'review') === 'direct'){ const r = runOps(ops, {actor:'ag-ai', onBehalf:'u1', via:'agent_direct', ctx:'ctx-ai', title:'內建 AI：' + is.key}); m.exec = r.exec.id; m.t = lead + '（已直接寫入）'; }
  return m;
}

// ---------- 首頁：可個人自訂的小工具 ----------
const WIDGETS = {
  decide:{t:'需要你決定', fixed:true, desc:'待核准、套用失敗、agent 暫停、規則提醒。不能移除，只能移動'},
  agent:{t:'agent 動態', desc:'今天外部 agent 與內建 AI 直接改了幾張'},
  mine:{t:'我的工作', desc:'指派給你、未完成'},
  triage:{t:'待分流', desc:'AUTH 新建且未指派'},
  filter:{t:'存起來的篩選', cfg:true, desc:'任一個查詢條件，例如 severity:High'},
  recent:{t:'最近看過', desc:'你最近開過的議題'},
  cont:{t:'繼續處理', desc:'上次做到一半的分組'},
  projects:{t:'專案概況', desc:'各專案未解決數'},
};
const HOME_DEFAULT = [{id:'decide', key:'w1', wide:true}, {id:'agent', key:'w2'}, {id:'mine', key:'w3'}, {id:'triage', key:'w4'}, {id:'cont', key:'w5'}];
function homePrefs(){
  if (!UI.home){
    let saved = null; try { saved = JSON.parse(localStorage.getItem('ox.home.alice') || 'null'); } catch (e){}
    UI.home = {items:saved || JSON.parse(JSON.stringify(HOME_DEFAULT)), edit:false, adding:false};
  }
  return UI.home;
}
function saveHome(){ try { localStorage.setItem('ox.home.alice', JSON.stringify(UI.home.items)); } catch (e){} }
function wDecide(){
  const ctx = activeCtx(); const broken = ctx && !ctxUsable(ctx);
  const props = awaiting(); const dead = pending().filter(c => c.state === 'proposed' && blockedBy(c)); const failed = D.changesets.filter(c => c.state === 'failed');
  const grouped = {}; props.forEach(c => { const k = c.group || '_'; (grouped[k] = grouped[k] || []).push(c); });
  const items = [];
  D.contexts.filter(c => c.state === 'active' && D.ctl[c.id] && D.ctl[c.id].suspended).forEach(c => items.push(`<div class="card" style="border-color:var(--warn)"><div class="row"><span class="chip c-warn">需要處理</span><b>${actorHtml(c.agent)} 的寫入已暫停</b></div><p class="muted" style="margin:6px 0">原因：${esc(D.ctl[c.id].reason)}（${esc(D.ctl[c.id].at)}）。讀取照常；在你恢復前它不能再改任何東西。</p><div class="row"><a class="btn pri" href="#/agents/activity/${c.id}">檢視活動與回復</a><button class="btn" data-a="resume" data-id="${c.id}">恢復寫入</button></div></div>`));
  if (broken) items.push(`<div class="card" style="border-color:var(--warn)"><div class="row"><span class="chip c-warn">需要處理</span><b>${esc(ctx.clientName)} 的工作脈絡已停用</b></div><p class="muted" style="margin:6px 0">原因：${ctxBrokenBy(ctx).join('、')} 的權限設定變更。這個 agent 的 ${pending().filter(c => c.ctx === ctx.id && c.state === 'proposed').length} 份待審提案都不能套用。</p><a class="btn" href="#/agents">檢視並建立新工作脈絡</a></div>`);
  for (const [gid, list] of Object.entries(grouped)){
    if (gid !== '_'){ const g = groupOf(gid); items.push(`<div class="card p"><div class="row"><span class="chip c-prop">待核准</span>${groupLabel(g)}<span class="muted">${list.length} 份提案 · ${esc(D.contexts.find(c => c.id === g.ctx).clientName)}</span><span style="flex:1"></span><a class="btn prop" href="#/review?group=${gid}">開啟分組</a></div></div>`); }
    else list.slice(0, 5).forEach(c => items.push(`<div class="card p"><div class="row"><span class="chip c-prop">待核准</span><b>提案 #${c.no}</b><span class="agent-label">${esc(c.title)}</span><span style="flex:1"></span><a class="btn prop" href="#/cs/${c.id}">審閱</a></div><div style="margin-top:6px">${opLines(c, curVer(c), false)}</div></div>`));
  }
  if (dead.length) items.push(`<div class="card"><div class="row"><span class="chip c-dim">無法套用</span><span>${dead.length} 份提案的委派或工作脈絡已失效：${esc(errText(blockedBy(dead[0]).code, blockedBy(dead[0])))}</span><span style="flex:1"></span><a class="btn sm" href="#/review">查看</a></div></div>`);
  failed.forEach(c => items.push(`<div class="card" style="border-color:var(--bad)"><div class="row"><span class="chip c-bad">套用失敗</span><b>提案 #${c.no}</b> <span class="muted">${esc(lastErrText(c))}</span><span style="flex:1"></span><a class="btn" href="#/cs/${c.id}">處理</a></div></div>`));
  if (!UI.reminders.off && !UI.reminders.snoozed['AUTH-123'] && D.I['AUTH-123'].status === '處理中' && D.I['AUTH-123'].assignee !== 'u1')
    items.push(`<div class="card"><div class="row"><span class="chip c-act">規則提醒</span>${keyLink('AUTH-123')}<span>在「處理中」停留 3 天，客戶期限 11/08</span><span style="flex:1"></span><a class="btn sm" href="#/issue/AUTH-123">開啟</a><button class="btn sm" data-a="snooze" data-k="AUTH-123">明天 09:00 再提醒</button><button class="btn sm ghost" data-a="rule-off">停用這條規則</button></div><p class="hint" style="margin:4px 0 0">由「高優先度超過 3 天未變更狀態」規則產生（不是 AI 判斷）。條件解除就自動結案。</p></div>`);
  return items.join('') || '<span class="muted">目前沒有。</span>';
}
const issueTable = xs => xs.length ? `<table class="t"><tr><th>單號</th><th>標題</th><th>狀態</th><th>優先度</th></tr>${xs.slice(0, 8).map(i => `<tr><td>${keyLink(i.key)}</td><td>${esc(i.subject)}</td><td>${stChip(i.status)}</td><td>${esc(i.priority)}</td></tr>`).join('')}</table>${xs.length > 8 ? `<div class="dim" style="font-size:12.5px">還有更多</div>` : ''}` : '<span class="muted">沒有符合的議題。</span>';
function wBody(it){
  switch (it.id){
    case 'decide': return wDecide();
    case 'agent': {
      const sums = D.contexts.filter(c => c.state === 'active' && c.agent && todaySummary(c) > 0);
      const ai = D.execs.filter(e => e.actor === 'ag-ai' && e.via !== 'undo' && !e.revertedBy).length;
      return (sums.map(c => `<div class="row"><span>${actorHtml(c.agent)} 今天直接改了 <b>${todaySummary(c)}</b> 張</span><span style="flex:1"></span><a class="btn sm" href="#/agents/activity/${c.id}">檢視</a></div>`).join('') + (ai ? `<div class="row" style="margin-top:4px"><span>${actorHtml('ag-ai')} 今天寫入 <b>${ai}</b> 次</span></div>` : '')) || '<span class="muted">今天還沒有 agent 寫入。</span>';
    }
    case 'mine': return issueTable(Object.values(D.I).filter(i => i.assignee === 'u1' && !['已解決','已拒絕'].includes(i.status)));
    case 'triage': { const xs = Object.values(D.I).filter(i => i.project === 'AUTH' && i.status === '新建' && !i.assignee); return `${issueTable(xs)}<a class="btn sm" style="margin-top:6px" href="#/issues?q=project:AUTH%20status:新建%20assignee:none">全部 ${xs.length} 張</a>`; }
    case 'filter': {
      const q = it.q || 'severity:High';
      if (UI.home.edit) return `<label class="f"><span>查詢條件</span><input type="text" data-a="w-q" data-key="${it.key}" value="${esc(q)}"></label><p class="hint">和議題頁的搜尋語法相同；標題用條件本身。</p>`;
      const r = search({q, limit:8}); return issueTable(r.items.map(x => x.is));
    }
    case 'recent': { const ks = (UI.viewed || []).filter(k => D.I[k]).slice(0, 6); return ks.length ? ks.map(k => `<div>${keyLink(k)} ${esc(D.I[k].subject)}</div>`).join('') : '<span class="muted">還沒有開過議題。</span>'; }
    case 'cont': { const g = D.groups[D.groups.length - 1]; if (!g) return '<span class="muted">沒有做到一半的分組。</span>'; const all = D.changesets.filter(c => c.group === g.id); const done = all.filter(c => c.state === 'succeeded').length; return `<div class="row">${groupLabel(g)}<span class="muted">已套用 ${done}／${all.length}</span><span style="flex:1"></span><a class="btn sm" href="#/review?group=${g.id}">繼續</a></div>`; }
    case 'projects': return `<table class="t"><tr><th>專案</th><th>未解決</th><th>新建未指派</th></tr>${Object.values(D.P).map(p => { const xs = Object.values(D.I).filter(i => i.project === p.key && !i.isPrivate); return `<tr><td><a href="#/issues?q=project:${p.key}">${p.key}</a> ${esc(p.name)}</td><td>${xs.filter(i => !['已解決','已拒絕'].includes(i.status)).length}</td><td>${xs.filter(i => i.status === '新建' && !i.assignee).length}</td></tr>`; }).join('')}</table>`;
  }
  return window.wTeam ? wTeam(it) : '';
}
function homePage(){
  const H = homePrefs(); const E = H.edit;
  const absent = Object.keys(WIDGETS).filter(id => id === 'filter' || !H.items.some(x => x.id === id));
  const w = (it, i) => {
    const W = WIDGETS[it.id];
    const title = it.id === 'filter' ? `篩選：${esc(it.q || 'severity:High')}` : W.t;
    const tools = E ? `<span style="flex:1"></span><button class="btn sm ghost" data-a="w-mv" data-i="${i}" data-d="-1" ${i ? '' : 'disabled'} title="往前">↑</button><button class="btn sm ghost" data-a="w-mv" data-i="${i}" data-d="1" ${i < H.items.length - 1 ? '' : 'disabled'} title="往後">↓</button><button class="btn sm ghost" data-a="w-wide" data-i="${i}">${it.wide ? '窄' : '寬'}</button>${W.fixed ? '<span class="chip c-dim" title="安全相關提醒不能隱藏">固定</span>' : `<button class="btn sm ghost" data-a="w-rm" data-i="${i}" title="移除">✕</button>`}` : '';
    return `<section class="widget ${it.wide ? 'wide' : ''} ${E ? 'editing' : ''}"><div class="wh"><b>${title}</b>${tools}</div>${wBody(it)}</section>`;
  };
  return `<div class="page">
    <div class="row" style="margin-bottom:12px"><h1 style="margin:0">首頁</h1><span class="hint">${SIM.ai ? '內建 AI 已開啟：問題可以直接問右欄的 AI。' : '內建 AI 未啟用（前端 Q2）：AI 透過你委派的外部 agent 參與。'}</span><span style="flex:1"></span>
      ${E ? `<button class="btn sm" data-a="w-add">＋ 新增小工具</button><button class="btn sm ghost" data-a="w-reset">恢復預設</button><button class="btn sm pri" data-a="w-done">完成</button>` : '<button class="btn sm" data-a="w-edit">自訂首頁</button>'}</div>
    ${E ? '<div class="banner i">拖拉之外，用 ↑↓ 調順序、寬／窄調大小。設定只存在你的帳號，不影響別人。「需要你決定」不能移除，避免 agent 暫停或套用失敗被藏起來。</div>' : ''}
    ${E && H.adding ? `<div class="card"><div class="k" style="margin-bottom:6px">可加入的小工具</div>${absent.map(id => `<div class="row" style="margin:4px 0"><b style="min-width:110px">${WIDGETS[id].t}</b><span class="dim" style="font-size:12.5px">${WIDGETS[id].desc}</span><span style="flex:1"></span><button class="btn sm" data-a="w-put" data-id="${id}">加入</button></div>`).join('')}</div>` : ''}
    <div class="wgrid">${H.items.map(w).join('')}</div>
  </div>`;
}

// ---------- 動作 ----------
function actLayout(a, el){
  const d = el.dataset; const H = UI.home;
  switch (a){
    case 'nav-mini': UI.navMini = !UI.navMini; render(); return true;
    case 'collab-toggle': UI.collabHide = d.v === '1'; render(); return true;
    case 'ai-mode': UI.aiMode = d.v; toast(d.v === 'direct' ? '內建 AI 改成直接寫入（原型）' : '內建 AI 改成需要核准'); render(); return true;
    case 'ai-chip': aiAsk(d.q, scopeOf(parseHash())); return true;
    case 'ai-send': { const v = ($('#ai-in').value || '').trim(); if (!v) return true; aiAsk(v, scopeOf(parseHash())); return true; }
    case 'ai-apply': { const m = D.aiLog.find(x => x.id === d.m); const key = m.ops[0].target; const r = runOps(m.ops, {actor:'ag-ai', onBehalf:'u1', approver:'u1', via:'changeset', ctx:'ctx-ai', where:'右欄 AI 對話', title:'內建 AI：' + key}); m.exec = r.exec.id; UI.openLock[key] = D.I[key].lock; toast('已套用', 'ok'); render(); return true; }
    case 'ai-drop': { const m = D.aiLog.find(x => x.id === d.m); m.dropped = true; render(); return true; }
    case 'w-edit': homePrefs().edit = true; render(); return true;
    case 'w-done': H.edit = false; H.adding = false; saveHome(); toast('已儲存你的首頁', 'ok'); render(); return true;
    case 'w-reset': H.items = window.personaDefault ? personaDefault() : JSON.parse(JSON.stringify(HOME_DEFAULT)); render(); return true;
    case 'w-add': H.adding = !H.adding; render(); return true;
    case 'w-put': H.items.push({id:d.id, key:'w' + Date.now(), q:d.id === 'filter' ? 'severity:High' : undefined}); H.adding = false; render(); return true;
    case 'w-rm': H.items.splice(+d.i, 1); render(); return true;
    case 'w-wide': H.items[+d.i].wide = !H.items[+d.i].wide; render(); return true;
    case 'w-mv': { const i = +d.i, j = i + (+d.d); [H.items[i], H.items[j]] = [H.items[j], H.items[i]]; render(); return true; }
  }
  return false;
}
document.addEventListener('change', e => { const t = e.target; if (t.dataset.a === 'w-q'){ const it = UI.home.items.find(x => x.key === t.dataset.key); it.q = t.value; render(); } });
document.addEventListener('keydown', e => { if (e.key === 'Enter' && e.target.id === 'ai-in' && !e.isComposing && e.keyCode !== 229) actLayout('ai-send', e.target); });

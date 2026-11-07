// oxledger 原型：時程（里程碑、專案時間軸）與右欄常駐 AI 對話
// 使用者 2026-11-07：「時序的部分太不明顯，且專案下應該要可以設定 milestone 等；另外右側沒有直接與 AI 對話的功能」
'use strict';

const TODAY = '2026-11-07';
const dnum = s => Math.round(Date.parse(s + 'T00:00:00Z') / 864e5);
const dfmt = n => new Date(n * 864e5).toISOString().slice(5, 10).replace('-', '/');
const DONE = ['已解決', '已拒絕'];

// ---------- 資料：里程碑（對應 Redmine 的 Version／target version） ----------
function addMsData(d){
  if (d.MS) return d;
  d.MS = {
    AUTH:[
      {id:'ms-a19', name:'v1.9 修正版', date:'2026-10-30', status:'closed', desc:'Safari cookie 修正', log:[]},
      {id:'ms-a20', name:'v2.0 SSO 上線', date:'2026-11-20', status:'open', desc:'公司入口 SSO、登入頁改版', log:[]},
      {id:'ms-a21', name:'v2.1 MFA 強化', date:'2026-12-18', status:'open', desc:'MFA 簡訊、備用碼、passkey 評估', log:[]},
    ],
    WEB:[{id:'ms-w11', name:'W-11 首頁改版', date:'2026-11-27', status:'open', desc:'', log:[]}],
    OPS:[],
  };
  const set = (k, ms, s, e) => { const is = d.I[k]; if (!is) return; is.milestone = ms; is.start_date = s; is.due_date = e; };
  set('AUTH-88', 'ms-a19', '2026-10-14', '2026-10-28');
  set('AUTH-101', 'ms-a20', '2026-10-21', '2026-11-05');
  set('AUTH-123', 'ms-a20', '2026-11-03', '2026-11-12');
  set('AUTH-91', 'ms-a20', '2026-11-04', '2026-11-14');
  set('AUTH-140', 'ms-a20', '2026-11-07', '2026-11-18');
  set('AUTH-127', 'ms-a20', null, null);
  set('AUTH-110', 'ms-a21', '2026-11-10', '2026-11-28');
  set('AUTH-132', 'ms-a21', '2026-11-12', '2026-11-24');
  set('AUTH-134', 'ms-a21', '2026-12-01', '2026-12-15');
  return d;
}
const _initialData2 = initialData;
initialData = function(){ return addMsData(_initialData2()); };
addMsData(D);
const msOf = id => { for (const p in D.MS){ const m = D.MS[p].find(x => x.id === id); if (m) return m; } return null; };
function msName(id){ const m = msOf(id); return m ? m.name : id; }
function msStats(m, project){
  const xs = Object.values(D.I).filter(i => i.project === project && i.milestone === m.id && !i.isPrivate);
  const done = xs.filter(i => DONE.includes(i.status)).length;
  const left = dnum(m.date) - dnum(TODAY);
  const overdue = xs.filter(i => i.due_date && !DONE.includes(i.status) && dnum(i.due_date) < dnum(TODAY));
  const late = xs.filter(i => i.due_date && !DONE.includes(i.status) && dnum(i.due_date) > dnum(m.date));
  const nodate = xs.filter(i => !i.due_date && !DONE.includes(i.status));
  const health = m.status === 'closed' ? 'closed' : (overdue.length || late.length) ? 'risk' : 'ok';
  return {xs, done, total:xs.length, left, overdue, late, nodate, health};
}
const healthChip = h => h === 'closed' ? '<span class="chip c-dim">已關閉</span>' : h === 'risk' ? '<span class="chip c-warn">At risk</span>' : '<span class="chip c-ok">On track</span>';
const msStatusLabel = {open:'進行中', locked:'鎖定（不能再加單）', closed:'已關閉'};

// ---------- 專案頁：概況（時間軸）／里程碑設定／規劃 ----------
function pgProject(r){
  const key = r.parts[1]; const P = D.P[key]; if (!P) return '<div class="page">找不到專案。</div>';
  const tab = r.parts[2] || 'timeline';
  const tabs = [['timeline', '時程'], ['milestones', '里程碑'], ['issues', '議題'], ['plan', '規劃（心智圖）']];
  const head = `<div class="row" style="margin-bottom:6px"><h1 style="margin:0">${key} ${esc(P.name)}</h1></div>
    <div class="ptabs">${tabs.map(([k, t]) => k === 'issues' ? `<a href="#/issues?q=project:${key}">${t}</a>` : `<a href="#/project/${key}/${k}" class="${tab === k ? 'on' : ''}">${t}</a>`).join('')}</div>`;
  let body = '';
  if (tab === 'timeline') body = pgTimeline(key);
  else if (tab === 'milestones') body = pgMilestones(key);
  else if (tab === 'plan') body = pgPlan().replace('<div class="page">', '<div>');
  return `<div class="page">${head}${body}</div>`;
}
function pgTimeline(key){
  const msList = (D.MS[key] || []).slice().sort((a, b) => dnum(a.date) - dnum(b.date));
  const all = Object.values(D.I).filter(i => i.project === key && !i.isPrivate);
  const T = UI.tl = UI.tl || {range:'q', showDone:false};
  const d0 = dnum('2026-10-13'), d1 = T.range === 'm' ? dnum('2026-12-07') : dnum('2026-12-28');
  const span = d1 - d0, fr = n => ((Math.min(Math.max(n, d0), d1) - d0) / span).toFixed(4), pct = n => `calc(180px + (100% - 180px) * ${fr(n)})`;   // 左側 180px 放標題
  const weeks = []; for (let n = d0; n <= d1; n += 7) weeks.push(n);
  const today = dnum(TODAY);
  const bar = i => {
    if (!i.start_date || !i.due_date) return '';
    const s = dnum(i.start_date), e = dnum(i.due_date) + 1;
    const od = !DONE.includes(i.status) && dnum(i.due_date) < today;
    const m = msOf(i.milestone); const late = m && !DONE.includes(i.status) && dnum(i.due_date) > dnum(m.date);
    const cls = DONE.includes(i.status) ? 'done' : i.status === '處理中' ? 'wip' : i.status === '待驗證' ? 'ver' : 'new';
    return `<a class="tbar ${cls} ${od ? 'od' : ''} ${late ? 'late' : ''}" href="#/issue/${i.key}" style="left:${pct(s)};width:calc((100% - 180px) * ${(fr(e) - fr(s)).toFixed(4)})" title="${esc(i.key + ' ' + i.subject)}｜${i.start_date} → ${i.due_date}｜${i.status}${od ? '｜逾期' : ''}${late ? '｜晚於里程碑' : ''}"><span>${i.key}</span></a>`;
  };
  const lane = (title, m, xs) => {
    const st = m ? msStats(m, key) : null;
    const rows = xs.filter(i => T.showDone || !DONE.includes(i.status));
    const dated = rows.filter(i => i.start_date && i.due_date), undated = rows.filter(i => !(i.start_date && i.due_date));
    return `<div class="tlane">
      <div class="tlh"><b>${title}</b>${m ? ` <span class="mono dim">${m.date.slice(5).replace('-', '/')}</span> ${healthChip(st.health)} <span class="dim" style="font-size:12px">${st.done}/${st.total} 完成${m.status !== 'closed' ? ` · ${st.left >= 0 ? '還有 ' + st.left + ' 天' : '已過 ' + (-st.left) + ' 天'}` : ''}</span>` : ''}</div>
      <div class="tgrid">${m ? `<div class="tms" style="left:${pct(dnum(m.date) + 0.5)}" title="${esc(m.name)} ${m.date}"></div>` : ''}${dated.map(i => `<div class="trow">${bar(i)}<span class="tlab" title="${esc(i.subject)}">${i.key} ${esc(i.subject)}</span></div>`).join('') || '<div class="trow"></div>'}</div>
      ${undated.length ? `<div class="hint" style="padding:2px 0 6px 0">未排日期：${undated.map(i => keyLink(i.key)).join('、')}</div>` : ''}
    </div>`;
  };
  const risks = msList.filter(m => m.status !== 'closed').map(m => [m, msStats(m, key)]).filter(([m, s]) => s.health === 'risk');
  return `
    <div class="row" style="margin:6px 0 10px"><span class="dim" style="font-size:12.5px">範圍</span><button class="btn sm ${T.range === 'm' ? 'on' : ''}" data-a="tl-range" data-v="m">8 週</button><button class="btn sm ${T.range === 'q' ? 'on' : ''}" data-a="tl-range" data-v="q">一季</button>
      <label class="row" style="gap:4px;font-size:12.5px;margin-left:10px"><input type="checkbox" data-a="tl-done" ${T.showDone ? 'checked' : ''}> 顯示已完成</label><span style="flex:1"></span>
      <span class="tleg"><i class="new"></i>新建 <i class="wip"></i>處理中 <i class="ver"></i>待驗證 <i class="done"></i>完成 <i class="od"></i>逾期 <i class="late"></i>晚於里程碑</span>
      <a class="btn sm" href="#/project/${key}/milestones">管理里程碑</a></div>
    ${risks.length ? `<div class="banner w" style="display:block">${risks.map(([m, s]) => `<div><b>${esc(m.name)}</b>（${m.date}）：${s.overdue.length ? `逾期 ${s.overdue.map(i => keyLink(i.key)).join('、')}` : ''}${s.overdue.length && s.late.length ? '；' : ''}${s.late.length ? `到期日晚於里程碑 ${s.late.map(i => keyLink(i.key)).join('、')}` : ''}${s.nodate.length ? `；未排日期 ${s.nodate.length} 張` : ''}</div>`).join('')}<div class="hint">由日期規則算出（不是 AI 判斷）。</div></div>` : ''}
    <div class="card tl">
      <div class="thead">${weeks.map(n => `<span style="left:${pct(n)}">${dfmt(n)}</span>`).join('')}</div>
      <div class="tbody">
        <div class="ttoday" style="left:${pct(today + 0.5)}"><span>今天 11/07</span></div>
        ${msList.map(m => lane(esc(m.name), m, all.filter(i => i.milestone === m.id))).join('')}
        ${lane('未排入里程碑', null, all.filter(i => !i.milestone && !DONE.includes(i.status)).slice(0, 6))}
      </div>
    </div>
    <p class="hint">時程由各單的開始日、到期日與里程碑日期推導（投影視圖）。拖拉改日期屬於領域變更，要走提案或直接編輯，這版原型不示範拖拉；改日期請到議題頁「編輯欄位」或問右欄 AI。</p>`;
}
function pgMilestones(key){
  const list = D.MS[key] || []; const F = UI.msForm || {};
  return `
    <p class="muted">里程碑（對應 Redmine 的「版本」）：每張單可以掛一個目標里程碑。關閉後不能再把單排進來；有單的里程碑不能刪，只能關閉。改日期會檢查已排入的單。</p>
    <div class="card"><table class="t"><tr><th>名稱</th><th>日期</th><th>狀態</th><th>進度</th><th>健康度</th><th></th></tr>
      ${list.slice().sort((a, b) => dnum(a.date) - dnum(b.date)).map(m => { const s = msStats(m, key); return `<tr>
        <td><b>${esc(m.name)}</b><div class="hint">${esc(m.desc || '')}</div></td>
        <td><input type="date" data-ms="date" data-id="${m.id}" value="${m.date}" style="width:auto"></td>
        <td><select data-ms="status" data-id="${m.id}" style="width:auto">${Object.entries(msStatusLabel).map(([k, t]) => `<option value="${k}" ${m.status === k ? 'selected' : ''}>${t}</option>`).join('')}</select></td>
        <td><div class="prog"><i style="width:${s.total ? Math.round(s.done / s.total * 100) : 0}%"></i></div><span class="dim" style="font-size:12px">${s.done}/${s.total}</span></td>
        <td>${healthChip(s.health)}</td>
        <td>${s.total ? '' : `<button class="btn sm ghost" data-a="ms-del" data-p="${key}" data-id="${m.id}">刪除</button>`}</td></tr>
        ${m.warn ? `<tr><td colspan="6"><div class="banner w" style="margin:0">${m.warn}</div></td></tr>` : ''}`; }).join('')}
    </table></div>
    <div class="card"><div class="k" style="margin-bottom:6px">新增里程碑</div>
      <div class="row"><input type="text" id="ms-name" placeholder="例如：v2.2 管理後台" style="flex:1"><input type="date" id="ms-date" value="2027-01-15" style="width:auto"><button class="btn pri" data-a="ms-add" data-p="${key}">新增</button></div>
      <p class="hint">分享範圍（子專案、整個站台）先不做；第一版只在專案內。</p></div>`;
}
// 議題頁屬性列：里程碑與日期
function msCells(is, ed){
  const list = (D.MS[is.project] || []).filter(m => m.status !== 'closed' || m.id === is.milestone);
  const m = msOf(is.milestone);
  const v = f => UI.form[f] ?? is[f] ?? '';
  const late = m && is.due_date && dnum(is.due_date) > dnum(m.date) && !DONE.includes(is.status);
  return `<dt>里程碑</dt><dd>${ed ? `<select data-f="milestone"><option value="">（未排）</option>${list.map(x => `<option value="${x.id}" ${v('milestone') === x.id ? 'selected' : ''} ${x.status === 'locked' ? 'disabled' : ''}>${esc(x.name)}（${x.date.slice(5)}）</option>`).join('')}</select>` : m ? `<a href="#/project/${is.project}/timeline">${esc(m.name)}</a> <span class="dim mono">${m.date.slice(5)}</span>` : '<span class="dim">（未排）</span>'}</dd>
    <dt>開始 → 到期</dt><dd>${ed ? `<input type="date" data-f="start_date" value="${v('start_date')}" style="width:auto"> → <input type="date" data-f="due_date" value="${v('due_date')}" style="width:auto">` : is.due_date ? `<span class="mono">${(is.start_date || '').slice(5)} → ${is.due_date.slice(5)}</span>${late ? ' <span class="chip c-warn">晚於里程碑</span>' : ''}${!DONE.includes(is.status) && dnum(is.due_date) < dnum(TODAY) ? ' <span class="chip c-bad">逾期</span>' : ''}` : '<span class="dim">（未排）</span>'}</dd>`;
}
const _fmtVal = fmtVal;
fmtVal = function(f, v){ if (f === 'milestone') return v ? msName(v) : '（未排）'; return _fmtVal(f, v); };
const _fieldName = fieldName;
fieldName = function(f){ return f === 'milestone' ? '里程碑' : _fieldName(f); };

// 首頁小工具：里程碑倒數
WIDGETS.milestone = {t:'里程碑', desc:'接下來的里程碑：倒數、進度、風險'};
const _wTeam = wTeam;
wTeam = function(it){
  if (it.id !== 'milestone') return _wTeam(it);
  const rows = Object.entries(D.MS).flatMap(([p, xs]) => xs.filter(m => m.status !== 'closed').map(m => [p, m, msStats(m, p)])).sort((a, b) => dnum(a[1].date) - dnum(b[1].date)).slice(0, 4);
  return rows.map(([p, m, s]) => `<div class="msw"><div class="row"><a href="#/project/${p}/timeline"><b>${p} ${esc(m.name)}</b></a>${healthChip(s.health)}<span style="flex:1"></span><span class="mono ${s.left <= 14 ? 'c-warn' : 'dim'}">${s.left} 天</span></div><div class="prog"><i style="width:${s.total ? Math.round(s.done / s.total * 100) : 0}%"></i></div><div class="dim" style="font-size:12px">${s.done}/${s.total} 完成${s.overdue.length ? ` · 逾期 ${s.overdue.length}` : ''}${s.late.length ? ` · 晚於里程碑 ${s.late.length}` : ''}</div></div>`).join('') || '<span class="muted">沒有進行中的里程碑。</span>';
};
PERSONA_TPL.pm.splice(1, 0, 'milestone');
PERSONA_TPL.eng.splice(2, 0, 'milestone');
PERSONA_TPL.design.splice(2, 0, 'milestone');
(function(){ let saved = null; try { saved = localStorage.getItem('ox.home.alice'); } catch (e){} if (!saved && UI.home && !UI.home.edit) UI.home.items = personaDefault(); })();

// ---------- 右欄：常駐 AI 對話（U38） ----------
function chatScope(r){
  if (r.parts[0] === 'issue' && D.I[r.parts[1]]) return {kind:'issue', key:r.parts[1], label:r.parts[1]};
  if (r.parts[0] === 'project' && D.P[r.parts[1]]) return {kind:'project', key:r.parts[1], label:'專案 ' + r.parts[1]};
  return {kind:'global', key:'global', label:'全部'};
}
const chatKey = s => s.kind === 'global' ? 'global' : s.kind + ':' + s.key;
aiAsk = function(q, scope){
  const s = scope && scope.label ? scope : chatScope(parseHash());
  const to = UI.chatTo || 'ag-ai';
  D.aiLog = D.aiLog || [];
  D.aiLog.push({k:'u', t:q, scope:chatKey(s), to});
  UI.aiBusy = true; UI.collabTab = 'chat'; render();
  setTimeout(() => { UI.aiBusy = false; D.aiLog.push(Object.assign({k:'a', scope:chatKey(s), id:'m' + (++AI_SEQ), who:to}, to === 'ag-ai' ? aiAnswer2(q, s) : botAnswer(to, q, s))); render(); const l = document.querySelector('.chatlog'); if (l) l.scrollTop = l.scrollHeight; }, 500);
};
function aiAnswer2(q, s){
  if (/里程碑|時程|進度|風險|延期|到期|排進|milestone/i.test(q)){
    if (s.kind === 'issue'){
      const is = D.I[s.key]; const m = msOf(is.milestone);
      if (/排進|改到|移到/.test(q)){
        const next = (D.MS[is.project] || []).filter(x => x.status === 'open' && x.id !== is.milestone).sort((a, b) => dnum(a.date) - dnum(b.date))[0];
        if (!next) return {t:'沒有其他進行中的里程碑可以排。'};
        return aiPropose(is, [{kind:'update_fields', target:is.key, fields:{milestone:next.id}}], `建議把 ${keyLink(is.key)} 排進「${esc(next.name)}」（${next.date}）：`);
      }
      if (!m) return {t:`${keyLink(is.key)} 還沒排進里程碑。可以說「排進下一個里程碑」。`};
      const late = is.due_date && dnum(is.due_date) > dnum(m.date);
      return {t:`${keyLink(is.key)} 在「${esc(m.name)}」（${m.date}，還有 ${dnum(m.date) - dnum(TODAY)} 天）。到期日 ${is.due_date || '未排'}${late ? '，<b>晚於里程碑</b>，建議提早或改排下一個里程碑' : ''}。〔里程碑、到期日〕`};
    }
    const ps = s.kind === 'project' ? [s.key] : Object.keys(D.MS);
    const lines = ps.flatMap(p => (D.MS[p] || []).filter(m => m.status !== 'closed').map(m => { const st = msStats(m, p); return `<div>${p}「${esc(m.name)}」${m.date}（${st.left} 天）：${st.done}/${st.total} 完成${st.overdue.length ? `，逾期 ${st.overdue.map(i => keyLink(i.key)).join('、')}` : ''}${st.late.length ? `，晚於里程碑 ${st.late.map(i => keyLink(i.key)).join('、')}` : ''}${st.nodate.length ? `，未排日期 ${st.nodate.map(i => keyLink(i.key)).join('、')}` : ''}</div>`; }));
    return {t:(lines.join('') || '沒有進行中的里程碑。') + '<span class="dim">依日期規則計算；要不要延期由你決定。</span>'};
  }
  if (s.kind === 'project') return aiAnswer(q, {kind:'global'});
  return aiAnswer(q, s);
}
function botAnswer(to, q, s){
  const b = botById(to); const T = teamState();
  if (T.paused[to]) return {t:`${esc(b.handle)} 已暫停，不能交辦。`};
  if (b.onBehalf !== ME){ T.requests.push({bot:to, text:q, state:'sent'}); return {t:`你不是 ${esc(b.handle)} 的 sponsor，這則已轉成請求，送到 ${esc(uname(b.onBehalf))} 的「需要你決定」。`}; }
  if (to === 'ag-qa' && /AUTH-110|回歸|檢查/.test(q)){ actTeam('bot-run', {dataset:{id:'ag-qa'}}); return {t:'已在 AUTH-110 跑回歸並留言（直接寫入）；要把狀態退回「處理中」，已送你核准（AI 團隊 › QA）。'}; }
  return {t:`收到。我的職責是：${esc(b.duty)} 這則會照允許清單處理。`};
}
function chatPanel(s){
  const L = (D.aiLog || []).filter(m => (m.scope === chatKey(s)) || (s.kind === 'global' && m.scope === 'global'));
  const to = UI.chatTo || 'ag-ai';
  const chips = s.kind === 'issue' ? ['摘要這張單', '建議指派與優先度', '這張單的里程碑與到期日', '排進下一個里程碑', '起草 note 問重現步驟']
    : s.kind === 'project' ? ['里程碑進度與風險', '沒人處理的高優先度單有哪些？'] : ['里程碑進度與風險', '沒人處理的高優先度單有哪些？', '這週 agent 改了什麼？'];
  const msg = m => {
    if (m.k === 'u') return `<div class="aimsg u">${m.to && m.to !== 'ag-ai' ? `<span class="dim" style="font-size:11.5px">給 ${esc(AGENTS[m.to].handle)}</span><br>` : ''}${esc(m.t)}</div>`;
    let card = '';
    if (m.ops){
      const st = m.exec ? `<span class="chip c-ok">已套用</span>${undoItems(D.execs.find(e => e.id === m.exec)).some(i => i.ok) ? ` <button class="btn sm" data-a="undo-open" data-ex="${m.exec}">回復…</button>` : ''}`
        : m.dropped ? '<span class="chip c-dim">已略過</span>' : `<button class="btn sm pri" data-a="ai-apply" data-m="${m.id}">核准並套用</button><button class="btn sm ghost" data-a="ai-drop" data-m="${m.id}">不要</button>`;
      card = `<div class="aicard"><div style="font-size:12.5px">${m.ops.map(o => o.kind === 'update_fields' ? Object.entries(o.fields).map(([f, v]) => `${keyLink(o.target)} ${fieldName(f)} → <b>${esc(fmtVal(f, v))}</b>`).join('<br>') : `${keyLink(o.target)} 新增 note：「${esc(o.text)}」`).join('<br>')}</div><div class="row" style="margin-top:6px;gap:6px">${st}</div></div>`;
    }
    return `<div class="aimsg a"><div class="who">${actorHtml(m.who || 'ag-ai')}</div><div>${m.t}</div>${card}</div>`;
  };
  const off = !SIM.ai;
  return `<div class="chat">
    <div class="row chatto"><span class="dim" style="font-size:12px">對象</span><select data-a="chat-to" style="width:auto;flex:1"><option value="ag-ai" ${to === 'ag-ai' ? 'selected' : ''}>內建 AI（oxledger-ai@alice）</option>${ROLE_BOTS.map(b => `<option value="${b.id}" ${to === b.id ? 'selected' : ''}>${esc(b.title)}：${esc(b.handle)}${b.onBehalf === ME ? '' : '（提出請求）'}</option>`).join('')}</select></div>
    ${to === 'ag-ai' ? `<div class="row" style="gap:4px;font-size:12px"><span class="dim">寫入</span><button class="btn sm ${(UI.aiMode || 'review') === 'review' ? 'on' : ''}" data-a="ai-mode" data-v="review">需要核准</button><button class="btn sm ${UI.aiMode === 'direct' ? 'on' : ''}" data-a="ai-mode" data-v="direct">直接寫入</button><span class="hint" title="讀取範圍和外部 agent 一樣，不含私有 note 與受限欄位">讀取同 agent 範圍</span></div>` : ''}
    <div class="chatlog">${off && to === 'ag-ai' ? '<div class="banner w">管理員已關閉內建 AI（情境控制可切換）。仍可以把訊息交給 AI 團隊角色。</div>' : ''}${L.map(msg).join('') || `<div class="dim" style="font-size:12.5px">在這裡直接和 AI 對話。範圍跟著中間：現在是「${esc(s.label)}」。</div>`}${UI.aiBusy ? '<div class="dim">…</div>' : ''}</div>
    <div class="chips">${chips.map(c => `<button class="btn sm" data-a="ai-chip" data-q="${esc(c)}">${esc(c)}</button>`).join('')}</div>
    <div class="chatin"><textarea id="ai-in" rows="2" placeholder="${s.kind === 'issue' ? '針對 ' + s.key + ' 問 AI…（Enter 送出，Shift+Enter 換行）' : '問 AI…（Enter 送出）'}" ${off && to === 'ag-ai' ? 'disabled' : ''}></textarea><button class="btn pri" data-a="ai-send" ${off && to === 'ag-ai' ? 'disabled' : ''}>送出</button></div>
  </div>`;
}
collab = function(r){
  const s = chatScope(r);
  const dyn = dynamics(s.kind === 'issue' ? {kind:'issue', key:s.key} : {kind:'global'});
  if (UI.collabHide) return `<aside class="collab rail"><button class="btn sm" data-a="collab-toggle" data-v="0" title="展開 AI 對話">💬</button><div class="vlabel">AI 對話 · 動態 ${dyn.count}</div></aside>`;
  const tab = UI.collabTab || 'chat';
  return `<aside class="collab">
    <div class="ch"><button class="ctab ${tab === 'chat' ? 'on' : ''}" data-a="collab-tab" data-v="chat">AI 對話</button><button class="ctab ${tab === 'dyn' ? 'on' : ''}" data-a="collab-tab" data-v="dyn">動態${dyn.count ? ` <span class="badge">${dyn.count}</span>` : ''}</button><span style="flex:1"></span><span class="chip c-dim" title="範圍跟著中間的頁面">${esc(s.label)}</span><button class="btn sm ghost" data-a="collab-toggle" data-v="1" title="收合">▶</button></div>
    ${tab === 'chat' ? chatPanel(s) : `<div class="csec">${dyn.html || '<p class="muted" style="margin:4px 0">目前沒有 agent 動態。</p>'}<a class="dim" href="#/team" style="font-size:12.5px">AI 團隊 →</a></div>`}
  </aside>`;
};

// ---------- 動作 ----------
const _actTeam = actTeam;
actTeam = function(a, el){
  const d = el.dataset;
  switch (a){
    case 'collab-tab': UI.collabTab = d.v; render(); return true;
    case 'tl-range': UI.tl.range = d.v; render(); return true;
    case 'tl-done': UI.tl.showDone = el.checked; render(); return true;
    case 'ms-add': { const n = ($('#ms-name').value || '').trim(), dt = $('#ms-date').value; if (!n || !dt){ toast('名稱與日期都要填', 'bad'); return true; } D.MS[d.p].push({id:'ms-' + Date.now(), name:n, date:dt, status:'open', desc:'', log:[{at:clock(), by:ME, t:'建立'}]}); toast('已新增里程碑', 'ok'); render(); return true; }
    case 'ms-del': { D.MS[d.p] = D.MS[d.p].filter(m => m.id !== d.id); toast('已刪除'); render(); return true; }
  }
  return _actTeam(a, el);
};
document.addEventListener('change', e => {
  const t = e.target;
  if (t.dataset.ms){
    const m = msOf(t.dataset.id); const p = Object.keys(D.MS).find(k => D.MS[k].includes(m));
    if (t.dataset.ms === 'date'){
      m.date = t.value; const s = msStats(m, p);
      m.warn = s.late.length ? `改到 ${m.date} 後，${s.late.map(i => keyLink(i.key) + '（到期 ' + i.due_date.slice(5) + '）').join('、')} 的到期日晚於里程碑。不會自動改這些單的日期；要改請逐張調整或問右欄 AI。` : '';
      toast('已改里程碑日期', 'ok');
    } else if (t.dataset.ms === 'status'){ m.status = t.value; toast(`已改成「${msStatusLabel[t.value]}」`); }
    m.log.push({at:clock(), by:ME, t:t.dataset.ms + ' → ' + t.value});
    render(); return;
  }
  if (t.dataset.a === 'chat-to'){ UI.chatTo = t.value; render(); return; }
});
document.addEventListener('keydown', e => { if (e.target.id === 'ai-in' && e.key === 'Enter' && e.shiftKey) e.stopImmediatePropagation(); }, true);

// 路由
const _teamPage = teamPage;
teamPage = function(r){
  if (r.parts[0] === 'project') return pgProject(r);
  if (r.parts[0] === 'plan') return pgProject({parts:['project', 'AUTH', 'plan']});
  return _teamPage(r);
};

// 內建 AI 預設開啟（使用者要右欄能直接對話；情境控制仍可關閉來看關閉時的樣子）
SIM.ai = true;
const _startFlow2 = startFlow;
startFlow = function(id){ _startFlow2(id); if (!['f-ai-off'].includes(id)){ SIM.ai = true; renderCtl(); render(); } };

FLOWS.unshift(
  {id:'f25', t:'時程與里程碑', ref:'使用者 2026-11-07；U39', steps:['左欄專案 AUTH → 預設是「時程」分頁：時間軸、今天線、里程碑菱形','上方黃框列出 At risk 的里程碑（逾期、到期日晚於里程碑）','「里程碑」分頁：改 v2.0 日期到 11/10 → 列出晚於里程碑的單','新增一個里程碑；有單的不能刪、只能關閉','開 AUTH-123：屬性列有里程碑與開始 → 到期；編輯欄位可改','首頁「里程碑」小工具：倒數與進度'], go:'/project/AUTH/timeline'},
  {id:'f26', t:'右欄直接和 AI 對話', ref:'使用者 2026-11-07；U38', steps:['右欄預設是「AI 對話」，範圍跟著中間頁面（全部／專案／單號）','在 AUTH 時程頁問「里程碑進度與風險」','開 AUTH-140 → 「排進下一個里程碑」→ 核准並套用','對象改成「QA：qa@alice」→ 交辦「檢查 AUTH-110」','對象改成「分流：triage@carol」→ 你不是 sponsor，變成請求','「動態」分頁看 agent 寫入；▶ 收合後點 💬 展開'], go:'/project/AUTH/timeline'},
);

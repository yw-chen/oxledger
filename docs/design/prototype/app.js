// oxledger 原型：SPA 畫面與互動
'use strict';

const esc = s => String(s == null ? '' : s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const $ = s => document.querySelector(s);

const UI = {
  session:{logged:true, authAt:Date.now() - 2 * 3600e3},
  viewAs:'human', sel:new Set(), batch:null, recent:[], toasts:[], modal:null, form:{}, openLock:{},
  wiz:null, conflict:null, apply:{}, palette:null, clicks:0, flow:null, returnTo:null, reminders:{snoozed:{}, off:false},
  searchMore:false, slow:null, admin:null, sessions:[{id:'s1', dev:'Windows · Edge（這台）', at:'今天 12:30', cur:true}, {id:'s2', dev:'iPhone · Safari', at:'11-04 21:10', cur:false}],
};

// ---------- 路由 ----------
function parseHash(){
  const h = decodeURIComponent(location.hash.slice(1)) || '/home';
  const [path, qs] = h.split('?');
  const q = {}; (qs || '').split('&').filter(Boolean).forEach(kv => { const [k, v] = kv.split('='); q[decodeURIComponent(k)] = decodeURIComponent(v || ''); });
  return {parts:path.split('/').filter(Boolean), q, raw:h};
}
function go(h){ if (location.hash === '#' + h) render(); else location.hash = h; }
window.addEventListener('hashchange', () => { UI.searchMore = false; render(); const a = $('.center'); if (a) a.scrollTop = 0; });

// ---------- 小元件 ----------
const av = id => { if (typeof AGENTS !== 'undefined' && AGENTS[id]) return `<span class="av ag" title="${esc(AGENTS[id].handle)}（agent）">AI</span>`; const u = U(id); return u ? `<span class="av" style="background:${u.color}" title="${esc(u.name)}">${esc(u.name[0])}</span>` : ''; };
const keyLink = k => `<a class="key" href="#/issue/${k}">${k}</a>`;
const stChip = s => `<span class="chip ${({'新建':'c-act','處理中':'c-warn','待驗證':'c-prop','已解決':'c-ok','已拒絕':'c-dim'})[s] || 'c-dim'}">${esc(s)}</span>`;
const csStateChip = s => `<span class="chip ${({proposed:'c-prop', approved:'c-act', executing:'c-act', succeeded:'c-ok', failed:'c-bad', rejected:'c-dim', canceled:'c-dim', expired:'c-dim'})[s]}">${({proposed:'待審', approved:'已核准・執行中', executing:'執行中', succeeded:'已套用', failed:'套用失敗', rejected:'已拒絕', canceled:'已取消', expired:'已到期'})[s]}</span>`;
function toast(msg, kind){ const t = {id:Math.random(), msg, kind}; UI.toasts.push(t); renderToasts(); setTimeout(() => { UI.toasts = UI.toasts.filter(x => x !== t); renderToasts(); }, 3800); }
function renderToasts(){ $('#toasts').innerHTML = UI.toasts.map(t => `<div class="toast ${t.kind || ''}">${t.msg}</div>`).join(''); }
const pending = () => D.changesets.filter(c => c.state === 'proposed' || c.state === 'failed' || c.state === 'approved');
const blockedBy = c => { const e = applyCheck(c, c.cur); return e && ['context_unavailable','delegation_unavailable'].includes(e.code) ? e : null; };
const awaiting = () => pending().filter(c => c.state === 'proposed' && !blockedBy(c));
const groupOf = id => D.groups.find(g => g.id === id);
const groupLabel = g => `<span class="agent-label" title="agent 提供的分組名稱，不代表任何核准或系統狀態">${esc(g.label)}</span>`;

function opLines(cs, v, compact){
  const out = [];
  for (const o of v.ops){
    if (o.kind === 'create_issue') out.push(`<div><span class="chip c-prop">建立</span> ${esc(o.project)} · ${esc(o.tracker || 'Bug')}：<b>${esc(o.fields.subject)}</b>${compact ? '' : `<div class="muted" style="margin:4px 0 0 4px;white-space:pre-wrap">${esc(o.fields.description || '')}</div>`}${o.fields.priority ? ` <span class="dim">優先度</span> ${esc(o.fields.priority)}` : ''}${o.fields['cf:severity'] ? ` <span class="dim">Severity</span> ${esc(o.fields['cf:severity'])}` : ''}${o.isPrivate ? ' <span class="chip c-warn">私有</span>' : ''}</div>`);
    if (o.kind === 'update_fields') for (const [f, val] of Object.entries(o.fields)){
      const before = o.before ? o.before[f] : null;
      out.push(`<div class="diff">${compact ? '' : keyLink(o.target) + ' '}<span class="dim">${fieldName(f)}</span> <span class="old">${esc(fmtVal(f, before))}</span><span class="arrow">→</span><span class="new">${esc(fmtVal(f, val))}</span></div>`);
    }
    if (o.kind === 'transition') out.push(`<div class="diff">${compact ? '' : keyLink(o.target) + ' '}<span class="dim">狀態</span> <span class="old">${esc(o.from)}</span><span class="arrow">→</span><span class="new">${esc(o.to)}</span></div>`);
    if (o.kind === 'add_note') out.push(`<div>${compact ? '' : keyLink(o.target) + ' '}<span class="chip c-prop">${o.private ? '私有 note' : 'note'}</span> ${compact ? esc(o.text.slice(0, 40)) + (o.text.length > 40 ? '…' : '') : `<div class="note" style="white-space:pre-wrap">${esc(o.text)}</div>`}</div>`);
    if (o.kind === 'duplicate_of') out.push(`<div>${keyLink(o.source)} <span class="chip c-prop">重複於</span> ${keyLink(o.target)} <span class="dim">（只建立關係，不關單）</span></div>`);
  }
  return out.join('');
}
function csTargetsLabel(cs){ const v = curVer(cs); const c = v.ops.find(o => o.kind === 'create_issue'); if (c) return `<span class="dim">新建於</span> ${c.project}`; const t = opTargets(v); return t.map(k => keyLink(k) + ` <span class="muted">${esc(D.I[k] ? D.I[k].subject : '')}</span>`).join('<br>'); }

// ---------- 頂部 ----------
function topbar(r){
  const n = awaiting().length;
  const on = p => r.parts[0] === p ? 'on' : '';
  const ctx = D.contexts.find(c => c.state === 'active');
  const broken = ctx && !ctxUsable(ctx);
  return `<header class="top">
    <div class="logo">${window.brandMark ? brandMark() : '<i></i>oxledger'}</div>
    <div class="kbar" data-a="palette" role="button" tabindex="0"><span class="t">搜尋紀錄、輸入 AUTH-123 或指令…</span><span class="kbd" style="margin-left:auto">Ctrl K</span></div>
    <div class="me"><a href="#/account" style="text-decoration:none;color:inherit" class="row">${av('u1')}<span class="nm">Alice</span></a></div>
  </header>`;
}

// ---------- 登入 ----------
function pgLogin(){
  const exp = UI.loginReason === 'expired';
  return `<div class="page" style="max-width:420px;padding-top:9vh">
    <div class="logo" style="font-size:22px;margin-bottom:6px">${window.brandMark ? brandMark(true) : '<i></i>oxledger'}</div><p class="muted" style="margin:0 0 14px">${esc(window.brandCfg ? brandCfg().login : '')}</p>
    ${exp ? `<div class="banner w">登入已逾時。登入後會回到原本的頁面，你填到一半的內容還在。</div>` : ''}
    ${UI.loginErr ? `<div class="banner b">帳號或密碼不正確。忘記密碼請聯絡管理員。</div>` : ''}
    <div class="card">
      <label class="f"><span>帳號</span><input type="text" id="lg-u" value="alice" autocomplete="username"></label>
      <label class="f"><span>密碼</span><input type="password" id="lg-p" autocomplete="current-password" placeholder="原型：任何密碼都可；輸入 wrong 看錯誤訊息"></label>
      <button class="btn pri" style="width:100%" data-a="login">登入</button>
      <p class="hint" style="margin:10px 0 0">閒置 12 小時或登入滿 7 天需要重新登入；一般讀取、審閱、核准都不會再問密碼。</p>
    </div></div>`;
}

// ---------- 首頁 ----------
function pgHome(){ return homePage(); }
function lastErrText(c){ const e = c.executions[c.executions.length - 1]; return e && e.error ? errText(e.error.code, e.error) : ''; }

// ---------- 搜尋 ----------
function hlText(s, hl){
  if (!hl.length) return esc(s);
  const arr = [...s]; let out = ''; const marks = hl.slice().sort((a, b) => a[0] - b[0]); let i = 0;
  for (const [st, en, k] of marks){ if (st < i) continue; out += esc(arr.slice(i, st).join('')); out += `<mark class="${k === 'f' ? 'f' : ''}" aria-label="命中">${esc(arr.slice(st, en).join(''))}</mark>`; i = en; }
  return out + esc(arr.slice(i).join(''));
}
function pgIssues(r){
  const q = r.q.q || '';
  const res = search({q, limit:UI.searchMore ? 50 : 8});
  const recent = UI.recent.filter(k => D.I[k] && !D.I[k].indexed);
  const condLabel = c => `${({project:'專案', status:'狀態', tracker:'類型', assignee:'指派', severity:'Severity', priority:'優先度', '估時':'估時'})[c.f] || c.f} ${c.op === ':' ? '＝' : c.op} ${c.v === 'none' ? '未指派' : c.v}`;
  let body;
  if (UI.slow === 'running') body = `<div class="card"><div class="sync"><span class="spin"></span>正在套用欄位條件… <button class="btn sm" data-a="slow-cancel">取消</button></div></div>`;
  else if (UI.slow === 'timeout') body = `<div class="banner w">這組條件太廣，請加上專案或日期範圍。條件沒有被自動縮小。<span class="sp"></span><button class="btn sm" data-a="slow-fix" data-add="project:AUTH">加上 專案＝AUTH</button><button class="btn sm" data-a="slow-fix" data-add="status:新建">加上 狀態＝新建</button></div>`;
  else body = `
    ${recent.map(k => `<div class="card" style="border-color:var(--ok)"><div class="row"><span class="chip c-ok">剛建立</span>${keyLink(k)}<b>${esc(D.I[k].subject)}</b><span class="sync"><span class="spin"></span>搜尋清單同步中，稍後就找得到</span><span style="flex:1"></span><a class="btn sm" href="#/issue/${k}">開啟</a></div><p class="hint" style="margin:4px 0 0">這張卡片來自寫入回執，不是搜尋命中；同步完成前也能直接開啟或引用。</p></div>`).join('')}
    ${res.lagging && !recent.length ? `<div class="sync" style="margin-bottom:8px"><span class="spin"></span>搜尋清單同步中，稍後完成</div>` : ''}
    <div class="card" style="padding:4px 10px">
    ${res.items.length ? res.items.map(x => {
      const is = x.is;
      const why = res.text ? x.ev.map(e => `<div style="margin-top:2px"><span class="dim" style="font-size:12px">${e.field === 'subject' ? '標題' : '描述'}${e.strength === 'fallback' ? '・<span class="c-prop">部分字元相符</span>' : ''}</span>　${hlText(e.text, e.hl)}</div>`).join('')
        : `<div class="dim" style="font-size:12.5px">${res.conds.map(c => `${condLabel(c).split(' ')[0]}：${c.f === '估時' ? is.cf.estimate + ' h' : c.f === 'severity' ? is.cf.severity : c.f === 'assignee' ? uname(is.assignee) : c.f === 'project' ? is.project : c.f === 'status' ? is.status : c.f === 'priority' ? is.priority : ''}`).join('　')}</div>`;
      return `<div style="padding:9px 0;border-bottom:1px solid var(--line)"><div class="row">${keyLink(is.key)}<b>${esc(is.subject)}</b>${stChip(is.status)}<span class="dim" style="font-size:12.5px">${uname(is.assignee)} · ${esc(is.priority)}</span>${x.strength === 'fallback' ? '<span class="chip c-prop" title="只有部分單字相符，排序較後">部分字元相符</span>' : ''}</div>${why}</div>`;
    }).join('') : `<p class="muted">在你看得到的範圍內沒有符合的紀錄。</p>`}
    </div>
    <div class="row" style="margin-top:8px"><span class="dim">已載入 ${res.items.length} 筆${res.more ? '，還有更多' : ''}</span>${res.more ? '<button class="btn sm" data-a="more">載入更多</button>' : ''}<span class="hint">不顯示總數（Q-S6）；排序可能受你看不到的文件影響，但不會透露內容（Q-S1）</span></div>`;
  return `<div class="page">
    <h1>議題</h1>
    <div class="row" style="margin:8px 0 6px"><input type="search" id="sq" value="${esc(q)}" placeholder="文字，或 project:AUTH status:新建 severity:High 估時>8" style="flex:1"><button class="btn pri" data-a="search">搜尋</button><a class="btn" href="#/new?project=AUTH">回報問題</a></div>
    <div class="row" style="margin-bottom:12px"><span class="dim" style="font-size:12.5px">常用：</span>
      <a class="btn sm" href="#/issues?q=project:AUTH%20status:open">AUTH 未解決</a>
      <a class="btn sm" href="#/issues?q=assignee:alice">指派給我</a>
      <a class="btn sm" href="#/issues?q=project:AUTH%20status:新建%20assignee:none">本週待分流</a>
      <a class="btn sm" href="#/issues?q=severity:High%20估時>8">Severity High 且估時>8</a>
      <a class="btn sm" href="#/issues?q=Safari%20登入">Safari 登入</a>
      <a class="btn sm" href="#/issues?q=登出逾時">登出逾時（部分字元）</a>
      ${SIM.fieldMigrating ? '<span class="chip c-warn" title="估時 欄位轉換中：暫時不能篩選或排序">估時：維護中</span>' : ''}
    </div>
    ${res.conds.length ? `<div class="row" style="margin-bottom:8px"><span class="dim" style="font-size:12.5px">條件</span>${res.conds.map(c => `<span class="chip c-act">${esc(condLabel(c))}</span>`).join('')}</div>` : ''}
    ${body}
  </div>`;
}

// ---------- 單張議題 ----------
const pcells = h => h.replace(/<dt>/g, '<div class="pc"><span class="k">').replace(/<\/dt>\s*<dd>/g, '</span><div>').replace(/<\/dd>/g, '</div></div>');
function pgIssue(key){
  const is = D.I[key];
  if (!is) return `<div class="page"><h1>找不到 ${esc(key)}</h1><p class="muted">這張單不存在，或你沒有權限查看。</p></div>`;
  if (UI.openLock[key] == null) UI.openLock[key] = is.lock;
  const agent = UI.viewAs === 'agent';
  const ctx = activeCtx();
  const agentCan = ctx && ctx.delegations.some(d => d.project === is.project && d.state === 'active') && !is.isPrivate;
  const ed = UI.form.edit === key;
  const notes = is.notes.filter(n => !agent || !n.private);
  const rel = D.changesets.filter(c => ['proposed','failed','approved'].includes(c.state) && opTargets(curVer(c)).includes(key));
  const trans = WORKFLOW[is.status] || [];
  const stale = UI.openLock[key] !== is.lock;
  const fv = (f, val) => ed ? '' : esc(fmtVal(f, val));
  const cfRows = CF_DEFS.map(d => {
    if (agent && d.restricted) return '';
    const v = is.cf[d.key];
    if (ed && !(d.key === 'estimate' && SIM.fieldMigrating)) return `<dt>${d.name}${d.restricted ? ' <span class="chip c-warn" title="受限欄位：只有 Manager 可讀">受限</span>' : ''}</dt><dd>${d.type === 'list' ? `<select data-f="cf:${d.key}"><option value="">（空）</option>${d.options.map(o => `<option ${o === (UI.form['cf:' + d.key] ?? v) ? 'selected' : ''}>${o}</option>`).join('')}</select>` : `<input type="text" data-f="cf:${d.key}" value="${esc(UI.form['cf:' + d.key] ?? (v ?? ''))}">`}</dd>`;
    return `<dt>${d.name}${d.restricted ? ' <span class="chip c-warn" title="受限欄位：只有 Manager 可讀；agent 讀不到">受限</span>' : ''}</dt><dd>${d.key === 'estimate' && SIM.fieldMigrating ? '<span class="dim">暫時無法顯示（欄位維護中）</span>' : v == null ? '<span class="dim">—</span>' : esc(fmtVal('cf:' + d.key, v))}</dd>`;
  }).join('');
  return `<div class="page">
    ${agent ? `<div class="banner p">你正在用「agent 視角」看這張單：${agentCan ? `${esc(ctx.clientName)} 透過 MCP 只讀得到下面這些（私有 note、受限欄位都不在）。` : '這個 agent 沒有這個專案的有效委派，讀不到這張單。'}<span class="sp"></span><button class="btn sm" data-a="viewas" data-v="human">回到我的視角</button></div>` : ''}
    ${stale && !UI.form.conflictMsg ? `<div class="banner i">這張單在你開啟後有更新（第 ${UI.openLock[key]} → ${is.lock} 版）。<span class="sp"></span><button class="btn sm" data-a="reload-issue" data-k="${key}">重新載入</button></div>` : ''}
    ${UI.form.conflictMsg ? `<div class="banner w">${UI.form.conflictMsg}<span class="sp"></span><button class="btn sm" data-a="reload-keep" data-k="${key}">重新載入並保留我的輸入</button></div>` : ''}
    ${agent && !agentCan ? '' : `
    <div class="row"><span class="key" style="font-size:15px">${key}</span>${stChip(is.status)}${is.isPrivate ? '<span class="chip c-warn">私有</span>' : ''}<span class="dim">${esc(D.P[is.project].name)} · ${esc(is.tracker)} · 第 ${is.lock} 版</span>
      <span style="flex:1"></span>
      ${agent ? '' : `<button class="btn sm" data-a="viewas" data-v="agent" title="看 agent 透過 MCP 讀得到什麼">agent 視角</button>`}
    </div>
    ${ed ? `<input type="text" data-f="subject" value="${esc(UI.form.subject ?? is.subject)}" style="font-size:18px;font-weight:700;margin:8px 0">` : `<h1 style="margin:6px 0 10px">${esc(is.subject)}</h1>`}
    <div class="propstrip">${pcells(`
          <dt>指派</dt><dd>${ed ? `<select data-f="assignee"><option value="">（未指派）</option>${D.P[is.project].assignable.map(u => `<option value="${u}" ${(UI.form.assignee ?? is.assignee) === u ? 'selected' : ''}>${esc(uname(u))}</option>`).join('')}</select>` : `<span class="row" style="gap:6px">${av(is.assignee)}${esc(uname(is.assignee))}</span>`}</dd>
          <dt>優先度</dt><dd>${ed ? `<select data-f="priority">${PRIORITIES.map(p => `<option ${(UI.form.priority ?? is.priority) === p ? 'selected' : ''}>${p}</option>`).join('')}</select>` : esc(is.priority)}</dd>
          ${cfRows}
          ${is.duplicateOf ? `<dt>關係</dt><dd>重複於 ${keyLink(is.duplicateOf)}</dd>` : ''}
          ${is.duplicates && is.duplicates.length ? `<dt>關係</dt><dd>${is.duplicates.map(keyLink).join('、')} 是本單的重複</dd>` : ''}
          ${window.msCells ? msCells(is, ed) : ''}
          <dt>建立</dt><dd>${esc(is.created)} · ${actorHtml(is.author)}${is.onBehalf ? `<div class="hint">代表 ${esc(uname(is.onBehalf))}；own 權限算 ${esc(uname(is.onBehalf))} 的單</div>` : ''}</dd>`)}</div>
    ${window.handoffCard ? handoffCard(key) : ''}
    ${agent ? '' : `<div class="actbar">${ed ? '' : `<button class="btn" data-a="edit" data-k="${key}">編輯欄位</button>`}<span class="dim" style="font-size:12.5px">轉換狀態</span>${trans.map(t => `<button class="btn sm" data-a="transition" data-k="${key}" data-to="${t}">→ ${t}</button>`).join('')}<span class="dim" style="font-size:12.5px;margin-left:8px">關係</span><select id="dup-t" style="width:auto">${Object.keys(D.I).filter(k => k !== key && k.startsWith(is.project + '-')).map(k => `<option>${k}</option>`).join('')}</select><button class="btn sm" data-a="dup" data-k="${key}" title="兩端都要能編輯、限同專案；不會自動關單（U2）">標為重複於…</button></div>`}
      <div class="card"><div class="k">描述</div>${ed ? `<textarea data-f="description">${esc(UI.form.description ?? is.description)}</textarea>` : `<p style="white-space:pre-wrap;margin:6px 0 0">${esc(is.description)}</p>`}</div>
      ${ed ? `<div class="row" style="margin-bottom:12px"><button class="btn pri" data-a="save-edit" data-k="${key}">儲存</button><button class="btn" data-a="cancel-edit">取消</button><span class="hint">直接操作（human_direct）：和 agent 提案走同一套驗證，但不用核准</span></div>` : ''}
      <h2>Notes</h2>
      ${notes.map(n => `<div class="note ${n.private ? 'priv' : ''}"><div class="h">${av(n.author)} ${actorHtml(n.author, n.approver)} · ${esc(n.at)} ${n.private ? '<span class="chip c-warn">私有</span>' : ''}</div>${n.retracted ? `<div class="dim">已由 ${esc(uname(n.retracted.by))} 撤回（${esc(n.retracted.at)}）。</div>` : `<div style="white-space:pre-wrap">${esc(n.text)}</div>`}</div>`).join('') || '<p class="muted">還沒有 note。</p>'}
      ${agent ? '' : `<div class="card"><textarea id="note-tx" placeholder="新增 note…">${esc(UI.form.note || '')}</textarea><div class="row" style="margin-top:6px"><label class="row" style="gap:4px;font-size:13px"><input type="checkbox" id="note-priv"> 私有 note</label><span style="flex:1"></span><button class="btn pri" data-a="add-note" data-k="${key}">新增 note</button></div></div>`}
      <div class="row"><h2 style="margin-right:auto">歷史</h2>${['all:全部','human:只看人','agent:只看 agent'].map(x => { const [k, l] = x.split(':'); return `<button class="btn sm ${(UI.evF || 'all') === k ? 'pri' : ''}" data-a="ev-f" data-v="${k}">${l}</button>`; }).join('')}</div>
      ${is.events.slice().reverse().filter(e => (UI.evF || 'all') === 'all' || (UI.evF === 'agent') === !!AGENTS[e.actor]).map(e => `<div class="ev">${esc(e.at)} · ${actorHtml(e.actor, e.approver)} · ${esc(e.text)}</div>`).join('')}
`}
  </div>`;
}

// ---------- 建立議題（human_direct） ----------
function pgNew(r){
  const p = UI.form.np || r.q.project || 'AUTH';
  const f = UI.form;
  const sim = f.ns ? search({q:f.ns, project:p, limit:4}) : null;
  return `<div class="page" style="max-width:820px">
    <h1>回報問題</h1><p class="sub">欄位依 ${esc(p)} · Bug 的 schema 顯示（describe）。必填欄位有 <em class="c-bad">*</em>。</p>
    ${f.newErr ? `<div class="banner b">${f.newErr}</div>` : ''}
    <div class="card">
      <div class="row" style="margin-bottom:10px"><label class="f" style="flex:1"><span>專案</span><select data-f="np">${['AUTH','WEB','OPS'].map(x => `<option ${x === p ? 'selected' : ''}>${x}</option>`).join('')}</select></label><label class="f" style="flex:1"><span>類型</span><select><option>Bug</option><option>Feature</option></select></label></div>
      <label class="f"><span>標題 <em>*</em></span><input type="text" data-f="nsub" value="${esc(f.nsub || '')}" placeholder="一句話說明發生什麼事"></label>
      <div class="row" style="margin:-4px 0 10px"><button class="btn sm" data-a="find-sim">送出前先找相似的單</button>${sim ? `<span class="dim" style="font-size:12.5px">${sim.items.length ? '可能相關：' + sim.items.map(x => keyLink(x.is.key) + ' ' + esc(x.is.subject)).join('、') : '沒有找到相似的單'}</span>` : ''}</div>
      <label class="f"><span>描述</span><textarea data-f="ndesc" placeholder="重現步驟、預期與實際結果">${esc(f.ndesc || '')}</textarea></label>
      <div class="row"><label class="f" style="flex:1"><span>優先度</span><select data-f="npri">${PRIORITIES.map(x => `<option ${x === (f.npri || 'Normal') ? 'selected' : ''}>${x}</option>`).join('')}</select></label>
      <label class="f" style="flex:1"><span>Severity</span><select data-f="nsev"><option value="">（空）</option>${SEVERITIES.map(x => `<option ${x === f.nsev ? 'selected' : ''}>${x}</option>`).join('')}</select></label>
      <label class="f" style="flex:1"><span>指派</span><select data-f="nasg"><option value="">（未指派）</option>${D.P[p].assignable.map(u => `<option value="${u}" ${f.nasg === u ? 'selected' : ''}>${esc(uname(u))}</option>`).join('')}</select></label></div>
      <label class="row" style="gap:6px;font-size:13px"><input type="checkbox" data-f="npriv" ${f.npriv ? 'checked' : ''}> 私有議題（只有作者、被指派者與有權限的角色看得到）</label>
      <div class="row" style="margin-top:12px"><button class="btn pri" data-a="create">建立</button><a class="btn" href="#/issues">取消</a></div>
    </div></div>`;
}

// ---------- 待審佇列 ----------
function pgReview(r){
  const gid = r.q.group || '';
  const tab = r.q.tab || 'mine';
  const b = UI.batch;
  let list = tab === 'done' ? D.changesets.filter(c => ['succeeded','rejected','canceled','expired'].includes(c.state)) : D.changesets.filter(c => pending().includes(c) || (b && b.res[c.id]));
  if (gid) list = list.filter(c => c.group === gid);
  const g = gid ? groupOf(gid) : null;
  const selectable = c => reviewModes(c).modes.includes('queue');
  const selKeys = {}; let dupSel = false;
  [...UI.sel].forEach(id => { const c = D.changesets.find(x => x.id === id); if (!c) return; opTargets(curVer(c)).forEach(k => { if (selKeys[k]) dupSel = true; selKeys[k] = 1; }); });
  const rows = list.map(c => {
    const rm = reviewModes(c); const val = validate(c); const v = curVer(c);
    const res = b && b.res[c.id];
    const ev = v.evidence.length ? v.evidence.map(e => `<div style="font-size:12.5px">${keyLink(e.key)} <span class="${e.status === 'current' ? 'dim' : 'c-warn'}">「${esc(e.quote.slice(0, 18))}…」${e.status === 'current' ? '' : ' 原文已修改'}</span></div>`).join('') : '<span class="dim" style="font-size:12.5px">—</span>';
    const live = ['proposed','failed'].includes(c.state);
    const blk = live && blockedBy(c);
    const check = !live ? '<span class="dim">—</span>' : val.status === 'ok' ? `<span class="c-ok">目前檢查通過</span>` : val.status === 'conflict' ? `<span class="c-warn">有衝突</span>` : `<span class="c-bad">檢查未通過</span>`;
    const resTx = !res ? '' : res.s === 'ok' ? `<span class="res c-ok">✓ 已套用</span>` : res.s === 'sending' ? `<span class="res sync"><span class="spin"></span>送出中</span>` : res.s === 'pending' ? `<span class="res c-warn">確認中</span>` : res.s === 'stopped' ? `<span class="res c-dim">未送出</span>` : `<span class="res c-bad">✗ ${esc(errText(res.code))}</span>`;
    return `<tr class="${UI.sel.has(c.id) ? 'sel' : ''}">
      <td style="width:28px">${tab === 'mine' ? `<input type="checkbox" data-a="sel" data-id="${c.id}" ${UI.sel.has(c.id) ? 'checked' : ''} ${selectable(c) && !b ? '' : 'disabled'} title="${selectable(c) ? '可以在佇列核准' : '需要打開單份審閱：' + esc(rm.why.join('、'))}" aria-label="選取提案 #${c.no}">` : ''}</td>
      <td><a href="#/cs/${c.id}">#${c.no}</a> ${c.group && !gid ? groupLabel(groupOf(c.group)) : ''}<div style="margin-top:2px">${csTargetsLabel(c)}</div></td>
      <td>${opLines(c, v, true)}</td>
      <td>${ev}</td>
      <td style="font-size:12.5px">${check}${live ? `<div class="dim">${val.checked_at}</div>` : ''}${!live || selectable(c) ? '' : `<div class="dim">需逐份打開：${esc(rm.why.join('、'))}</div>`}</td>
      <td>${tab === 'mine' ? (resTx || (blk ? `<span class="chip c-dim" title="${esc(errText(blk.code, blk))}">無法套用</span>` : csStateChip(c.state))) : csStateChip(c.state)}</td>
    </tr>`;
  }).join('');
  const nSel = UI.sel.size;
  const summary = b ? (() => { const vals = Object.values(b.res); const ok = vals.filter(x => x.s === 'ok').length, bad = vals.filter(x => x.s === 'bad').length, pd = vals.filter(x => x.s === 'pending' || x.s === 'sending').length, st = vals.filter(x => x.s === 'stopped').length; return `已完成 ${ok}／${vals.length}${bad ? `；${bad} 份失敗` : ''}${pd ? `；${pd} 份確認中` : ''}${st ? `；${st} 份未送出` : ''}`; })() : '';
  return `<div class="page">
    <h1>待審</h1><p class="sub">agent 只能提案；每一份都要你核准才會寫入。短欄位與單次狀態轉換的提案可以勾選一起核准，各自套用、各自成功或失敗。</p>
    <div class="row" style="margin-bottom:10px"><a class="btn sm ${tab === 'mine' ? 'pri' : ''}" href="#/review${gid ? '?group=' + gid : ''}">等我審 ${awaiting().length}</a><a class="btn sm ${tab === 'done' ? 'pri' : ''}" href="#/review?tab=done${gid ? '&group=' + gid : ''}">已處理</a>
      ${g ? `<span class="dim">分組：</span>${groupLabel(g)} <a class="btn sm ghost" href="#/review">顯示全部</a>` : ''}</div>
    ${g ? `<p class="hint">分組名稱是 agent 提供的導覽文字，不代表核准或系統狀態；選取整組不等於核准整組。</p>` : ''}
    ${b ? `<div class="banner ${b.stopped ? 'w' : b.done ? 'o' : 'i'}">${b.done ? '' : '<span class="spin"></span>'}${summary}${b.stopped ? '。' + esc(b.stopped) : ''}<span class="sp"></span>${b.done ? '<button class="btn sm" data-a="batch-clear">完成</button>' : '<button class="btn sm" data-a="batch-stop">停止（只影響尚未送出的）</button>'}</div>` : ''}
    ${tab === 'mine' && !b ? `<div class="row" style="margin-bottom:8px"><button class="btn sm" data-a="sel-page">選取本頁符合的</button><button class="btn sm ghost" data-a="sel-none">清除選取</button><span style="flex:1"></span>${dupSel ? '<span class="c-warn" style="font-size:12.5px">同一次選取不能有兩份改同一張議題</span>' : ''}<button class="btn prop" data-a="batch" ${nSel && !dupSel ? '' : 'disabled'}>核准並套用所選（${nSel} 份）</button></div>` : ''}
    <div class="card" style="padding:4px 8px;overflow:auto"><table class="t"><tr><th></th><th>提案／目標</th><th>變更</th><th>根據</th><th>檢查</th><th>狀態</th></tr>${rows || `<tr><td colspan="6" class="muted">沒有提案。</td></tr>`}</table></div>
    ${b && b.done ? `<div class="row">${Object.entries(b.res).filter(([, x]) => x.s === 'ok').map(([id]) => { const c = D.changesets.find(x => x.id === id); const e = c.executions[c.executions.length - 1]; return (e.effects || []).map(f => f.version_id ? `<a class="btn sm" href="#/project/${f.project}/milestones">開啟里程碑</a>` : `<a class="btn sm" href="#/issue/${f.key}">開啟 ${f.key}</a>`).join(''); }).join('')}</div>` : ''}
  </div>`;
}

// ---------- 單份提案 ----------
function errText(code, e){
  return ({
    conflict:'這張單在提案後被改過，尚未套用任何變更',
    precondition_failed:'這張單在提案後被改過，尚未套用任何變更',
    version_superseded:'agent 已更新這份提案，你看的是舊版本',
    delegation_unavailable:'這份提案的委派已失效（到期／已撤銷／專案不再符合條件），無法套用',
    context_unavailable:'這個 agent 的工作脈絡已停用' + (e && e.details && e.details.project ? `（${e.details.project} 的權限設定變更）` : e && e.details && e.details.closed ? '（已建立新工作脈絡或已停用）' : '') + '，它的提案都不能套用',
    proposal_unavailable:'提案內容目前無法完整存取，不能套用',
    changeset_expired:'提案已在 11/14 14:32 到期',
    invalid_transition:e && e.msg ? e.msg : '目前不能轉到這個狀態',
    review_outdated:'預覽之後資料又變了，請再確認一次',
    execution_in_progress:'結果確認中，請勿重複按；會自動更新',
    temporarily_unavailable:'暫時無法連線，尚未確認是否完成',
    audience_expansion_forbidden:'這份提案會把內容寫到讀者更多的地方，不能套用',
    already_applied:'這份提案已經套用過',
    changeset_not_executable:'這份提案已結束，不能再套用',
    invalid_value:'有欄位的值不合法',
    review_mode_not_allowed:'這份提案要打開單份審閱',
    unauthenticated:'登入已逾時',
    agent_suspended:'這個 agent 的寫入已暫停，沒有改到任何資料；讀取照常',
    forbidden_op:'這個操作在委派設定裡是「不允許」，沒有改到任何資料',
    undo_not_applicable:'這些欄位在 agent 寫入後又被改過，回復不會覆蓋別人的決定；沒有改到資料',
    rate_limited:'agent 寫入太快，稍後自動重試；沒有改到資料',
  })[code] || code;
}
function pgCs(id, r){
  const cs = D.changesets.find(c => c.id === id);
  if (!cs) return `<div class="page"><h1>找不到提案</h1><p class="muted">提案不存在，或你沒有權限查看。</p><a class="btn" href="#/review">回待審清單</a></div>`;
  const askV = r.q.version ? +r.q.version : cs.cur;
  const v = cs.versions[askV - 1] || curVer(cs);
  const isCur = v.v === cs.cur;
  const val = validate(cs);
  const ctx = D.contexts.find(c => c.id === cs.ctx);
  const ap = UI.apply[cs.id] || {};
  const last = cs.executions[cs.executions.length - 1];
  const g = cs.group ? groupOf(cs.group) : null;
  const pre = applyCheck(cs, cs.cur);
  const blockBanner = pre && ['context_unavailable','delegation_unavailable'].includes(pre.code) && cs.state === 'proposed'
    ? `<div class="banner b">${esc(errText(pre.code, pre))}<span class="sp"></span>${pre.code === 'context_unavailable' ? '<a class="btn sm" href="#/agents">建立新工作脈絡</a>' : '<a class="btn sm" href="#/agents">管理委派</a>'}</div>` : '';
  const chk = (n, s) => `<div class="row" style="gap:6px"><span class="${s === 'ok' ? 'c-ok' : s === 'conflict' ? 'c-warn' : 'c-bad'}">${s === 'ok' ? '✓' : s === 'conflict' ? '!' : '✗'}</span>${n}${s === 'conflict' ? '：資料在提案後有變更' : ''}</div>`;
  let resultBox = '';
  if (ap.s === 'sending') resultBox = `<div class="banner i"><span class="spin"></span>送出中…</div>`;
  else if (ap.s === 'pending') resultBox = `<div class="banner w"><span class="spin"></span>結果確認中，請勿重複按；會自動更新。</div>`;
  else if (ap.s === 'unknown') resultBox = `<div class="banner w">伺服器沒有收到完整的核准請求，這份提案<b>尚未套用</b>。<span class="sp"></span><button class="btn sm" data-a="apply-resend" data-id="${cs.id}">用原請求重送</button></div><p class="hint">重送沿用原本的 operation_id（${esc(ap.op)}），不會套用兩次。</p>`;
  else if (cs.state === 'succeeded') resultBox = `<div class="banner o ripple">✓ 已套用（${esc(last.at)}）：${(last.effects || []).map(e => `${e.version_id ? `里程碑「${esc(e.key)}」` : keyLink(e.key)} ${e.kind === 'created' ? '已建立' : '已更新'}`).join('、')}<span class="sp"></span>${(last.effects || []).map(e => e.version_id ? `<a class="btn sm" href="#/project/${e.project}/milestones">開啟里程碑</a>` : `<a class="btn sm" href="#/issue/${e.key}">開啟 ${e.key}</a>`).join('')}</div>${(last.effects || []).some(e => D.I[e.key] && !D.I[e.key].indexed) ? '<div class="sync"><span class="spin"></span>搜尋清單同步中，稍後就找得到；議題本身現在就能開啟</div>' : ''}`;
  else if (cs.state === 'failed' && last && last.error){
    const c = last.error.code;
    resultBox = `<div class="banner b">套用失敗：${esc(errText(c, last.error.details || last.error))}<span class="sp"></span>${c === 'conflict' ? `<a class="btn sm" href="#/cs/${cs.id}/conflict">檢視差異</a>` : c === 'context_unavailable' ? '<a class="btn sm" href="#/agents">建立新工作脈絡</a>' : c === 'delegation_unavailable' ? '<a class="btn sm" href="#/agents">管理委派</a>' : ''}</div>`;
  }
  const canAct = isCur && ['proposed','failed'].includes(cs.state) && !['sending','pending','unknown'].includes(ap.s);
  return `<div class="page">
    <div class="row"><a href="#/review${g ? '?group=' + g.id : ''}" class="dim" style="text-decoration:none">← 待審</a></div>
    ${!isCur ? `<div class="banner w">agent 已更新這份提案，你看的是第 ${v.v} 版（目前是第 ${cs.cur} 版）。<span class="sp"></span><a class="btn sm" href="#/cs/${cs.id}">看最新版本</a></div>` : ''}
    ${blockBanner}
    <div class="row" style="margin:6px 0"><h1 style="margin:0">提案 #${cs.no}</h1>${csStateChip(cs.state)}<span class="dim mono">第 ${v.v} 版 · ${esc(v.hash)}</span></div>
    <div class="row" style="margin-bottom:12px"><span class="agent-label" title="agent 寫的標題">${esc(cs.title)}</span>${g ? groupLabel(g) : ''}<span class="dim">由 ${esc(ctx.clientName)} 提出 · ${esc(v.by === 'owner' ? '你修訂的版本' : 'agent 版本')} · 有效到 ${esc(cs.expires)}</span></div>
    ${resultBox}
    <div class="grid2"><div>
      <div class="card p"><div class="k" style="margin-bottom:6px">只會做這些變更</div>${opLines(cs, v, false)}</div>
      <div class="card"><div class="k" style="margin-bottom:6px">根據（agent 引用的原文；審閱時重新驗證）</div>
        ${v.evidence.length ? v.evidence.map(e => `<div style="margin:6px 0">${keyLink(e.key)} <span class="dim">${e.field === 'subject' ? '標題' : '描述'}</span> ${e.status === 'current' ? '<span class="chip c-ok">原文仍相同</span>' : `<span class="chip c-warn">原文已修改</span> <a href="#/issue/${e.key}">查看目前內容</a>`}<div class="note">「${esc(e.quote)}」</div></div>`).join('') : '<span class="muted">這份提案沒有附引用。</span>'}
      </div>
      ${cs.executions.length ? `<div class="card"><div class="k" style="margin-bottom:6px">執行紀錄</div>${cs.executions.map(e => `<div class="ev">第 ${e.attempt} 次 · ${esc(e.at)} · <span class="mono">${esc(e.op)}</span> · ${e.status === 'succeeded' ? '<span class="c-ok">成功</span>' : e.status === 'failed' ? `<span class="c-bad">失敗：${esc(e.error.code)}</span>` : '<span class="c-warn">尚未開始（queued）</span>'}</div>`).join('')}</div>` : ''}
      ${cs.versions.length > 1 ? `<div class="card"><div class="k" style="margin-bottom:6px">版本</div>${cs.versions.map(x => `<div class="ev"><a href="#/cs/${cs.id}?version=${x.v}">第 ${x.v} 版</a> · ${x.by === 'owner' ? '你修訂' : 'agent'} · ${esc(x.at)} ${x.v === cs.cur ? '<span class="chip c-act">目前</span>' : ''}</div>`).join('')}</div>` : ''}
    </div><div>
      ${['proposed','failed'].includes(cs.state) ? '' : '<!--'}<div class="card"><div class="k" style="margin-bottom:6px">${val.status === 'ok' ? '<span class="c-ok">目前檢查通過</span>' : val.status === 'conflict' ? '<span class="c-warn">有衝突</span>' : '<span class="c-bad">檢查未通過</span>'}</div>
        ${chk('結構與合法值', val.checks.values)}${chk('權限', val.checks.permission)}${chk('工作流程', val.checks.workflow)}${chk('版本前置條件', val.checks.precondition)}${chk('受眾（不寫到讀者更多的地方）', val.checks.audience)}
        ${val.blocking.map(x => `<div class="c-bad" style="font-size:12.5px;margin-top:4px">${esc(x.msg || x.code)}</div>`).join('')}
        ${val.warnings.map(x => `<div class="c-warn" style="font-size:12.5px;margin-top:4px">警告：${esc(x.msg)}</div>`).join('')}
        <p class="hint" style="margin:8px 0 0">依 ${val.checked_at} 的資料與權限檢查；套用時會再次確認。</p>
      </div>${['proposed','failed'].includes(cs.state) ? '' : '-->'}
      ${canAct ? `<div class="card">
        ${val.status === 'conflict' ? `<a class="btn pri" style="display:block;text-align:center" href="#/cs/${cs.id}/conflict">檢視差異並決定</a>` : `<button class="btn pri" style="width:100%" data-a="apply" data-id="${cs.id}" ${val.status === 'ok' || cs.state === 'failed' ? '' : 'disabled'}>核准並套用</button>`}
        <div class="row" style="margin-top:8px"><button class="btn sm bad" data-a="reject" data-id="${cs.id}">拒絕</button><button class="btn sm" data-a="cancel-cs" data-id="${cs.id}">取消提案</button></div>
        <p class="hint" style="margin:8px 0 0">不會再跳「你確定嗎」：上面的變更清單就是確認內容。拒絕是終態，agent 要改得開新提案（U10）。</p>
      </div>` : ''}
      <div class="card"><div class="k">來源專案快照</div><p style="margin:4px 0 0;font-size:13px">${v.src.map(p => `<span class="chip c-act">${p}</span>`).join(' ')} <span class="hint">寫入目的地的讀者必須 ⊆ 這些專案的讀者</span></p></div>
    </div></div>
  </div>`;
}

// ---------- 衝突 ----------
function pgConflict(id){
  const cs = D.changesets.find(c => c.id === id);
  if (!cs) return '';
  const v = curVer(cs);
  const val = validate(cs);
  const C = UI.conflict && UI.conflict.id === id ? UI.conflict : (UI.conflict = {id, choices:{}, previewAt:null, previewLocks:null, outdated:false});
  const rows = []; const changedOther = [];
  for (const k of val.conflicts){
    const is = D.I[k];
    const fieldsTouched = [];
    for (const o of v.ops.filter(o => o.target === k)){
      if (o.kind === 'update_fields') for (const [f, nv] of Object.entries(o.fields)){
        const orig = o.before ? o.before[f] : null, cur = getField(is, f);
        fieldsTouched.push(f);
        const ch = (C.choices[k] || {})[f] || 'proposal';
        rows.push(`<div class="h">${keyLink(k)} ${fieldName(f)}</div><div>${esc(fmtVal(f, orig))}</div><div class="${cur !== orig ? 'chg' : ''}">${esc(fmtVal(f, cur))}${cur !== orig ? ' <span class="chip c-warn">已變</span>' : ''}</div><div class="c-prop">${esc(fmtVal(f, nv))}</div><div><label><input type="radio" name="c-${k}-${f}" data-a="cchoice" data-k="${k}" data-f="${f}" data-v="current" ${ch === 'current' ? 'checked' : ''}> 用現在的值</label><br><label><input type="radio" name="c-${k}-${f}" data-a="cchoice" data-k="${k}" data-f="${f}" data-v="proposal" ${ch === 'proposal' ? 'checked' : ''}> 用提案的值</label></div>`);
      }
      if (o.kind === 'transition'){
        const legal = (WORKFLOW[is.status] || []).includes(o.to);
        const ch = (C.choices[k] || {}).status || (legal ? 'proposal' : 'current');
        rows.push(`<div class="h">${keyLink(k)} 狀態</div><div>${esc(o.from)}</div><div class="${is.status !== o.from ? 'chg' : ''}">${esc(is.status)}</div><div class="c-prop">${esc(o.to)}${legal ? '' : ' <span class="chip c-bad">目前不能轉</span>'}</div><div><label><input type="radio" name="c-${k}-st" data-a="cchoice" data-k="${k}" data-f="status" data-v="current" ${ch === 'current' ? 'checked' : ''}> 維持現在</label><br><label><input type="radio" name="c-${k}-st" data-a="cchoice" data-k="${k}" data-f="status" data-v="proposal" ${ch === 'proposal' ? 'checked' : ''} ${legal ? '' : 'disabled'}> 轉到 ${esc(o.to)}</label>${legal ? '' : `<div class="hint">可以轉到：${(WORKFLOW[is.status] || []).join('、')}</div>`}</div>`);
      }
    }
    const lastEvs = is.events.slice((v.evAt || {})[k] ?? is.events.length - 3);
    lastEvs.forEach(e => changedOther.push(`${keyLink(k)} ${esc(e.at)} ${esc(uname(e.actor))}：${esc(e.text)}`));
    is._touchedChanged = fieldsTouched.some(f => getField(is, f) !== (v.ops.find(o => o.target === k && o.kind === 'update_fields') || {before:{}}).before[f]);
  }
  const prevHtml = previewOps(cs, C);
  const onlyOther = val.conflicts.every(k => !D.I[k]._touchedChanged) && !v.ops.some(o => o.kind === 'transition' && val.conflicts.includes(o.target) && D.I[o.target].status !== o.from);
  return `<div class="page">
    <div class="row"><a href="#/cs/${cs.id}" class="dim" style="text-decoration:none">← 提案 #${cs.no}</a></div>
    <h1>這張單在提案後被改過，尚未套用任何變更</h1>
    <p class="sub">逐欄決定要用哪個值；按下後會建立修訂版並立刻核准套用，這是一個動作。${onlyOther ? '<b class="c-ok">你提案的欄位都沒變</b>，只是有別的內容更新，預設全用提案的值。' : ''}</p>
    ${C.outdated ? `<div class="banner w">預覽之後資料又變了，請再確認一次。沒有核准、也沒有建立新版本。<span class="sp"></span><button class="btn sm" data-a="repreview" data-id="${id}">重新預覽</button></div>` : ''}
    <div class="cols3"><div class="h">欄位</div><div class="h">原本（提案時）</div><div class="h">現在</div><div class="h">提案</div><div class="h">你的選擇</div>${rows.join('')}</div>
    ${changedOther.length ? `<div class="card" style="margin-top:12px"><div class="k" style="margin-bottom:4px">提案之後發生的變更（只列你看得到的）</div>${changedOther.map(x => `<div class="ev">${x}</div>`).join('')}</div>` : ''}
    <div class="card p" style="margin-top:12px"><div class="k" style="margin-bottom:4px">修訂後的完整內容（伺服器算的預覽 · ${C.previewAt}）</div>${prevHtml}</div>
    <div class="row"><button class="btn pri" data-a="revise-apply" data-id="${id}">建立修訂版並核准套用</button><button class="btn" data-a="revise-only" data-id="${id}">只儲存修訂版</button><a class="btn ghost" href="#/cs/${cs.id}">返回</a></div>
    <p class="hint">修訂版仍受原本的委派範圍、期限與受眾限制。按下時伺服器會用預覽當下的版本再比一次；不符就回「預覽之後資料又變了」。</p>
  </div>`;
}
function previewOps(cs, C){
  if (!C.previewAt){ C.previewAt = clock(); C.previewLocks = {}; validate(cs).conflicts.forEach(k => C.previewLocks[k] = D.I[k].lock); }
  const v = curVer(cs);
  const lines = [];
  for (const o of v.ops){
    if (o.kind === 'update_fields'){ for (const [f, nv] of Object.entries(o.fields)){ const ch = (C.choices[o.target] || {})[f] || 'proposal'; if (ch === 'proposal') lines.push(`<div class="diff">${keyLink(o.target)} ${fieldName(f)} <span class="old">${esc(fmtVal(f, getField(D.I[o.target], f)))}</span><span class="arrow">→</span><span class="new">${esc(fmtVal(f, nv))}</span></div>`); else lines.push(`<div class="dim">${o.target} ${fieldName(f)}：維持現在的值（不變更）</div>`); } }
    else if (o.kind === 'transition'){ const legal = (WORKFLOW[D.I[o.target].status] || []).includes(o.to); const ch = (C.choices[o.target] || {}).status || (legal ? 'proposal' : 'current'); lines.push(ch === 'proposal' ? `<div class="diff">${keyLink(o.target)} 狀態 <span class="old">${esc(D.I[o.target].status)}</span><span class="arrow">→</span><span class="new">${esc(o.to)}</span></div>` : `<div class="dim">${o.target} 狀態：維持「${esc(D.I[o.target].status)}」</div>`); }
    else lines.push(opLines(cs, {ops:[o]}, false));
  }
  return lines.join('') + `<p class="hint" style="margin:6px 0 0">預覽綁定目前版本：${Object.entries(C.previewLocks).map(([k, l]) => `${k} 第 ${l} 版`).join('、')}</p>`;
}

// ---------- Agent 委派 ----------
function pgAgents(){
  const list = D.contexts.slice().reverse();
  const card = c => {
    const usable = ctxUsable(c);
    const broken = c.state === 'active' && !usable;
    const R = Object.keys(c.R);
    const inter = intersectAud(R);
    const dir = Object.keys(D.P).filter(p => D.P[p].uniform).map(p => {
      const deleg = c.delegations.find(d => d.project === p && d.state === 'active');
      const pub = subset(D.P[p].audience, inter);
      return `<tr><td>${p}</td><td>${deleg ? '<span class="c-ok">可讀</span>' : R.includes(p) ? '<span class="dim">曾授予，已撤銷</span>' : '<span class="dim">—</span>'}</td><td>${!deleg ? '<span class="dim">—</span>' : pub ? '<span class="c-ok">可寫入</span>' : `<span class="c-warn">不能寫公開內容</span> <span class="hint">（${R.filter(x => x !== p).join('、')} 的讀者較少）</span>`}</td></tr>`;
    }).join('');
    return `<div class="card" style="${broken ? 'border-color:var(--warn)' : c.state === 'closed' ? 'opacity:.7' : ''}">
      <div class="row">${c.agent ? actorHtml(c.agent) : ''}<b>${esc(c.clientName)}</b><span class="chip ${c.state === 'closed' ? 'c-dim' : broken ? 'c-warn' : 'c-ok'}">${c.state === 'closed' ? '已關閉' : broken ? '已停用' : '使用中'}</span><span class="dim">工作脈絡 ${c.id} · 建立於 ${esc(c.created)}</span></div>
      ${broken ? `<div class="banner w" style="margin-top:8px">這個 agent 的工作脈絡因 ${ctxBrokenBy(c).join('、')} 的權限設定變更而停用。它的提案都不能套用，也不能再讀取。<span class="sp"></span><button class="btn sm pri" data-a="new-ctx" data-id="${c.id}">建立新工作脈絡</button></div>` : ''}
      ${c.state === 'closed' ? `<p class="muted">${esc(c.reason || '已關閉')}</p>` : `
      <div class="row" style="margin:8px 0"><span class="dim">曾授予的專案（寫入限制的來源，撤銷也不移除）：</span>${R.map(p => `<span class="chip c-act">${p}</span>`).join('')}</div>
      <table class="t" style="margin-bottom:8px"><tr><th>專案</th><th>可讀</th><th>可寫入</th></tr>${dir}</table>
      <div class="k" style="margin:10px 0 4px">委派</div>
      <table class="t"><tr><th>專案</th><th>操作</th><th>期限</th><th>狀態</th><th></th></tr>${c.delegations.map(d => `<tr><td>${d.project}</td><td>${d.policy ? `${polSummary(d.policy)} ${d.state === 'active' && !broken ? `<button class="btn sm ghost" data-a="dpol-open" data-d="${d.id}">${UI.dpolOpen === d.id ? '收起' : '設定'}</button>` : ''}` : esc(d.template)}</td><td>${esc(d.expires)}</td><td>${d.state === 'active' ? '<span class="c-ok">有效</span>' : '<span class="dim">已撤銷</span>'}</td><td>${d.state === 'active' && !broken ? `<button class="btn sm" data-a="revoke-dg" data-id="${c.id}" data-d="${d.id}" title="只收縮這個專案；不會解除寫入限制">撤銷這個專案的委派</button>` : ''}</td></tr>`).join('')}</table>
      ${c.delegations.filter(d => d.id === UI.dpolOpen && d.policy).map(d => `<div class="card" style="margin-top:8px"><div class="k" style="margin-bottom:6px">${d.project} 的操作設定 <span class="hint">改了立即生效（policy_version ${d.policy.version}）；已送出的結果不會因此改變</span></div>${policyTable(d.policy, (kind, k) => `data-dpol="${c.id}:${d.id}:${kind}:${k}"`)}</div>`).join('')}
      <div class="k" style="margin:10px 0 4px">Token</div>
      <table class="t"><tr><th>Token</th><th>建立</th><th>狀態</th><th></th></tr>${c.tokens.map(t => `<tr><td class="mono">oxa_…${esc(t.last4)}</td><td>${esc(t.created)}</td><td>${t.state === 'active' ? '<span class="c-ok">有效</span>' : '<span class="dim">已撤銷</span>'}</td><td>${t.state === 'active' ? `<button class="btn sm" data-a="revoke-tk" data-id="${c.id}" data-t="${t.id}">撤銷 token</button>` : ''}</td></tr>`).join('')}</table>
      ${UI.newToken && UI.newToken.ctx === c.id ? tokenBox(UI.newToken.tok, false) : ''}
      <div class="row" style="margin-top:10px">
        ${broken ? '' : `<a class="btn" href="#/agents/new?ctx=${c.id}">加入專案</a><button class="btn" data-a="issue-tk" data-id="${c.id}">發新 token</button>`}
        ${c.agent ? `<a class="btn" href="#/agents/activity/${c.id}">活動${D.execs.some(e => e.ctx === c.id) ? `（${D.execs.filter(e => e.ctx === c.id && e.via !== 'undo').length}）` : ''}</a>` : ''}
        ${!broken && c.state === 'active' && c.agent ? (D.ctl[c.id] && D.ctl[c.id].suspended ? `<button class="btn" data-a="resume" data-id="${c.id}">恢復寫入</button>` : `<button class="btn" data-a="pause" data-id="${c.id}">暫停寫入</button>`) : ''}
        <button class="btn" data-a="new-ctx" data-id="${c.id}">建立新工作脈絡</button>
        <button class="btn bad" data-a="close-ctx" data-id="${c.id}">停用這個 agent</button>
      </div>
      <p class="hint">「撤銷這個專案的委派」只收縮可讀範圍，<b>不會解除寫入限制</b>；要解除只能建立新工作脈絡（舊脈絡與它的委派、token 一起結束）。發新 token 不影響現有提案。</p>`}
    </div>`;
  };
  return `<div class="page">
    <div class="row"><h1 style="margin:0">Agent 委派</h1><span style="flex:1"></span><a class="btn pri" href="#/agents/new">委派給 agent</a></div>
    <p class="sub">外部 agent（例如 Claude Code）用專屬 token 透過 MCP 讀寫你委派的專案。在歷史裡它是獨立的使用者（例如 claude-code@alice〔agent〕），和你本人分開。每個操作可設成「直接寫入」「需要核准」或「不允許」；直接寫入的變更可在「活動」檢視與回復。</p>
    ${list.length ? list.map(card).join('') : '<div class="card muted">還沒有委派任何 agent。</div>'}
  </div>`;
}
function polSummary(p){ const n = {review:0, off:0}; [...Object.values(p.commands), ...Object.values(p.fields)].forEach(m => { if (n[m] != null) n[m]++; }); return !n.review && !n.off ? '<span class="c-ok">全部直接寫入</span>' : `<span class="c-warn">${n.review ? n.review + ' 項需要核准' : ''}${n.review && n.off ? '、' : ''}${n.off ? n.off + ' 項不允許' : ''}</span>，其餘直接寫入`; }
function tokenBox(tok, first){
  return `<div class="card" style="border-color:var(--ok);margin-top:10px">
    <div class="row"><b>${first ? '完成。' : '新的 token'}</b><span class="c-warn" style="font-size:12.5px">這個 token 只顯示這一次</span></div>
    <div class="code">${tok}</div>
    <div class="row"><button class="btn sm" data-a="copy" data-t="${tok}">複製 token</button><button class="btn sm" data-a="copy" data-t="cfg">複製 MCP 設定</button><button class="btn sm">下載 client 設定檔</button></div>
    <div class="k" style="margin-top:10px">貼進 agent 的 MCP 設定</div>
    <div class="code">{
  "mcpServers": {
    "oxledger": {
      "type": "http",
      "url": "${ORIGIN}/mcp",
      "headers": { "Authorization": "Bearer ${tok.slice(0, 10)}…" }
    }
  }
}</div></div>`;
}

function pgWizard(r){
  const addTo = r.q.ctx ? D.contexts.find(c => c.id === r.q.ctx) : null;
  const W = UI.wiz && UI.wiz.key === (r.q.ctx || 'new') ? UI.wiz : (UI.wiz = {key:r.q.ctx || 'new', step:addTo ? 2 : 1, client:addTo ? addTo.client : 'claude-code', projects:[], template:'分流＋回報', policy:defaultPolicy(), days:7, done:null});
  const steps = ['選 agent 軟體','選專案','操作與期限','確認'];
  const stepBar = `<div class="steps">${steps.map((s, i) => `<span class="${W.step === i + 1 ? 'on' : W.step > i + 1 ? 'done' : ''}">${i + 1}. ${s}</span>`).join('')}</div>`;
  const existing = addTo ? Object.keys(addTo.R) : (activeCtx() && activeCtx().client === W.client ? Object.keys(activeCtx().R) : []);
  const ctxForClient = addTo || D.contexts.find(c => c.state === 'active' && c.client === W.client);
  let body = '';
  if (W.done) return `<div class="page" style="max-width:760px"><h1>${addTo || W.done.joined ? '已加入專案' : '委派完成'}</h1>${W.done.joined ? `<div class="banner o">已把 ${W.done.projects.join('、')} 加入 ${esc(W.done.clientName)} 的工作脈絡，沿用原本的 token，agent 不用重設 MCP。</div>` : tokenBox(W.done.tok, true)}<a class="btn" href="#/agents">回 Agent 委派</a></div>`;
  if (W.step === 1){
    body = `<label class="pick ${W.client === 'claude-code' ? 'on' : ''}"><input type="radio" name="cl" data-a="wz" data-k="client" data-v="claude-code" ${W.client === 'claude-code' ? 'checked' : ''}><b>Claude Code（alice-laptop）</b> <span class="dim">已登記的 client</span>${activeCtx() && activeCtx().client === 'claude-code' ? ' <span class="chip c-act">已有工作脈絡：會加入既有脈絡</span>' : ''}</label>
      <label class="pick ${W.client === 'codex' ? 'on' : ''}"><input type="radio" name="cl" data-a="wz" data-k="client" data-v="codex" ${W.client === 'codex' ? 'checked' : ''}><b>Codex CLI（alice-laptop）</b> <span class="dim">已登記的 client</span></label>`;
  } else if (W.step === 2){
    const sel = W.projects;
    const after = [...new Set([...existing, ...sel])];
    const inter = intersectAud(after);
    body = `${existing.length ? `<div class="banner i">這個 agent 的工作脈絡曾授予：${existing.join('、')}。加入的專案會和它們一起決定 agent 能寫到哪裡。</div>` : ''}
      ${Object.values(D.P).map(p => { const has = ctxForClient && ctxForClient.delegations.some(d => d.project === p.key && d.state === 'active'); const dis = !p.uniform || has; return `<label class="pick ${sel.includes(p.key) ? 'on' : ''} ${dis ? 'dis' : ''}"><input type="checkbox" data-a="wz-p" data-v="${p.key}" ${sel.includes(p.key) ? 'checked' : ''} ${dis ? 'disabled' : ''}><b>${p.key}</b> ${esc(p.name)} <span class="dim">· ${p.audience.length} 位讀者</span>${has ? ' <span class="chip c-ok">已委派</span>' : ''}${!p.uniform ? `<div class="c-warn" style="font-size:12.5px;margin-top:3px">${esc(p.whyNot)}</div>` : ''}</label>`; }).join('')}
      ${after.length ? `<div class="card"><div class="k" style="margin-bottom:4px">加入後 agent 能寫到哪裡</div><table class="t"><tr><th>專案</th><th>可讀</th><th>可寫入</th></tr>${after.map(p => `<tr><td>${p}</td><td class="c-ok">可讀</td><td>${subset(D.P[p].audience, inter) ? '<span class="c-ok">可以</span>' : `<span class="c-warn">不能寫公開內容</span> <span class="hint">（${after.filter(x => x !== p && D.P[x].audience.length < D.P[p].audience.length).join('、')} 讀者較少）</span>`}</td></tr>`).join('')}</table><p class="hint" style="margin:6px 0 0">不顯示讀者名單。之後撤銷某個專案的委派，也<b>不會</b>解除這個限制。</p></div>` : ''}`;
  } else if (W.step === 3){
    body = `<div class="row" style="margin-bottom:8px"><span class="dim">快速設定：</span><button class="btn sm" data-a="pol-all" data-v="direct">全部直接寫入（預設）</button><button class="btn sm" data-a="pol-all" data-v="review">全部需要核准</button></div>
      ${policyTable(W.policy, (kind, k) => `data-pol="${kind}:${k}"`)}
      <label class="f" style="max-width:240px"><span>期限</span><select data-a="wz-days"><option ${W.days === 1 ? 'selected' : ''} value="1">1 天</option><option ${W.days === 7 ? 'selected' : ''} value="7">7 天（建議）</option><option ${W.days === 30 ? 'selected' : ''} value="30">30 天</option></select></label>
      <p class="hint">設定只會縮小你本人的權限，不會放大；agent 讀不到私有 issue、私有 note 與受限欄位。之後版本新增的操作不會自動加進這個委派。</p>`;
  } else {
    const after = [...new Set([...existing, ...W.projects])];
    body = `<div class="card"><dl class="field"><dt>agent</dt><dd>${W.client === 'codex' ? 'Codex CLI（alice-laptop）' : 'Claude Code（alice-laptop）'}</dd><dt>新增專案</dt><dd>${W.projects.join('、') || '—'}</dd><dt>操作</dt><dd>${polSummary(W.policy)}</dd><dt>期限</dt><dd>${W.days} 天</dd><dt>工作脈絡</dt><dd>${ctxForClient ? `加入既有脈絡 ${ctxForClient.id}（沿用 token）` : '建立第一個工作脈絡並發 token'}</dd><dt>寫入範圍</dt><dd>${after.filter(p => subset(D.P[p].audience, intersectAud(after))).join('、') || '—'}</dd></dl></div>
      <div class="banner w">agent 可以不經你逐筆核准就修改（設成「直接寫入」的操作）。它讀不到私有內容、不能擴大權限；直接寫入不保證內容正確，可在「活動」檢視與回復。在歷史裡它會顯示成 ${esc(AGENTS[agentFor(W.client)].handle)}〔agent〕。</div>
      <p class="hint">建立委派會產生能長期替你寫入的憑證；如果 15 分鐘內沒有輸入過密碼，會請你確認一次（整個流程只問一次）。</p>`;
  }
  const canNext = W.step !== 2 || W.projects.length;
  return `<div class="page" style="max-width:760px"><div class="row"><a href="#/agents" class="dim" style="text-decoration:none">← Agent 委派</a></div>
    <h1>${addTo ? `把專案加入 ${esc(addTo.clientName)}` : '委派給 agent'}</h1>${stepBar}${body}
    <div class="row" style="margin-top:12px">${W.step > (addTo ? 2 : 1) ? '<button class="btn" data-a="wz-back">上一步</button>' : ''}<span style="flex:1"></span>${W.step < 4 ? `<button class="btn pri" data-a="wz-next" ${canNext ? '' : 'disabled'}>下一步</button>` : '<button class="btn pri" data-a="wz-finish">完成委派</button>'}</div></div>`;
}

// ---------- 帳號 ----------
function pgAccount(){
  return `<div class="page" style="max-width:760px"><h1>帳號</h1><p class="sub">Alice Lin（alice）</p>
    <div class="card"><div class="k" style="margin-bottom:6px">登入中的裝置</div><table class="t"><tr><th>裝置</th><th>最近使用</th><th></th></tr>${UI.sessions.map(s => `<tr><td>${esc(s.dev)}</td><td>${esc(s.at)}</td><td>${s.cur ? '<span class="dim">目前</span>' : `<button class="btn sm" data-a="kill-s" data-id="${s.id}">登出這個裝置</button>`}</td></tr>`).join('')}</table></div>
    <div class="card"><dl class="field"><dt>閒置登出</dt><dd>12 小時（背景自動更新不算活動）</dd><dt>最長登入</dt><dd>7 天</dd><dt>敏感操作</dt><dd>建立委派、發 token、建新工作脈絡：15 分鐘內輸入過密碼才不再問</dd></dl></div>
    <button class="btn bad" data-a="logout">登出</button></div>`;
}

// ---------- 管理：自訂欄位 ----------
function pgAdmin(){
  const A = UI.admin || (UI.admin = {step:0});
  const fields = [
    {name:'Severity', type:'清單', roles:'所有角色', state:'使用中'},
    {name:'估時（小時）', type:'數字', roles:'所有角色', state:SIM.fieldMigrating ? '維護中' : '使用中'},
    {name:'客戶名稱', type:'文字', roles:'Manager', state:'使用中'},
    {name:'Triage 註記', type:'文字', roles:'Non member、Anonymous（內建角色）、Manager', state:'使用中'},
    {name:'影響版本', type:'文字', roles:'所有角色', state:A.step >= 5 ? '已停用' : '使用中'},
  ];
  if (A.step >= 4) fields.push({name:'影響版本（清單）', type:'清單', roles:'所有角色', state:'使用中'});
  let wz = '';
  if (A.step === 1) wz = `<div class="card"><h3>1. 建新欄位</h3><p class="muted">不就地改型別（Q-S9）。先建一個新欄位，再把舊值複製轉換過去。</p><dl class="field"><dt>名稱</dt><dd>影響版本（清單）</dd><dt>型別</dt><dd>清單</dd><dt>選項</dt><dd>從舊值整理出 6 個：2.3、2.4、2.4.1、2.5、2.6、3.0</dd><dt>可讀角色</dt><dd>和舊欄位相同</dd></dl><button class="btn pri" data-a="adm" data-s="2">建立並預檢</button></div>`;
  if (A.step === 2) wz = `<div class="card"><h3>2. 預檢（不寫入）</h3><p>舊欄位有 <b>412</b> 個值：<span class="c-ok">398 個可以轉換</span>，<span class="c-warn">14 個無法對應到選項</span>。</p><table class="t"><tr><th>議題</th><th>舊值</th><th>原因</th></tr><tr><td>${keyLink('AUTH-101')}</td><td>2.4 / 2.5</td><td>一個值含兩個版本</td></tr><tr><td>${keyLink('WEB-80')}</td><td>下一版</td><td>不是版本號</td></tr><tr><td colspan="3" class="dim">…另外 12 筆（只列你看得到的議題）</td></tr></table>
    <div style="margin:10px 0"><label class="row" style="gap:6px"><input type="radio" name="pol" checked> 有無法轉換的值就不開始（建議）</label><label class="row" style="gap:6px"><input type="radio" name="pol" data-a="adm-pol"> 清空無法轉換的值並記錄清單</label></div>
    <button class="btn pri" data-a="adm" data-s="3">開始複製轉換</button> <button class="btn" data-a="adm" data-s="0">先去修正舊值</button></div>`;
  if (A.step === 3) wz = `<div class="card"><h3>3. 複製轉換中</h3><div class="sync"><span class="spin"></span>第 ${A.batch || 1}／12 批 · 每批各自提交，搜尋不會被長交易卡住</div><p class="hint">舊欄位在轉換期間照常可讀可寫；完成後再比對一次增量。</p></div>`;
  if (A.step === 4) wz = `<div class="card"><h3>4. 停用舊欄位</h3><p>已複製 398 個值，14 個依你的選擇清空並記錄（<a href="#/admin/fields">下載清單</a>）。</p><div class="banner w">有 3 個已存的 view 用到舊欄位「影響版本」：AUTH 待發版、WEB 回歸清單、我的版本追蹤。停用後這些條件會標成「欄位已停用」，請改用新欄位。</div><button class="btn pri" data-a="adm" data-s="5">停用舊欄位</button></div>`;
  if (A.step === 5) wz = `<div class="banner o">完成：「影響版本（清單）」已上線，舊欄位已停用（值保留，可隨時查閱）。<span class="sp"></span><button class="btn sm" data-a="adm" data-s="0">關閉</button></div>`;
  return `<div class="page"><h1>管理 · 自訂欄位</h1><p class="sub">受限欄位可以開放給內建角色（Non member、Anonymous）；篩選和顯示用同一套可讀規則（D18）。</p>
    <div class="card" style="padding:4px 8px"><table class="t"><tr><th>名稱</th><th>型別</th><th>可讀角色</th><th>狀態</th><th></th></tr>${fields.map(f => `<tr><td>${esc(f.name)}</td><td>${f.type}</td><td>${esc(f.roles)}</td><td>${f.state === '使用中' ? '<span class="c-ok">使用中</span>' : f.state === '維護中' ? '<span class="c-warn">維護中</span>' : '<span class="dim">已停用</span>'}</td><td>${f.name === '影響版本' && A.step === 0 ? '<button class="btn sm" data-a="adm" data-s="1">換型別…</button>' : ''}</td></tr>`).join('')}</table></div>
    ${wz}</div>`;
}

// ---------- 錯誤訊息一覽 ----------
function pgErrors(){
  const codes = ['conflict','version_superseded','delegation_unavailable','context_unavailable','proposal_unavailable','changeset_expired','invalid_transition','review_outdated','execution_in_progress','temporarily_unavailable','agent_suspended','undo_not_applicable'];
  const next = {conflict:'檢視差異', version_superseded:'看最新版本', delegation_unavailable:'管理委派', context_unavailable:'建立新工作脈絡', proposal_unavailable:'回待審清單', changeset_expired:'請 agent 重新提案', invalid_transition:'狀態選單', review_outdated:'重新預覽', execution_in_progress:'（自動更新）', temporarily_unavailable:'重試（沿用原 operation_id）', agent_suspended:'檢視活動與回復', undo_not_applicable:'逐欄手動處理'};
  return `<div class="page"><h1>錯誤訊息一覽</h1><p class="sub">每個錯誤都說：發生什麼、有沒有改到資料、下一步（設計 02 §15.5）。</p>
    ${codes.map(c => `<div class="banner ${['execution_in_progress','review_outdated','temporarily_unavailable'].includes(c) ? 'w' : 'b'}"><span class="mono dim" style="min-width:210px">${c}</span>${esc(errText(c, c === 'invalid_transition' ? {msg:'目前不能轉到「待驗證」；可以轉到：處理中、已拒絕'} : c === 'context_unavailable' ? {details:{project:'WEB'}} : null))}<span class="sp"></span><button class="btn sm">${next[c]}</button></div>`).join('')}
    <div class="banner w"><span class="mono dim" style="min-width:210px">reauthentication_required</span>為了發出新的 agent 憑證，請再輸入一次密碼<span class="sp"></span><button class="btn sm" data-a="stepup-demo">密碼框</button></div>
    <div class="banner b"><span class="mono dim" style="min-width:210px">login (401)</span>帳號或密碼不正確。忘記密碼請聯絡管理員。</div></div>`;
}

// ---------- 主 render ----------
function render(){
  const r = parseHash();
  const p = r.parts[0];
  if (SIM.sessionExpired && p !== 'login'){ UI.returnTo = r.raw; UI.loginReason = 'expired'; SIM.sessionExpired = false; UI.session.logged = false; markCtl(); }
  let html;
  if (!UI.session.logged || p === 'login') html = pgLogin();
  else {
    let pg;
    if (p === 'home') pg = pgHome();
    else if (p === 'issues') pg = pgIssues(r);
    else if (p === 'issue') pg = pgIssue(r.parts[1]);
    else if (p === 'new') pg = pgNew(r);
    else if (p === 'review') pg = pgReview(r);
    else if (p === 'cs' && r.parts[2] === 'conflict') pg = pgConflict(r.parts[1]);
    else if (p === 'cs') pg = pgCs(r.parts[1], r);
    else if (p === 'agents' && r.parts[1] === 'new') pg = pgWizard(r);
    else if (p === 'agents' && r.parts[1] === 'activity') pg = pgActivity(r.parts[2]);
    else if (p === 'agents') pg = pgAgents();
    else if (p === 'account') pg = pgAccount();
    else if (p === 'admin') pg = pgAdmin();
    else if (p === 'errors') pg = pgErrors();
    else pg = (window.teamPage && teamPage(r)) || pgHome();
    if (p === 'issue' && r.parts[1]){ UI.viewed = [r.parts[1], ...(UI.viewed || []).filter(k => k !== r.parts[1])].slice(0, 10); }
    html = topbar(r) + `<div class="tri">${lnav(r)}<div class="center">${pg}</div>${collab(r)}</div>`;
  }
  $('.app').innerHTML = html;
  renderModal(); renderPalette();
  if (window.renderAgent) renderAgent();
  if (window.renderGuide) renderGuide();
  if (p === 'issues' && SIM.slowFilter && UI.slow == null && (r.q.q || '').match(/[<>:]/)) startSlow();
}

// ---------- modal：step-up ----------
function needStepUp(){ return Date.now() - UI.session.authAt > 15 * 60e3; }
function stepUp(then){ if (!needStepUp()) return then(); UI.modal = {kind:'stepup', then}; renderModal(); setTimeout(() => { const i = $('#su-p'); if (i) i.focus(); }, 30); }
function renderModal(){
  const m = UI.modal; const el = $('#modal');
  if (!m){ el.innerHTML = ''; return; }
  if (m.kind === 'stepup') el.innerHTML = `<div class="modal-bg"><div class="modal"><h3>請再輸入一次密碼</h3><p class="muted">為了發出能長期替你提案的 agent 憑證。這個流程只會問這一次。</p><input type="password" id="su-p" placeholder="原型：任何密碼都可"><div class="row" style="margin-top:12px"><span style="flex:1"></span><button class="btn" data-a="modal-x">取消</button><button class="btn pri" data-a="stepup-ok">確認</button></div></div></div>`;
  if (m.kind === 'newctx') el.innerHTML = `<div class="modal-bg"><div class="modal"><h3>建立新工作脈絡</h3><p class="muted">舊脈絡 ${m.id} 會關閉，它的委派、token 與未套用的提案一併失效。新脈絡從零開始，沒有寫入限制。</p><label class="row" style="gap:6px;align-items:flex-start;margin:10px 0"><input type="checkbox" id="ack" style="margin-top:4px"> <span>我確認 ${esc(D.contexts.find(c => c.id === m.id).clientName)} 已清除舊的對話與快取，從新的 agent 工作階段開始。<span class="hint">伺服器無法驗證這一點（U14）。</span></span></label><label class="f"><span>新脈絡先委派哪些專案</span><select id="nc-p"><option value="WEB">WEB</option><option value="AUTH">AUTH</option></select></label><div class="row" style="margin-top:12px"><span style="flex:1"></span><button class="btn" data-a="modal-x">取消</button><button class="btn pri" data-a="newctx-ok">建立</button></div></div></div>`;
  if (window.renderModalD20){ const h = renderModalD20(m); if (h){ el.innerHTML = h; return; } }
  if (m.kind === 'confirm') el.innerHTML = `<div class="modal-bg"><div class="modal"><h3>${m.title}</h3><p class="muted">${m.body}</p><div class="row" style="margin-top:12px"><span style="flex:1"></span><button class="btn" data-a="modal-x">取消</button><button class="btn ${m.danger ? 'bad' : 'pri'}" data-a="confirm-ok">${m.ok}</button></div></div></div>`;
}

// ---------- ⌘K ----------
function routeIntent(t){
  const s = t.trim();
  if (!s) return {kind:'empty'};
  if (s.length > 500 || /\n/.test(t)) return {kind:'clarify', long:true};
  if (/^[A-Za-z]+-\d+$/.test(s)) return {kind:'issue_open', key:s.toUpperCase()};
  if (/^\/new\b/.test(s)) return {kind:'command_draft', cmd:'new'};
  const am = s.match(/^\/assign\s+([A-Za-z]+-\d+)\s+(\S+)/);
  if (am) return {kind:'command_draft', cmd:'assign', key:am[1].toUpperCase(), who:am[2]};
  if (/^\//.test(s)) return {kind:'command_draft', cmd:'unknown'};
  if (/^\S+\s*(:|>=|<=|>|<)/.test(s)){
    if (STRUCT_RE.test(s)) return {kind:'structured_search'};
    const bad = s.match(/(^|\s)(\w+)\s*:/); return {kind:'structured_search', error:bad ? `不認得的欄位「${bad[2]}」。可用：project、status、tracker、assignee、severity、priority、估時` : '語法錯誤'};
  }
  if (/[A-Za-z]+-\d+/.test(s)) return {kind:'clarify', key:s.match(/[A-Za-z]+-\d+/)[0].toUpperCase()};
  return {kind:'text_search'};
}
function renderPalette(){
  const el = $('#pal');
  if (!UI.palette){ el.innerHTML = ''; return; }
  const t = UI.palette.text; const r = routeIntent(t);
  let opts = [];
  if (r.kind === 'empty') opts = [['回報問題', 'go:/new?project=AUTH', 'command_draft'], ['我的待審', 'go:/review', ''], ['本週待分流', 'go:/issues?q=project:AUTH%20status:新建%20assignee:none', 'structured_search'], ['開啟 AUTH-123', 'go:/issue/AUTH-123', 'issue_open']];
  if (r.kind === 'issue_open') opts = D.I[r.key] ? [[`開啟 ${r.key}　${D.I[r.key].subject}`, 'go:/issue/' + r.key, 'issue_open']] : [[`找不到 ${r.key}，或你沒有權限查看`, '', 'issue_open']];
  if (r.kind === 'structured_search') opts = r.error ? [[`⚠ ${r.error}`, '', 'structured_search']] : [[`以條件查詢：${t}`, 'go:/issues?q=' + encodeURIComponent(t), 'structured_search']];
  if (r.kind === 'command_draft'){
    if (r.cmd === 'new') opts = [['開啟回報表單（不會直接建立）', 'go:/new?project=AUTH', 'command_draft']];
    else if (r.cmd === 'assign'){ const c = USERS.filter(u => u.login.startsWith(r.who.toLowerCase()) && D.P[r.key.split('-')[0]] && D.P[r.key.split('-')[0]].assignable.includes(u.id)); opts = c.length > 1 ? c.map(u => [`把 ${r.key} 指派給 ${uname(u.id)}（開啟編輯表單預填）`, `assign:${r.key}:${u.id}`, 'command_draft']) : c.length ? [[`把 ${r.key} 指派給 ${uname(c[0].id)}（開啟編輯表單預填）`, `assign:${r.key}:${c[0].id}`, 'command_draft']] : [[`找不到可指派給 ${r.key} 的「${r.who}」`, '', 'command_draft']]; if (c.length > 1) opts.unshift([`「${r.who}」有 ${c.length} 位，請選一位`, '', 'command_draft']); }
    else opts = [['可用指令：/new、/assign AUTH-123 bob', '', 'command_draft']];
  }
  if (r.kind === 'clarify') opts = r.long ? [['搜尋這段內容', 'go:/issues?q=' + encodeURIComponent(t.slice(0, 60)), 'clarify'], ['用它來回報問題', 'new-from:' + t, 'clarify']] : [[`開啟 ${r.key}`, 'go:/issue/' + r.key, 'clarify'], [`以文字搜尋「${t}」`, 'go:/issues?q=' + encodeURIComponent(t), 'clarify'], ['改用指令：/assign ' + r.key + ' …', 'fill:/assign ' + r.key + ' ', 'clarify']];
  if (r.kind === 'text_search') opts = [[`以文字搜尋「${t}」`, 'go:/issues?q=' + encodeURIComponent(t), 'text_search']];
  if (SIM.ai && ['text_search','clarify'].includes(r.kind)) opts.unshift([`交給右欄 AI：「${t.slice(0, 40)}」`, 'ai:' + t, 'ai_handoff']);
  el.innerHTML = `<div class="pal" data-a="pal-bg"><div class="box" data-stop="1"><input id="pal-in" value="${esc(t)}" placeholder="AUTH-123、project:AUTH status:新建、/new、或一段文字" autocomplete="off">
    ${opts.map((o, i) => `<div class="opt ${i === 0 && o[1] ? 'on' : ''}" data-a="pal-go" data-to="${esc(o[1])}">${esc(o[0])}<span class="rk">${o[2]}</span></div>`).join('')}
    <div class="opt" style="cursor:default"><span class="hint">route_kind：<b>${r.kind}</b>。${SIM.ai ? '內建 AI 已開啟：自然語言可交給右欄 AI（範圍跟著目前頁面）。' : '內建 AI 未啟用：自然語言以文字搜尋處理。'}輸入法組字中按 Enter 不會送出。</span></div></div></div>`;
  const inp = $('#pal-in'); inp.focus(); inp.setSelectionRange(t.length, t.length);
}

// ---------- 事件 ----------
document.addEventListener('input', e => {
  const t = e.target;
  if (t.dataset.f){ UI.form[t.dataset.f] = t.type === 'checkbox' ? t.checked : t.value; if (t.dataset.f === 'np') render(); }
  if (t.id === 'pal-in'){ UI.palette.text = t.value; if (!e.isComposing) renderPalette(); }
  if (t.id === 'note-tx') UI.form.note = t.value;
});
document.addEventListener('compositionend', e => { if (e.target.id === 'pal-in'){ UI.palette.text = e.target.value; renderPalette(); } });
document.addEventListener('keydown', e => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k'){ e.preventDefault(); UI.palette = {text:''}; renderPalette(); return; }
  if (e.key === 'Escape'){ if (UI.palette){ UI.palette = null; renderPalette(); } else if (UI.modal){ UI.modal = null; renderModal(); } }
  if (e.key === 'Enter'){
    if (e.target.id === 'pal-in'){ if (e.isComposing || e.keyCode === 229) return; const o = document.querySelector('.pal .opt.on'); if (o) act('pal-go', o); }
    if (e.target.id === 'sq') act('search', e.target);
    if (e.target.id === 'lg-p' || e.target.id === 'lg-u') act('login', e.target);
    if (e.target.id === 'su-p') act('stepup-ok', e.target);
  }
});
document.addEventListener('change', e => {
  const t = e.target;
  if (t.dataset.f){ UI.form[t.dataset.f] = t.type === 'checkbox' ? t.checked : t.value; if (t.dataset.f === 'np'){ render(); } }
  if (t.dataset.a === 'wz-days'){ UI.wiz.days = +t.value; }
});
document.addEventListener('click', e => {
  const inApp = e.target.closest('.app, #modal, #pal');
  if (inApp && e.target.closest('button, a, [data-a], input[type=checkbox], input[type=radio]')) { UI.clicks++; const c = $('#clk'); if (c) c.textContent = UI.clicks; }
  const el = e.target.closest('[data-a]');
  if (!el) return;
  if (el.dataset.a === 'pal-bg' && e.target.closest('[data-stop]')) return;
  act(el.dataset.a, el, e);
});

function act(a, el, ev){
  const d = el.dataset;
  switch (a){
    case 'palette': UI.palette = {text:''}; renderPalette(); break;
    case 'pal-bg': UI.palette = null; renderPalette(); break;
    case 'pal-go': {
      const to = d.to; if (!to) return;
      UI.palette = null; renderPalette();
      if (to.startsWith('go:')) go(to.slice(3));
      else if (to.startsWith('assign:')){ const [, k, u] = to.split(':'); UI.form = {edit:k, assignee:u}; go('/issue/' + k); }
      else if (to.startsWith('new-from:')){ UI.form = {ndesc:to.slice(9), nsub:''}; go('/new?project=AUTH'); }
      else if (to.startsWith('ai:')){ UI.collabHide = false; aiAsk(to.slice(3), scopeOf(parseHash())); }
      else if (to.startsWith('fill:')){ UI.palette = {text:to.slice(5)}; renderPalette(); }
      break; }
    case 'login': {
      const pw = $('#lg-p').value;
      if (pw === 'wrong'){ UI.loginErr = true; render(); return; }
      UI.loginErr = false; UI.session.logged = true; UI.session.authAt = Date.now(); UI.loginReason = null;
      const back = UI.returnTo; UI.returnTo = null; go(back && !back.startsWith('/login') ? back : '/home'); break; }
    case 'logout': UI.session.logged = false; UI.loginReason = null; go('/login'); break;
    case 'kill-s': UI.sessions = UI.sessions.filter(s => s.id !== d.id); toast('已登出那個裝置', 'ok'); render(); break;
    case 'search': { const v = $('#sq').value; UI.slow = null; go('/issues?q=' + encodeURIComponent(v)); break; }
    case 'more': UI.searchMore = true; render(); break;
    case 'slow-cancel': clearTimeout(UI.slowT); UI.slow = 'canceled'; toast('已取消查詢'); render(); break;
    case 'slow-fix': { const q = parseHash().q.q || ''; UI.slow = 'skip'; go('/issues?q=' + encodeURIComponent(q + ' ' + d.add)); break; }
    case 'snooze': UI.reminders.snoozed[d.k] = true; toast('明天 09:00 再提醒'); render(); break;
    case 'rule-off': UI.reminders.off = true; toast('已停用這條規則（只對你）'); render(); break;
    case 'viewas': UI.viewAs = d.v; render(); break;
    case 'edit': { const is = D.I[d.k]; UI.form = {edit:d.k}; UI.openLock[d.k] = UI.openLock[d.k] ?? is.lock; render(); break; }
    case 'cancel-edit': UI.form = {}; render(); break;
    case 'reload-issue': UI.openLock[d.k] = D.I[d.k].lock; UI.form.conflictMsg = null; render(); break;
    case 'reload-keep': UI.openLock[d.k] = D.I[d.k].lock; UI.form.conflictMsg = null; toast('已載入最新內容，你的輸入還在；再按一次儲存'); render(); break;
    case 'save-edit': {
      const is = D.I[d.k];
      if (UI.openLock[d.k] !== is.lock){ UI.form.conflictMsg = `這張單在你開啟後被改過（第 ${UI.openLock[d.k]} → ${is.lock} 版），你的修改<b>尚未儲存</b>。`; render(); return; }
      const parts = [];
      for (const f of ['subject','description','assignee','priority','cf:severity','cf:estimate','cf:customer','milestone','start_date','due_date']){
        if (!(f in UI.form)) continue;
        let v = UI.form[f]; if (v === '') v = null; if (f === 'cf:estimate' && v != null) v = +v;
        if (getField(is, f) !== v){ parts.push(`${fieldName(f)} ${fmtVal(f, getField(is, f))} → ${fmtVal(f, v)}`); setField(is, f, v); (is.origin = is.origin || {})[f] = 'human:' + Date.now(); }
      }
      if (parts.length){ is.lock++; is.events.push({at:clock(), actor:'u1', via:'human_direct', text:parts.join('；')}); scheduleIndex(d.k); toast('已儲存', 'ok'); }
      else toast('沒有變更');
      UI.openLock[d.k] = is.lock; UI.form = {}; render(); break; }
    case 'transition': {
      const is = D.I[d.k];
      if (UI.openLock[d.k] !== is.lock){ UI.form.conflictMsg = `這張單在你開啟後被改過，狀態沒有變更。`; render(); return; }
      if (!(WORKFLOW[is.status] || []).includes(d.to)){ toast(errText('invalid_transition', {msg:`目前不能轉到「${d.to}」；可以轉到：${WORKFLOW[is.status].join('、')}`}), 'bad'); return; }
      is.events.push({at:clock(), actor:'u1', via:'human_direct', text:`狀態 ${is.status} → ${d.to}`}); is.status = d.to; (is.origin = is.origin || {}).status = 'human:' + Date.now(); is.lock++; UI.openLock[d.k] = is.lock; scheduleIndex(d.k); toast(`已轉到「${d.to}」`, 'ok'); render(); break; }
    case 'add-note': {
      const tx = (UI.form.note || '').trim(); if (!tx){ toast('note 是空的', 'w'); return; }
      const is = D.I[d.k]; const priv = $('#note-priv').checked;
      is.notes.push({id:'n' + Date.now(), author:'u1', at:clock(), text:tx, private:priv}); is.lock++; UI.openLock[d.k] = is.lock;
      is.events.push({at:clock(), actor:'u1', via:'human_direct', text:priv ? '新增私有 note' : '新增 note'}); UI.form.note = ''; toast('已新增 note', 'ok'); render(); break; }
    case 'dup': {
      const t = $('#dup-t').value; const is = D.I[d.k];
      UI.modal = {kind:'confirm', title:`把 ${d.k} 標為 ${t} 的重複？`, body:`只建立關係，不會關閉 ${d.k}。兩張單都會多一筆歷史。`, ok:'標為重複', then:() => { is.duplicateOf = t; (D.I[t].duplicates = D.I[t].duplicates || []).push(d.k); is.lock++; D.I[t].lock++; UI.openLock[d.k] = is.lock; is.events.push({at:clock(), actor:'u1', via:'human_direct', text:`標為 ${t} 的重複`}); D.I[t].events.push({at:clock(), actor:'u1', via:'human_direct', text:`${d.k} 標為本單的重複`}); toast('已建立重複關係', 'ok'); }};
      renderModal(); break; }
    case 'find-sim': UI.form.ns = UI.form.nsub || ''; render(); break;
    case 'create': {
      const f = UI.form; const p = f.np || parseHash().q.project || 'AUTH';
      if (!(f.nsub || '').trim()){ f.newErr = '標題是必填：請補上標題再建立。'; render(); return; }
      if (!D.P[p].uniform && false){ return; }
      const n = Math.max(...Object.keys(D.I).filter(k => k.startsWith(p + '-')).map(k => +k.split('-')[1])) + 1; const key = p + '-' + n;
      D.I[key] = {key, id:'0199…' + (600 + n), project:p, tracker:'Bug', subject:f.nsub.trim(), description:f.ndesc || '', status:'新建', assignee:f.nasg || null, priority:f.npri || 'Normal', cf:{severity:f.nsev || null, estimate:null, customer:null}, notes:[], events:[{at:clock(), actor:'u1', via:'human_direct', text:'建立'}], lock:0, created:'2026-11-07', isPrivate:!!f.npriv, indexed:false, author:'u1'};
      scheduleIndex(key); UI.recent.push(key); UI.form = {};
      toast(`已建立 ${key}`, 'ok'); go('/issue/' + key); break; }
    case 'sel': { if (UI.sel.has(d.id)) UI.sel.delete(d.id); else UI.sel.add(d.id); render(); break; }
    case 'sel-page': { const gid = parseHash().q.group; const seen = {}; UI.sel.clear(); pending().filter(c => (!gid || c.group === gid) && reviewModes(c).modes.includes('queue')).forEach(c => { const t = opTargets(curVer(c)); if (t.some(k => seen[k])) return; t.forEach(k => seen[k] = 1); UI.sel.add(c.id); }); render(); break; }
    case 'sel-none': UI.sel.clear(); render(); break;
    case 'batch': runBatch(); break;
    case 'batch-stop': if (UI.batch){ UI.batch.stopReq = true; } break;
    case 'batch-clear': UI.batch = null; UI.sel.clear(); render(); break;
    case 'apply': doApply(d.id); break;
    case 'apply-resend': resendApply(d.id); break;
    case 'reject': { const cs = D.changesets.find(c => c.id === d.id); UI.modal = {kind:'confirm', title:`拒絕提案 #${cs.no}？`, body:'拒絕是終態；agent 要調整得開新提案。', ok:'拒絕', danger:true, then:() => { cs.state = 'rejected'; toast('已拒絕'); }}; renderModal(); break; }
    case 'cancel-cs': { const cs = D.changesets.find(c => c.id === d.id); cs.state = 'canceled'; toast('已取消提案'); render(); break; }
    case 'cchoice': { const C = UI.conflict; C.choices[d.k] = C.choices[d.k] || {}; C.choices[d.k][d.f] = d.v; C.previewAt = null; render(); break; }
    case 'repreview': { const C = UI.conflict; C.outdated = false; C.previewAt = null; render(); break; }
    case 'revise-apply': case 'revise-only': {
      const cs = D.changesets.find(c => c.id === d.id); const C = UI.conflict;
      const now = {}; validate(cs).conflicts.forEach(k => now[k] = D.I[k].lock);
      const changed = Object.entries(C.previewLocks || {}).some(([k, l]) => D.I[k].lock !== l);
      if (changed){ C.outdated = true; render(); return; }
      const nv = reviseByOwner(cs, C.choices); UI.conflict = null;
      if (a === 'revise-only'){ toast(`已儲存修訂版（第 ${nv.v} 版），尚未套用`); go('/cs/' + cs.id); return; }
      go('/cs/' + cs.id); doApply(cs.id, 'revise_and_apply'); break; }
    case 'wz': UI.wiz[d.k] = d.v; render(); break;
    case 'wz-p': { const W = UI.wiz; if (W.projects.includes(d.v)) W.projects = W.projects.filter(x => x !== d.v); else W.projects.push(d.v); render(); break; }
    case 'wz-next': UI.wiz.step++; render(); break;
    case 'wz-back': UI.wiz.step--; render(); break;
    case 'wz-finish': stepUp(() => finishWizard()); break;
    case 'stepup-ok': { const m = UI.modal; UI.session.authAt = Date.now(); UI.modal = null; renderModal(); if (m && m.then) m.then(); break; }
    case 'stepup-demo': UI.session.authAt = 0; stepUp(() => toast('已確認身分', 'ok')); break;
    case 'modal-x': UI.modal = null; renderModal(); break;
    case 'confirm-ok': { const m = UI.modal; UI.modal = null; if (m.then) m.then(); render(); break; }
    case 'copy': toast('已複製（原型）', 'ok'); break;
    case 'revoke-dg': { const c = D.contexts.find(x => x.id === d.id); const dg = c.delegations.find(x => x.id === d.d); UI.modal = {kind:'confirm', title:`撤銷 ${dg.project} 的委派？`, body:`agent 之後讀不到 ${dg.project}，也不能再對它提案；${dg.project} 的未套用提案不能再核准。<b>這不會解除寫入限制</b>：${dg.project} 仍算在「曾授予的專案」裡。`, ok:'撤銷委派', danger:true, then:() => { dg.state = 'revoked'; toast(`已撤銷 ${dg.project} 的委派`); }}; renderModal(); break; }
    case 'revoke-tk': { const c = D.contexts.find(x => x.id === d.id); const t = c.tokens.find(x => x.id === d.t); t.state = 'revoked'; UI.newToken = null; toast('已撤銷 token；現有提案不受影響'); render(); break; }
    case 'issue-tk': stepUp(() => { const c = D.contexts.find(x => x.id === d.id); const tok = 'oxa_' + Math.random().toString(36).slice(2, 12) + Math.random().toString(36).slice(2, 12); c.tokens.push({id:'tk-' + Date.now(), last4:tok.slice(-4), created:clock(), state:'active'}); UI.newToken = {ctx:c.id, tok}; render(); }); break;
    case 'close-ctx': { const c = D.contexts.find(x => x.id === d.id); UI.modal = {kind:'confirm', title:`停用 ${c.clientName}？`, body:'關閉工作脈絡、撤銷所有委派與 token；它的未套用提案都不能再核准。', ok:'停用', danger:true, then:() => { c.state = 'closed'; c.reason = '你在 ' + clock() + ' 停用了這個 agent'; c.tokens.forEach(t => t.state = 'revoked'); c.delegations.forEach(x => x.state = 'revoked'); }}; renderModal(); break; }
    case 'new-ctx': stepUp(() => { UI.modal = {kind:'newctx', id:d.id}; renderModal(); }); break;
    case 'newctx-ok': {
      if (!$('#ack').checked){ toast('請先勾選宣告', 'w'); return; }
      const old = D.contexts.find(x => x.id === UI.modal.id); const p = $('#nc-p').value;
      old.state = 'closed'; old.reason = '已由新工作脈絡取代（' + clock() + '）'; old.tokens.forEach(t => t.state = 'revoked'); old.delegations.forEach(x => x.state = 'revoked');
      const tok = 'oxa_' + Math.random().toString(36).slice(2, 12) + Math.random().toString(36).slice(2, 12);
      const nc = {id:'ctx-' + (D.contexts.length + 1), client:old.client, agent:old.agent, clientName:old.clientName, state:'active', reason:null, R:{[p]:D.P[p].epoch}, created:clock(), delegations:[{id:'dg-' + Date.now(), project:p, state:'active', template:'分流＋回報', expires:'2026-11-14 ' + clock().slice(0, 5), policy:defaultPolicy()}], tokens:[{id:'tk-' + Date.now(), last4:tok.slice(-4), created:clock(), state:'active'}]};
      D.contexts.push(nc); UI.newToken = {ctx:nc.id, tok}; UI.modal = null; renderModal(); toast('已建立新工作脈絡；把新 token 貼到 agent 的 MCP 設定', 'ok'); render(); break; }
    case 'adm': { UI.admin.step = +d.s; if (+d.s === 3){ UI.admin.batch = 1; const tick = () => { if (UI.admin.step !== 3) return; UI.admin.batch++; if (UI.admin.batch > 12){ UI.admin.step = 4; render(); return; } render(); setTimeout(tick, 220); }; setTimeout(tick, 220); } render(); break; }
    case 'adm-pol': break;
    case 'ev-f': UI.evF = d.v; render(); break;
    case 'sum-off': UI.summaryOff = true; toast('不再顯示 agent 摘要卡（只對你）'); render(); break;
    case 'dpol-open': UI.dpolOpen = UI.dpolOpen === d.d ? null : d.d; render(); break;
    case 'pol-all': Object.keys(UI.wiz.policy.commands).forEach(k => UI.wiz.policy.commands[k] = d.v); Object.keys(UI.wiz.policy.fields).forEach(k => UI.wiz.policy.fields[k] = d.v); render(); break;
    case 'pol-open': UI.polOpen = !UI.polOpen; break;
    default: if (window.actLayout && actLayout(a, el)) break; if (window.actTeam && actTeam(a, el)) break; if (window.actD20) actD20(a, el, ev);
  }
}

function finishWizard(){
  const W = UI.wiz;
  const clientName = W.client === 'codex' ? 'Codex CLI（alice-laptop）' : 'Claude Code（alice-laptop）';
  let ctx = D.contexts.find(c => c.state === 'active' && c.client === W.client);
  const exp = '2026-11-' + String(7 + W.days > 30 ? 30 : 7 + W.days).padStart(2, '0') + ' ' + clock().slice(0, 5);
  if (ctx && !ctxUsable(ctx)){ toast('這個 agent 的工作脈絡已停用，只能建立新工作脈絡', 'bad'); return; }
  if (ctx){
    W.projects.forEach(p => { ctx.R[p] = ctx.R[p] || D.P[p].epoch; ctx.delegations.push({id:'dg-' + Date.now() + p, project:p, state:'active', template:W.template, expires:exp, policy:JSON.parse(JSON.stringify(W.policy))}); });
    W.done = {joined:true, projects:W.projects.slice(), clientName};
  } else {
    const tok = 'oxa_' + Math.random().toString(36).slice(2, 12) + Math.random().toString(36).slice(2, 12);
    ctx = {id:'ctx-' + (D.contexts.length + 1), client:W.client, agent:agentFor(W.client), clientName, state:'active', reason:null, R:{}, created:clock(), delegations:[], tokens:[{id:'tk-' + Date.now(), last4:tok.slice(-4), created:clock(), state:'active'}]};
    W.projects.forEach(p => { ctx.R[p] = D.P[p].epoch; ctx.delegations.push({id:'dg-' + Date.now() + p, project:p, state:'active', template:W.template, expires:exp, policy:JSON.parse(JSON.stringify(W.policy))}); });
    D.contexts.push(ctx); W.done = {tok, clientName};
  }
  render();
}

function newOpId(){ return 'op-' + Math.random().toString(16).slice(2, 10); }
function doApply(id, mode){
  const cs = D.changesets.find(c => c.id === id);
  if (SIM.sessionExpired){ render(); return; }
  const op = newOpId();
  UI.apply[id] = {s:'sending', op}; render();
  setTimeout(() => {
    const res = approveAndApply(cs, cs.cur, op, mode);
    if (res.status === 'pending'){ UI.apply[id] = {s:'pending', op}; render(); setTimeout(() => { UI.apply[id] = {s:'unknown', op}; render(); }, 1800); return; }
    UI.apply[id] = {};
    if (res.status === 'rejected'){ toast(errText(res.error.code, res.error), 'bad'); if (res.error.code === 'version_superseded') go('/cs/' + id); }
    if (res.status === 'succeeded'){ (res.exec.effects || []).forEach(e => { if (e.kind === 'created') UI.recent.push(e.key); }); toast('已套用', 'ok'); }
    render();
  }, 500);
}
function resendApply(id){
  const cs = D.changesets.find(c => c.id === id); const op = UI.apply[id].op;
  UI.apply[id] = {s:'sending', op}; render();
  setTimeout(() => { const res = approveAndApply(cs, cs.cur, op); UI.apply[id] = {}; if (res.status === 'succeeded'){ (res.exec.effects || []).forEach(e => { if (e.kind === 'created') UI.recent.push(e.key); }); toast('已套用（同一個 operation_id，只執行一次）', 'ok'); } render(); }, 500);
}
function runBatch(){
  const ids = [...UI.sel];
  const B = UI.batch = {res:{}, done:false, stopped:null, stopReq:false};
  ids.forEach(id => B.res[id] = {s:'queued'});
  let next = 0, active = 0;
  const pump = () => {
    if (B.stopReq && !B.stopped){ B.stopped = '已停止送出剩下的；已送出的會繼續確認結果'; }
    while (active < 4 && next < ids.length){
      const id = ids[next++];
      if (B.stopped){ B.res[id] = {s:'stopped'}; continue; }
      if (SIM.sessionExpired){ B.stopped = '登入逾時（401），已停止送出剩下的'; B.res[id] = {s:'stopped'}; continue; }
      active++; B.res[id] = {s:'sending'}; render();
      setTimeout(() => {
        const cs = D.changesets.find(c => c.id === id);
        const r = approveAndApply(cs, cs.cur, newOpId(), 'queue');
        B.res[id] = r.status === 'succeeded' ? {s:'ok'} : r.status === 'pending' ? {s:'pending'} : {s:'bad', code:(r.error || {}).code};
        active--; render(); pump();
      }, 350 + Math.random() * 500);
    }
    if (next >= ids.length && active === 0){ B.done = true; render(); }
  };
  pump();
}
function startSlow(){
  UI.slow = 'pending';
  UI.slowT = setTimeout(() => { UI.slow = 'running'; render(); UI.slowT = setTimeout(() => { UI.slow = 'timeout'; render(); }, 1600); }, 400);
}
window.onIndexed = () => { const p = parseHash().parts[0]; if (['issues','cs','home'].includes(p)) render(); };

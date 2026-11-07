// oxledger 原型：專案框架、可管理的專案分頁、GitHub 多 repo 連動（2026-11-07 使用者要求）
// 1. 議題頁一律放在所屬專案的框架裡（上方專案名與分頁），點議題不再跳出專案。網址仍是 #/issue/KEY（永久連結不變）。
// 2. 專案分頁可新增、改名、排序、隱藏、設預設；新增類型：儲存的篩選、外部連結、隱藏的內建視圖。〔U44 已決定（2026-11-07）：專案共用〕
// 3. 專案可連多個 GitHub repo；PR 標題、分支名、commit 訊息裡的卡號（AUTH-123）自動對到議題，顯示 PR 狀態與 CI。〔U45 已決定（2026-11-07）：照建議〕
'use strict';

// ---------- 資料 ----------
const PT_BUILTIN = {timeline:'時程', milestones:'里程碑', issues:'議題', dev:'開發（GitHub）', plan:'規劃（心智圖）'};
function ptOf(key){
  D.PT = D.PT || {};
  if (!D.PT[key]) D.PT[key] = {def:'timeline', lock:0, tabs:Object.entries(PT_BUILTIN).map(([t, title]) => ({id:t, type:t, title, hidden:false}))};
  return D.PT[key];
}
function ghOf(){
  if (D.GH){ if (!D.GH.linkable) D.GH.linkable = ['acme/auth-service','acme/web-login','acme/ios-app','acme/admin-console','acme/android-app']; return D.GH; }   // 舊的保存狀態沒有 linkable
  D.GH = {
    linkable:['acme/auth-service','acme/web-login','acme/ios-app','acme/admin-console','acme/android-app'],   // 系統管理員開放、App 可存取
    repos:{AUTH:[{full:'acme/auth-service', hook:'ok'}, {full:'acme/web-login', hook:'ok'}, {full:'acme/ios-app', hook:'ok'}], WEB:[{full:'acme/web-login', hook:'ok'}]},
    prs:[
      {repo:'acme/auth-service', n:412, title:'AUTH-123 修正 SSO 登入後跳回首頁', branch:'fix/sso-return-url', state:'open', review:'approved', ci:'failure', checks:[['unit', 'success'], ['e2e-safari', 'failure'], ['lint', 'success']], at:'11-07 10:12'},
      {repo:'acme/web-login', n:88, title:'登入頁 SSO 按鈕', branch:'feat/AUTH-140-sso-button', state:'draft', review:'none', ci:'pending', checks:[['build', 'pending']], at:'11-07 09:40'},
      {repo:'acme/auth-service', n:405, title:'AUTH-101: 密碼重設信改走非同步佇列', branch:'fix/reset-mail-queue', state:'merged', review:'approved', ci:'success', checks:[['unit', 'success'], ['integration', 'success']], at:'11-05 17:20'},
      {repo:'acme/ios-app', n:77, title:'Safari 重新導向修正', branch:'fix/AUTH-91-safari-redirect', state:'open', review:'changes', ci:'success', checks:[['xcode-build', 'success'], ['ui-tests', 'success']], at:'11-06 15:02'},
      {repo:'acme/auth-service', n:398, title:'AUTH-110 簡訊驗證碼限流', branch:'feat/sms-rate-limit', state:'merged', review:'approved', ci:'success', checks:[['unit', 'success']], at:'11-03 11:30'},
      {repo:'acme/ios-app', n:75, title:'AUTH-110 iOS 簡訊驗證碼自動填入', branch:'feat/sms-autofill', state:'open', review:'pending', ci:'success', checks:[['xcode-build', 'success']], at:'11-04 18:45'},
    ],
  };
  return D.GH;
}
const ghKeyRe = /\b([A-Z][A-Z0-9]+-\d+)\b/g;
// 卡號對應：只認「這個專案有連動的 repo」；卡號要存在、屬於該專案、呼叫者看得到（私有單不顯示）
function ghPrsFor(project){
  const G = ghOf(); const repos = (G.repos[project] || []).map(r => r.full);
  return G.prs.filter(p => repos.includes(p.repo)).map(p => {
    const keys = [...new Set([...(p.title + ' ' + p.branch).matchAll(ghKeyRe)].map(m => m[1]))].filter(k => D.I[k] && D.I[k].project === project && !D.I[k].isPrivate);
    return {...p, keys};
  }).filter(p => p.keys.length);
}
const ghForIssue = k => D.I[k] ? ghPrsFor(D.I[k].project).filter(p => p.keys.includes(k)) : [];
const ghState = s => ({open:'<span class="chip c-ok">Open</span>', draft:'<span class="chip c-dim">Draft</span>', merged:'<span class="chip c-prop">Merged</span>', closed:'<span class="chip c-dim">Closed</span>'})[s];
const ghReview = s => ({approved:'<span class="chip c-ok">已核准</span>', changes:'<span class="chip c-warn">要求修改</span>', pending:'<span class="chip c-act">待 review</span>', none:'<span class="dim">—</span>'})[s];
const ghCi = s => ({success:'<span class="chip c-ok">✓ 通過</span>', failure:'<span class="chip c-bad">✗ 失敗</span>', pending:'<span class="chip c-act">● 執行中</span>'})[s] || '<span class="dim">—</span>';
const ghLink = p => `<a href="https://github.com/${p.repo}/pull/${p.n}" target="_blank" rel="noopener" class="mono">${esc(p.repo)}#${p.n}</a>`;
// 狀態不一致提示（規則算出，不自動改單）
const ghMismatch = (p, k) => p.state === 'merged' && ['新建', '處理中'].includes(D.I[k].status);

// ---------- 專案框架 ----------
const ptCanManage = () => !UI.ptViewer;   // 原型：Alice 是 AUTH／WEB 的專案管理者；勾「以一般成員檢視」看唯讀
function projFrame(key, active, body, opts){
  const P = D.P[key]; const C = ptOf(key); const o = opts || {};
  const tabs = C.tabs.filter(t => !t.hidden);
  const href = t => t.type === 'link' ? t.url : `#/project/${key}/${t.id}`;
  return `<div class="page">
    <div class="row" style="margin-bottom:6px"><h1 style="margin:0">${key} ${esc(P.name)}</h1>${o.crumb || ''}</div>
    <div class="ptabs" style="align-items:center">${tabs.map(t => `<a href="${href(t)}" ${t.type === 'link' ? 'target="_blank" rel="noopener"' : ''} class="${active === t.id ? 'on' : ''}">${esc(t.title)}${t.type === 'link' ? ' ↗' : ''}${t.id === C.def ? '' : ''}</a>`).join('')}
      <span style="flex:1"></span>
      <a href="#/project/${key}/tabs?add=1" class="${active === 'tabs' ? 'on' : ''}" title="新增分頁" style="padding:6px 10px">＋</a>
      <a href="#/project/${key}/tabs" title="管理分頁" style="padding:6px 10px">⚙</a></div>
    ${body}</div>`;
}
const unpage = h => h.replace('<div class="page">', '<div>');

// 議題清單放進框架：搜尋與常用連結都留在專案裡
function ptIssuesBody(key, q, fixed){
  let h = unpage(pgIssues({parts:['issues'], q:{q}, raw:''}))
    .replace(/<h1>議題<\/h1>/, fixed ? `<div class="banner i">這個分頁固定篩選：<span class="mono">${esc(q)}</span>　<a href="#/project/${key}/issues?q=${encodeURIComponent(q)}">在「議題」分頁調整</a></div>` : '')
    .replace('data-a="search"', `data-a="pt-search" data-p="${key}"`)
    .replace(/href="#\/issues\?q=/g, `href="#/project/${key}/issues?q=`)
    .replace(/href="#\/new\?project=AUTH"/, `href="#/new?project=${key}"`);
  if (fixed) h = h.replace(/<div class="row" style="margin:8px 0 6px">[\s\S]*?<\/div>\s*<div class="row" style="margin-bottom:12px">[\s\S]*?<\/div>/, '');
  return h;
}

const _pgProjectPT = pgProject;
pgProject = function(r){
  const key = r.parts[1]; if (!D.P[key]) return _pgProjectPT(r);
  const C = ptOf(key);
  let id = r.parts[2] || C.def;
  if (id === 'tabs') return projFrame(key, 'tabs', ptManage(key, r));
  let t = C.tabs.find(x => x.id === id);
  if (!t || t.type === 'link'){ id = C.def; t = C.tabs.find(x => x.id === id); }
  UI.ptLast = UI.ptLast || {}; UI.ptLast[key] = id;
  let body;
  if (t.type === 'timeline') body = pgTimeline(key);
  else if (t.type === 'milestones') body = pgMilestones(key);
  else if (t.type === 'plan') body = unpage(pgPlan());
  else if (t.type === 'issues') body = ptIssuesBody(key, r.q.q || `project:${key} status:open`);
  else if (t.type === 'query') body = ptIssuesBody(key, t.q, true);
  else if (t.type === 'dev') body = ghDevTab(key);
  return projFrame(key, id, body);
};

// 單張議題：留在專案框架，「議題」或剛才的分頁維持選取，上方有回去的連結
const _pgIssuePT = pgIssue;
pgIssue = function(k){
  const is = D.I[k]; const h = _pgIssuePT(k);
  if (!is || UI.viewAs === 'agent') return h;
  const C = ptOf(is.project); const last = (UI.ptLast || {})[is.project];
  const lt = last && C.tabs.find(x => x.id === last && !x.hidden);
  const active = lt ? lt.id : 'issues';
  const crumb = `<span class="dim" style="margin-left:10px;font-size:13px">›</span><a href="#/project/${is.project}/${active}" style="font-size:13px">← 回到${esc(lt ? lt.title : '議題')}</a>`;
  return projFrame(is.project, active, unpage(h).replace('<div class="card"><div class="k">描述</div>', ghIssueCard(k) + '<div class="card"><div class="k">描述</div>'), {crumb});
};

// 左側導覽：看單時標在所屬專案，不再標「全部議題」
const _lnavPT = lnav;
lnav = function(r){
  if (r.parts[0] === 'issue' && D.I[r.parts[1]]){ const p = D.I[r.parts[1]].project; return _lnavPT({...r, parts:['project', p], raw:'/project/' + p + '/issues'}); }
  return _lnavPT(r);
};

// ---------- 分頁管理 ----------
function ptManage(key, r){
  const C = ptOf(key); const can = ptCanManage(); const F = UI.ptForm || {type:'query'};
  const typeLabel = t => t.type === 'query' ? '儲存的篩選' : t.type === 'link' ? '外部連結' : '內建視圖';
  const hiddenBuiltins = Object.keys(PT_BUILTIN).filter(b => !C.tabs.some(t => t.type === b) || C.tabs.some(t => t.type === b && t.hidden));
  const addOpen = r.q.add === '1' || UI.ptForm;
  return `
    <div class="row" style="margin:6px 0 10px"><h2 style="margin:0">管理分頁</h2><span class="chip c-act">整個專案共用</span><span class="hint">（U44 已決定）改動所有成員都看得到；需要專案管理權限（manage_project）。</span>
      <span style="flex:1"></span><label class="row" style="gap:4px;font-size:12.5px"><input type="checkbox" data-a="pt-viewer" ${UI.ptViewer ? 'checked' : ''}> 以一般成員檢視</label></div>
    ${can ? '' : '<div class="banner w">你沒有這個專案的管理權限，只能檢視分頁設定。要調整請找專案管理者。</div>'}
    ${UI.ptConflict ? `<div class="banner w">別人剛改過這個專案的分頁（第 ${C.lock} 版），你的變更沒有存入。已載入最新設定，請再改一次。<span class="sp"></span><button class="btn sm" data-a="pt-ack">知道了</button></div>` : ''}
    <div class="card tw" style="padding:0"><table><thead><tr><th>順序</th><th>名稱</th><th>類型</th><th>內容</th><th>顯示</th><th>預設</th><th></th></tr></thead><tbody>
      ${C.tabs.map((t, i) => `<tr>
        <td class="mono">${can ? `<button class="btn sm ghost" data-a="pt-move" data-p="${key}" data-i="${i}" data-d="-1" ${i ? '' : 'disabled'} title="往前">↑</button><button class="btn sm ghost" data-a="pt-move" data-p="${key}" data-i="${i}" data-d="1" ${i < C.tabs.length - 1 ? '' : 'disabled'} title="往後">↓</button>` : i + 1}</td>
        <td>${can ? `<input type="text" value="${esc(t.title)}" id="pt-t-${i}" style="width:150px">` : esc(t.title)}</td>
        <td>${typeLabel(t)}</td>
        <td class="dim" style="font-size:12.5px">${t.type === 'query' ? `<span class="mono">${esc(t.q)}</span>` : t.type === 'link' ? `<span class="mono">${esc(t.url)}</span>` : '—'}</td>
        <td><input type="checkbox" ${t.hidden ? '' : 'checked'} ${can && t.id !== C.def ? '' : 'disabled'} data-a="pt-hide" data-p="${key}" data-i="${i}" title="${t.id === C.def ? '預設分頁不能隱藏' : ''}"></td>
        <td><input type="radio" name="pt-def" ${t.id === C.def ? 'checked' : ''} ${can && !t.hidden && t.type !== 'link' ? '' : 'disabled'} data-a="pt-def" data-p="${key}" data-i="${i}"></td>
        <td>${can && !PT_BUILTIN[t.type] ? `<button class="btn sm bad" data-a="pt-del" data-p="${key}" data-i="${i}">刪除</button>` : ''}</td></tr>`).join('')}
    </tbody></table></div>
    ${can ? `<div class="row" style="margin:8px 0 16px"><button class="btn pri" data-a="pt-save" data-p="${key}">儲存名稱</button><button class="btn sm" data-a="pt-race" data-p="${key}" title="原型：模擬另一位管理者剛存過">模擬別人剛改過</button><span style="flex:1"></span><button class="btn sm ghost" data-a="pt-reset" data-p="${key}">還原成預設分頁</button></div>` : ''}
    ${can ? `<div class="card" ${addOpen ? '' : 'style="display:none"'} id="pt-add"><div class="k">新增分頁</div>
      <div class="row" style="margin:6px 0">${[['query', '儲存的篩選'], ['link', '外部連結'], ['builtin', '內建視圖']].map(([v, l]) => `<label class="row" style="gap:4px"><input type="radio" name="pt-type" data-a="pt-type" data-v="${v}" ${F.type === v ? 'checked' : ''}> ${l}</label>`).join('')}</div>
      ${F.type === 'builtin' ? (hiddenBuiltins.length ? `<label class="f"><span>內建視圖</span><select id="pt-b">${hiddenBuiltins.map(b => `<option value="${b}">${PT_BUILTIN[b]}</option>`).join('')}</select></label>` : '<p class="hint">所有內建視圖都已顯示；要調整請用上表的「顯示」。</p>')
        : `<label class="f"><span>名稱</span><input type="text" id="pt-n" placeholder="${F.type === 'link' ? '例：設計稿' : '例：本週待分流'}"></label>
           ${F.type === 'query' ? `<label class="f"><span>篩選條件（和議題搜尋同語法，一定限定本專案）</span><input type="text" id="pt-q" value="status:新建 assignee:none"></label><p class="hint">分頁存的是條件，每次開都重新查；只顯示每個人自己看得到的單。</p>`
             : `<label class="f"><span>網址（只接受 https）</span><input type="text" id="pt-u" placeholder="https://www.figma.com/file/..."></label><p class="hint">外部連結在新視窗開啟，oxledger 不嵌入、不代理那個網站。</p>`}`}
      ${UI.ptErr ? `<div class="banner b">${esc(UI.ptErr)}</div>` : ''}
      <div class="row" style="margin-top:8px"><span style="flex:1"></span><button class="btn pri" data-a="pt-add" data-p="${key}">新增</button></div></div>
      ${addOpen ? '' : `<button class="btn" data-a="pt-open-add">＋ 新增分頁</button>`}` : ''}
    <p class="hint">分頁只決定「看什麼」，不改任何單；網址 <span class="mono">#/project/${key}/&lt;分頁&gt;</span> 可以分享。最多 12 個分頁。</p>`;
}

// ---------- GitHub：開發分頁 ----------
function ghDevTab(key){
  const G = ghOf(); const repos = G.repos[key] || []; const prs = ghPrsFor(key); const f = UI.ghF || 'all'; const can = ptCanManage();
  const byKey = {}; prs.forEach(p => p.keys.forEach(k => (byKey[k] = byKey[k] || []).push(p)));
  let rows = Object.entries(byKey);
  const failing = rows.filter(([, ps]) => ps.some(p => p.state !== 'merged' && p.state !== 'closed' && p.ci === 'failure'));
  const waiting = rows.filter(([, ps]) => ps.some(p => ['pending', 'changes'].includes(p.review) && p.state === 'open'));
  const mism = rows.filter(([k, ps]) => ps.some(p => ghMismatch(p, k)));
  if (f === 'ci') rows = failing; else if (f === 'review') rows = waiting; else if (f === 'mism') rows = mism;
  const fb = (v, l, n) => `<button class="btn sm ${f === v ? 'on' : ''}" data-a="gh-f" data-v="${v}">${l}${n != null ? ` <b>${n}</b>` : ''}</button>`;
  return `
    <div class="row" style="margin:6px 0 10px">${fb('all', '全部有 PR 的單', Object.keys(byKey).length)}${fb('ci', 'CI 失敗', failing.length)}${fb('review', '等 review／要求修改', waiting.length)}${fb('mism', 'PR 已合併、單未推進', mism.length)}
      <span style="flex:1"></span><button class="btn sm" data-a="gh-sim" data-p="${key}" title="原型：模擬 GitHub 送來 check_run 事件">模擬：#412 重跑 CI 通過</button></div>
    <div class="card tw" style="padding:0"><table><thead><tr><th>卡號</th><th>單的狀態</th><th>Pull request</th><th>PR</th><th>Review</th><th>CI</th><th>更新</th></tr></thead><tbody>
      ${rows.length ? rows.map(([k, ps]) => ps.map((p, i) => `<tr>
        ${i ? '' : `<td rowspan="${ps.length}">${keyLink(k)}<div class="dim" style="font-size:12px;max-width:160px">${esc(D.I[k].subject)}</div></td><td rowspan="${ps.length}">${stChip(D.I[k].status)}${ps.some(x => ghMismatch(x, k)) ? '<div class="hint" style="margin:4px 0 0">PR 已合併，單還沒推進</div>' : ''}</td>`}
        <td>${ghLink(p)}<div style="font-size:12.5px">${esc(p.title)}</div><div class="dim mono" style="font-size:11.5px">${esc(p.branch)}</div></td>
        <td>${ghState(p.state)}</td><td>${ghReview(p.review)}</td>
        <td>${ghCi(p.ci)}<div class="dim" style="font-size:11.5px">${p.checks.map(([n, s]) => `${s === 'success' ? '✓' : s === 'failure' ? '✗' : '●'} ${esc(n)}`).join(' ')}</div></td>
        <td class="dim mono" style="font-size:12px">${p.at}</td></tr>`).join('')).join('') : '<tr><td colspan="7" class="muted">沒有符合的單。</td></tr>'}
    </tbody></table></div>
    <p class="hint">對應方式：PR 標題、分支名或 commit 訊息裡寫卡號（例：<span class="mono">AUTH-123</span>、<span class="mono">fix/AUTH-91-safari</span>）。只認下面「連動的 repo」；卡號不存在、屬於別的專案、或你看不到的單都不顯示。PR 合併不會自動改單的狀態，只標提示。</p>
    <h3>連動的 repo</h3>
    <div class="card tw" style="padding:0"><table><thead><tr><th>Repository</th><th>Webhook</th><th>對到的 PR</th><th></th></tr></thead><tbody>
      ${repos.map((r, i) => `<tr><td class="mono"><a href="https://github.com/${r.full}" target="_blank" rel="noopener">${esc(r.full)}</a></td><td>${r.hook === 'ok' ? '<span class="chip c-ok">正常</span>' : r.hook === 'sync' ? '<span class="chip c-act">回填中</span>' : '<span class="chip c-warn">GitHub App 已無法存取</span>'}</td><td>${prs.filter(p => p.repo === r.full).length}</td><td>${can ? `<button class="btn sm ghost" data-a="gh-rm" data-p="${key}" data-i="${i}">解除連動</button>` : ''}</td></tr>`).join('') || '<tr><td colspan="4" class="muted">還沒有連動任何 repo。</td></tr>'}
    </tbody></table></div>
    ${can ? (() => { const avail = (G.linkable || []).filter(f => !repos.some(r => r.full === f)); return `<div class="row" style="margin:8px 0">${avail.length ? `<select id="gh-repo" style="width:280px">${avail.map(f => `<option>${esc(f)}</option>`).join('')}</select><button class="btn" data-a="gh-add" data-p="${key}">連動 repo</button>` : '<span class="muted">沒有其他可連動的 repo。</span>'}${UI.ghErr ? `<span class="chip c-bad">${esc(UI.ghErr)}</span>` : ''}</div>`; })() + `
      <p class="hint">清單只列系統管理員開放、GitHub App 可存取的 repo（02 §19.2）：PR 標題、分支名會顯示給看得到該單的所有人，所以私有 repo 要先由系統管理員決定能不能分享。一個專案最多 20 個 repo，一個 repo 也可以連多個專案（例：acme/web-login 同時連 AUTH 與 WEB，各自只對自己的卡號）。oxledger 只讀 PR、check 與 commit，不寫回 GitHub（U45 已決定）。</p>` : ''}`;
}

// ---------- GitHub：議題頁的開發卡 ----------
function ghIssueCard(k){
  const ps = ghForIssue(k);
  if (!ps.length) return `<div class="card"><div class="k">開發</div><p class="hint" style="margin:6px 0 0">還沒有 PR。在分支名或 PR 標題寫 <span class="mono">${k}</span>，連動的 repo 開 PR 後會自動出現在這裡。</p></div>`;
  return `<div class="card"><div class="k">開發 <span class="dim">· ${ps.length} 個 PR，${new Set(ps.map(p => p.repo)).size} 個 repo</span></div>
    ${ps.map(p => `<div class="row" style="margin:6px 0;gap:8px">${ghState(p.state)}${ghLink(p)}<span>${esc(p.title)}</span><span style="flex:1"></span>${ghReview(p.review)}${ghCi(p.ci)}</div>`).join('')}
    ${ps.some(p => ghMismatch(p, k)) ? `<div class="banner i" style="margin:6px 0 0">PR 已合併，這張單還在「${esc(D.I[k].status)}」。要推進請用上方「轉換狀態」；oxledger 不會自動改。</div>` : ''}</div>`;
}

// ---------- 動作 ----------
function ptSaveCheck(key){
  const C = ptOf(key);
  if (UI.ptRace){ UI.ptRace = false; C.lock++; UI.ptConflict = true; UI.ptForm = null; render(); return false; }
  return true;
}
const _actTeamPT = actTeam;
actTeam = function(a, el){
  const d = el && el.dataset || {}; const key = d.p; const C = key ? ptOf(key) : null; const i = d.i != null ? +d.i : -1;
  switch (a){
    case 'pt-search': { const v = $('#sq').value; const q = /\bproject:/.test(v) ? v : `project:${key} ${v}`; go(`/project/${key}/issues?q=` + encodeURIComponent(q.trim())); return true; }
    case 'pt-viewer': UI.ptViewer = el.checked; render(); return true;
    case 'pt-ack': UI.ptConflict = false; render(); return true;
    case 'pt-race': UI.ptRace = true; toast('（模擬）另一位管理者剛存過分頁設定'); return true;
    case 'pt-open-add': UI.ptForm = {type:'query'}; render(); return true;
    case 'pt-type': UI.ptForm = {type:d.v}; UI.ptErr = null; render(); return true;
    case 'pt-move': { if (!ptSaveCheck(key)) return true; const j = i + +d.d; [C.tabs[i], C.tabs[j]] = [C.tabs[j], C.tabs[i]]; C.lock++; render(); return true; }
    case 'pt-hide': { if (!ptSaveCheck(key)) return true; C.tabs[i].hidden = !el.checked; C.lock++; render(); return true; }
    case 'pt-def': { if (!ptSaveCheck(key)) return true; C.def = C.tabs[i].id; C.lock++; toast(`預設分頁改成「${esc(C.tabs[i].title)}」`); render(); return true; }
    case 'pt-del': UI.modal = {kind:'confirm', title:`刪除分頁「${esc(C.tabs[i].title)}」？`, body:'只刪分頁設定，不會刪任何議題。所有成員都會看不到這個分頁。', ok:'刪除', danger:true, then:() => { if (!ptSaveCheck(key)) return; const t = C.tabs.splice(i, 1)[0]; if (C.def === t.id) C.def = 'timeline'; C.lock++; toast('已刪除分頁'); render(); }}; renderModal(); return true;
    case 'pt-save': {
      if (!ptSaveCheck(key)) return true;
      const names = C.tabs.map((t, n) => ($('#pt-t-' + n) || {}).value);
      if (names.some(x => !x || !x.trim())) { toast('分頁名稱不能空白', 'bad'); return true; }
      const lower = names.map(x => x.trim().normalize('NFC').toLowerCase()); if (new Set(lower).size !== lower.length) { toast('分頁名稱不能重複', 'bad'); return true; }
      C.tabs.forEach((t, n) => t.title = names[n].trim().slice(0, 20)); C.lock++; toast('已儲存分頁名稱'); render(); return true;
    }
    case 'pt-reset': UI.modal = {kind:'confirm', title:'還原成預設分頁？', body:'自訂的篩選與連結分頁會被刪除，名稱與順序回到預設。不會改任何議題。', ok:'還原', danger:true, then:() => { delete D.PT[key]; ptOf(key); toast('已還原預設分頁'); render(); }}; renderModal(); return true;
    case 'pt-add': {
      const F = UI.ptForm || {type:'query'}; UI.ptErr = null;
      if (C.tabs.length >= 12 && F.type !== 'builtin'){ UI.ptErr = '最多 12 個分頁；先刪掉不用的。'; render(); return true; }
      if (F.type === 'builtin'){ const b = ($('#pt-b') || {}).value; if (!b) return true; const ex = C.tabs.find(t => t.type === b); if (!ptSaveCheck(key)) return true; if (ex) ex.hidden = false; else C.tabs.push({id:b, type:b, title:PT_BUILTIN[b], hidden:false}); C.lock++; UI.ptForm = null; toast(`已顯示「${PT_BUILTIN[b]}」`); go(`/project/${key}/${b}`); return true; }
      const n = ($('#pt-n').value || '').trim();
      if (!n){ UI.ptErr = '請輸入名稱。'; render(); return true; }
      if (C.tabs.some(t => t.title.normalize('NFC').toLowerCase() === n.normalize('NFC').toLowerCase())){ UI.ptErr = '已經有同名的分頁。'; render(); return true; }
      const id = 'c' + Date.now().toString(36);
      if (F.type === 'query'){
        let q = ($('#pt-q').value || '').trim(); const pm = q.match(/\bproject:(\S+)/);
        if (pm && pm[1] !== key){ UI.ptErr = `分頁一定限定本專案；條件裡的 project:${pm[1]} 不能用。`; render(); return true; }
        if (!pm) q = `project:${key} ${q}`.trim();
        if (!ptSaveCheck(key)) return true; C.tabs.push({id, type:'query', title:n.slice(0, 20), q, hidden:false});
      } else {
        const u = ($('#pt-u').value || '').trim();
        if (!/^https:\/\/[^\s/]+\.[^\s]+/.test(u)){ UI.ptErr = '網址要以 https:// 開頭。'; render(); return true; }
        if (!ptSaveCheck(key)) return true; C.tabs.push({id, type:'link', title:n.slice(0, 20), url:u, hidden:false});
      }
      C.lock++; UI.ptForm = null; toast(`已新增分頁「${esc(n)}」`);
      if (F.type === 'query') go(`/project/${key}/${id}`); else render();
      return true;
    }
    case 'gh-f': UI.ghF = d.v; render(); return true;
    case 'gh-sim': { const p = ghOf().prs.find(x => x.n === 412); p.ci = 'success'; p.checks = p.checks.map(([n]) => [n, 'success']); p.at = '剛剛'; toast('GitHub：acme/auth-service#412 的 CI 重跑通過（AUTH-123）'); render(); return true; }
    case 'gh-rm': { const G = ghOf(); const r = G.repos[key][i]; UI.modal = {kind:'confirm', title:`解除連動 ${esc(r.full)}？`, body:`AUTH 的單上不再顯示這個 repo 的 PR 與 CI。GitHub 那邊不受影響，之後可以再連動。`.replace('AUTH', key), ok:'解除連動', danger:true, then:() => { G.repos[key].splice(i, 1); toast('已解除連動'); render(); }}; renderModal(); return true; }
    case 'gh-add': {
      const v = ($('#gh-repo') || {}).value; const G = ghOf(); UI.ghErr = null;
      if (!v || !(G.linkable || []).includes(v)) UI.ghErr = '這個 repo 不能連動';
      else if ((G.repos[key] || []).some(r => r.full === v)) UI.ghErr = '已經連動了';
      else if ((G.repos[key] || []).length >= 20) UI.ghErr = '最多連 20 個 repo';
      else { (G.repos[key] = G.repos[key] || []).push({full:v, hook:'sync'}); toast(`已連動 ${esc(v)}；正在回填 open PR 與最近 30 天的 PR`); }
      render(); return true;
    }
  }
  return _actTeamPT(a, el);
};
// 文字框改名不觸發 render（等按「儲存名稱」）

FLOWS.unshift(
  {id:'f30', t:'專案連多個 GitHub repo，用卡號追 PR 與 CI', ref:'使用者 2026-11-07；U45 已決定：照建議', steps:['AUTH → 「開發（GitHub）」分頁：連動 3 個 repo，5 張單有 PR','按「CI 失敗」→ 只剩 AUTH-123（acme/auth-service#412，e2e-safari 失敗）','按「模擬：#412 重跑 CI 通過」→ CI 失敗清單變空','AUTH-110 議題頁的「開發」卡：2 個 repo 各 1 個 PR；AUTH-101 PR 已合併但單還在處理中 → 只提示，不自動改','下方從「可連動」清單選 acme/admin-console → 連動，狀態「回填中」；清單只有系統管理員開放的 repo'], go:'/project/AUTH/dev'},
  {id:'f29', t:'點議題留在專案裡；專案分頁可新增與管理', ref:'使用者 2026-11-07；U44 已決定：專案共用', steps:['AUTH 時程點 AUTH-101 → 議題頁上方仍是 AUTH 與分頁，「← 回到時程」','「議題」分頁的搜尋與常用篩選都留在專案裡','分頁列「＋」→ 儲存的篩選「本週待分流」→ 新分頁出現並開啟','「⚙」管理：改名、↑↓排序、隱藏、設預設、刪除、還原預設；外部連結只收 https','按「模擬別人剛改過」再改 → conflict；勾「以一般成員檢視」→ 唯讀'], go:'/project/AUTH/timeline'}
);

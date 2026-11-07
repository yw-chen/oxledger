// oxledger 原型：團隊運作、品牌與表單客製、AI 團隊（研究 04，2026-11-07；U24–U37 皆為建議預設、待使用者確認）
'use strict';

// ================= 團隊成員與介面角色（U24） =================
// 介面角色只決定預設，不決定權限
const PERSONA = {pm:'PM', design:'設計師', eng:'工程師'};
const PEOPLE_ROLE = {u1:['eng'], u2:['eng'], u3:['eng'], u4:['pm'], u5:['eng'], u6:['design']};

// ================= AI 團隊（U33）：每個角色一個 agent principal，綁一位 sponsor =================
const ROLE_BOTS = [
  {id:'ag-triage', handle:'triage@carol', role:'triage', title:'分流', onBehalf:'u4', client:'Grok Bot（Carol 的雲端電腦）', runtime:'grok-carol', projects:['AUTH', 'HR'],
    duty:'新單進來時補 priority、severity，標可能的重複，建議指派對象（指派由人決定）。', handoff:['ag-pmassist'],
    pol:{create_issue:'off', 'f:subject':'off', 'f:description':'off', 'f:assignee':'review', 'f:priority':'direct', 'f:cf:severity':'direct', 'f:tracker':'off', 'f:parent':'off', transition:'off', add_note:'direct', duplicate_of:'direct'},
    m:{queue:4, today:9, pend:2, blocked:0, undo:'2%'}},
  {id:'ag-pmassist', handle:'pm-assist@carol', role:'pm', title:'PM 助理', onBehalf:'u4', client:'Grok Bot（Carol 的雲端電腦）', runtime:'grok-carol', projects:['AUTH'],
    duty:'起草描述與驗收條件、整理每週健康度草稿；新 issue 一律送 Carol 核准。', handoff:[],
    pol:{create_issue:'review', 'f:subject':'review', 'f:description':'review', 'f:assignee':'off', 'f:priority':'off', 'f:cf:severity':'off', 'f:tracker':'review', 'f:parent':'review', transition:'off', add_note:'direct', duplicate_of:'off'},
    m:{queue:1, today:3, pend:1, blocked:0, undo:'0%'}},
  {id:'ag-design', handle:'design-review@erin', role:'design', title:'設計審查', onBehalf:'u6', client:'Claude Code routine（erin-mac）', runtime:'erin-mac', projects:['AUTH', 'WEB'],
    duty:'對照設計規範與無障礙準則，在設計審查單留意見。不改狀態，通過與否由 Erin 決定。', handoff:[],
    pol:{create_issue:'off', 'f:subject':'off', 'f:description':'off', 'f:assignee':'off', 'f:priority':'off', 'f:cf:severity':'off', 'f:tracker':'off', 'f:parent':'off', transition:'off', add_note:'direct', duplicate_of:'off'},
    m:{queue:0, today:2, pend:0, blocked:1, undo:'0%'}},
  {id:'ag-qa', handle:'qa@alice', role:'qa', title:'QA', onBehalf:'u1', client:'Claude Code routine（ci-runner-02）', runtime:'ci-02', projects:['AUTH'],
    duty:'issue 轉到「待驗證」後跑回歸檢查並留言；要退回「處理中」時送 Alice 核准。', handoff:[],
    trigger:'status → 待驗證',
    pol:{create_issue:'off', 'f:subject':'off', 'f:description':'off', 'f:assignee':'off', 'f:priority':'off', 'f:cf:severity':'off', 'f:tracker':'off', 'f:parent':'off', transition:'review', add_note:'direct', duplicate_of:'off'},
    m:{queue:1, today:4, pend:0, blocked:0, undo:'5%'}},
  {id:'ag-release', handle:'release@carol', role:'release', title:'發布管家', onBehalf:'u4', client:'Grok Bot（Carol 的雲端電腦）', runtime:'grok-carol', projects:['AUTH'],
    duty:'整理 release notes 草稿（留成 note）；版本欄位等 U12 版本模型。', handoff:[],
    pol:{create_issue:'off', 'f:subject':'off', 'f:description':'off', 'f:assignee':'off', 'f:priority':'off', 'f:cf:severity':'off', 'f:tracker':'off', 'f:parent':'off', transition:'off', add_note:'direct', duplicate_of:'off'},
    m:{queue:0, today:1, pend:0, blocked:0, undo:'0%'}},
];
ROLE_BOTS.forEach(b => AGENTS[b.id] = {id:b.id, handle:b.handle, client:b.client, onBehalf:b.onBehalf});
const POL_ROWS = [['create_issue','建立 issue'],['f:subject','欄位：標題（含建單初始值）'],['f:description','欄位：描述'],['f:tracker','欄位：追蹤標籤（建單）'],['f:parent','欄位：上層（建單）'],['f:assignee','欄位：指派'],['f:priority','欄位：優先度'],['f:cf:severity','欄位：嚴重度'],['transition','轉換狀態'],['add_note','新增 note'],['duplicate_of','標為重複']];
const ME = 'u1';
const ADMIN_OF = {u1:['AUTH']};  // 原型：Alice 是 AUTH 的專案管理員
const botById = id => ROLE_BOTS.find(b => b.id === id);
const modeChip = m => m === 'direct' ? '<span class="chip c-prop">直接寫入</span>' : m === 'review' ? '<span class="chip c-act">需要核准</span>' : '<span class="chip c-dim">不允許</span>';

// ================= 狀態 =================
function teamState(){
  if (!UI.team){
    let persona = null; try { persona = JSON.parse(localStorage.getItem('ox.persona.alice') || 'null'); } catch (e){}
    UI.team = {
      persona:persona || PEOPLE_ROLE[ME].slice(), personaAsk:null, personaPreview:null,
      paused:{}, requests:[], pending:[], chain:null, chainAllow:false, triaged:{}, triRun:false, snoozed:{},
      handoff:{'AUTH-140':{from:'u6', to:'u1', at:'11-06 17:20', state:'交接中', accept:'SSO 按鈕在 320px 寬不換行；錯誤狀態有可讀文字（不只靠紅色）；鍵盤可以操作。',
        arts:[{name:'登入頁 v3（匯出 PNG，附在本單）', kind:'base', ver:'v3', hash:'9f2c41…', now:'v3'}, {name:'Figma 原檔', kind:'link', url:'https://figma.example.com/file/login-v3'}]}},
      mm:{created:false},
    };
  }
  return UI.team;
}
function teamReset(){ UI.team = null; UI.formsD = null; teamState(); }

// AUTH-140：設計交接示範單
function addTeamData(d){
  if (d.I['AUTH-140']) return d;
  d.I['AUTH-140'] = {key:'AUTH-140', id:'0199…540', project:'AUTH', tracker:'Feature', subject:'登入頁改版：SSO 按鈕與錯誤狀態', description:'依設計 v3 實作登入頁的 SSO 按鈕、錯誤狀態與窄螢幕版面。設計稿與驗收條件見交接卡。', status:'新建', assignee:'u1', priority:'High', cf:{severity:null, estimate:8, customer:null}, notes:[], events:[{at:'11-06 17:20', actor:'u6', via:'human_direct', text:'建立；交接給 Alice Lin（設計 → 工程）'}], lock:1, created:'2026-11-06', isPrivate:false, indexed:true, author:'u6'};
  return d;
}
const _initialData = initialData;
initialData = function(){ return addTeamData(_initialData()); };
addTeamData(D);
const _startFlow = startFlow;
startFlow = function(id){ teamReset(); _startFlow(id); };

// ================= 品牌（U28–U30） =================
// --- 顏色數學：sRGB ↔ OKLab／OKLCH，WCAG 2.2 對比 ---
const hex2rgb = h => { h = h.replace('#', ''); if (h.length === 3) h = h.split('').map(c => c + c).join(''); return [0, 2, 4].map(i => parseInt(h.slice(i, i + 2), 16) / 255); };
const rgb2hex = c => '#' + c.map(v => Math.round(Math.min(1, Math.max(0, v)) * 255).toString(16).padStart(2, '0')).join('').toUpperCase();
const lin = v => v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
const delin = v => v <= 0.0031308 ? 12.92 * v : 1.055 * Math.pow(v, 1 / 2.4) - 0.055;
function toOklch(hex){
  const [r, g, b] = hex2rgb(hex).map(lin);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b), m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b), s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  const L = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s, A = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s, B = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
  return {L, C:Math.hypot(A, B), H:(Math.atan2(B, A) * 180 / Math.PI + 360) % 360};
}
function fromOklchRaw(L, C, H){
  const a = C * Math.cos(H * Math.PI / 180), b = C * Math.sin(H * Math.PI / 180);
  const l = Math.pow(L + 0.3963377774 * a + 0.2158037573 * b, 3), m = Math.pow(L - 0.1055613458 * a - 0.0638541728 * b, 3), s = Math.pow(L - 0.0894841775 * a - 1.2914855480 * b, 3);
  return [4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s, -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s, -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s];
}
function fromOklch(L, C, H){   // 超出色域就降彩度
  for (let c = C; c >= 0; c -= 0.005){ const v = fromOklchRaw(L, c, H); if (v.every(x => x >= -0.0005 && x <= 1.0005)) return rgb2hex(v.map(delin)); }
  return rgb2hex(fromOklchRaw(L, 0, H).map(delin));
}
const lum = hex => { const [r, g, b] = hex2rgb(hex).map(lin); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
const ratio = (a, b) => { const x = lum(a), y = lum(b); return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05); };
function fitL(hex, bgs, target, up){   // 保留色相與彩度，沿 L 軸調到所有背景都達標
  const o = toOklch(hex); let L = o.L;
  for (let i = 0; i < 200; i++){ const c = fromOklch(L, o.C, o.H); if (bgs.every(b => ratio(c, b) >= target)) return c; L += up ? 0.005 : -0.005; if (L > 1 || L < 0) break; }
  return up ? '#FFFFFF' : '#000000';
}
const rgba = (hex, a) => { const [r, g, b] = hex2rgb(hex).map(v => Math.round(v * 255)); return `rgba(${r},${g},${b},${a})`; };
const THEMES = {dark:{bg:'#0B1220', card:'#142033', up:true, chromeL:0.21}, light:{bg:'#F4F7FB', card:'#FFFFFF', up:false, chromeL:0.95}};
function derive(cfg, mode){
  const T = THEMES[mode]; const rows = [];
  const act = fitL(cfg.primary, [T.bg, T.card], 4.5, T.up);
  rows.push({pair:'可操作文字、連結 ／ 頁面與卡片背景', need:4.5, given:Math.min(ratio(cfg.primary, T.bg), ratio(cfg.primary, T.card)), used:Math.min(ratio(act, T.bg), ratio(act, T.card)), from:cfg.primary, to:act});
  const solid = ratio(cfg.primary, T.bg) >= 3 ? cfg.primary.toUpperCase() : fitL(cfg.primary, [T.bg], 3, T.up);
  rows.push({pair:'主按鈕底色 ／ 頁面背景（UI 元件）', need:3, given:ratio(cfg.primary, T.bg), used:ratio(solid, T.bg), from:cfg.primary, to:solid});
  const onBrand = ratio('#0B1220', solid) >= ratio('#FFFFFF', solid) ? '#0B1220' : '#FFFFFF';
  rows.push({pair:'主按鈕文字 ／ 主按鈕底色', need:4.5, given:ratio(onBrand, solid), used:ratio(onBrand, solid), from:onBrand, to:onBrand, note:ratio(onBrand, solid) < 4.5 ? '無法滿足：主色太接近中間亮度，按鈕文字改用粗體並在報告列出' : ''});
  rows.push({pair:'焦點框 ／ 頁面背景（UI 元件）', need:3, given:ratio(act, T.bg), used:ratio(act, T.bg), from:act, to:act});
  const so = toOklch(cfg.secondary);
  const chrome = fromOklch(T.chromeL, Math.min(so.C, 0.05), so.H);
  const chromeFg = ratio('#F1F5F9', chrome) >= ratio('#0E1726', chrome) ? '#F1F5F9' : '#0E1726';
  rows.push({pair:'頂欄與左欄文字 ／ 頂欄底色（由副色推導）', need:4.5, given:ratio(chromeFg, chrome), used:ratio(chromeFg, chrome), from:chromeFg, to:chromeFg});
  const acc2 = fitL(cfg.secondary, [T.bg], 3, T.up);
  rows.push({pair:'裝飾色（logo 漸層、圖表第二色）／ 背景', need:3, given:ratio(cfg.secondary, T.bg), used:ratio(acc2, T.bg), from:cfg.secondary, to:acc2});
  return {vars:{'--act':act, '--act-a':rgba(act, mode === 'dark' ? .13 : .10), '--hl':rgba(act, mode === 'dark' ? .28 : .20), '--brand-solid':solid, '--on-brand':onBrand, '--chrome-bg':chrome, '--chrome-fg':chromeFg, '--accent-2':acc2}, rows};
}
const BRAND_DEFAULT = {title:'oxledger', short:'ox', primary:'#67E8F9', secondary:'#C4B5FD', logo:null, logoNote:'', login:'登入後繼續工作。'};
function brandState(){
  if (!UI.brandS){
    let pub = null; try { pub = JSON.parse(localStorage.getItem('ox.brand') || 'null'); } catch (e){}
    pub = pub || {cfg:Object.assign({}, BRAND_DEFAULT), v:1, at:'11-01 09:00', by:'u4'};
    UI.brandS = {pub, versions:[pub], draft:null, preview:false, mode:null};
  }
  return UI.brandS;
}
const brandCfg = () => { const B = brandState(); return B.preview && B.draft ? B.draft : B.pub.cfg; };
function applyBrand(){
  const cfg = brandCfg();
  let css = '';
  if (cfg.primary.toUpperCase() !== BRAND_DEFAULT.primary || cfg.secondary.toUpperCase() !== BRAND_DEFAULT.secondary){
    const dk = derive(cfg, 'dark').vars, lt = derive(cfg, 'light').vars;
    const v = o => Object.entries(o).map(([k, x]) => `${k}:${x}`).join(';');
    css = `:root{${v(dk)}} :root[data-theme="light"]{${v(lt)}}`;
  }
  let el = document.getElementById('brand-css'); if (!el){ el = document.createElement('style'); el.id = 'brand-css'; document.head.appendChild(el); }
  el.textContent = css;
  document.title = cfg.title + '（原型）';
}
function brandMark(big){
  const c = brandCfg();
  const mark = c.logo ? `<img src="${c.logo}" alt="" style="height:${big ? 30 : 20}px;max-width:${big ? 160 : 110}px;object-fit:contain">` : '<i></i>';
  return `${mark}${esc(c.title)}`;
}
// SVG logo 消毒（允許清單；原型以 DOMParser 實作，正式版在 server 端）
function sanitizeSvg(text){
  const removed = [];
  const doc = new DOMParser().parseFromString(text, 'image/svg+xml');
  if (doc.querySelector('parsererror')) return {err:'不是合法的 SVG'};
  const ok = new Set(['svg','g','path','rect','circle','ellipse','line','polyline','polygon','text','tspan','defs','lineargradient','radialgradient','stop','title','desc','clippath','mask','use']);
  const walk = n => { [...n.children].forEach(c => { const t = c.tagName.toLowerCase(); if (!ok.has(t)){ removed.push('<' + t + '>'); c.remove(); return; }
    [...c.attributes].forEach(a => { const nm = a.name.toLowerCase(), val = a.value.trim().toLowerCase(); if (nm.startsWith('on')){ removed.push(nm); c.removeAttribute(a.name); } else if ((nm === 'href' || nm === 'xlink:href') && !val.startsWith('#')){ removed.push('外部參照 ' + nm); c.removeAttribute(a.name); } else if (nm === 'style' && /url\(|expression/.test(val)){ removed.push('style 內的 url()'); c.removeAttribute(a.name); } });
    walk(c); }); };
  const root = doc.documentElement; [...root.attributes].forEach(a => { if (a.name.toLowerCase().startsWith('on')){ removed.push(a.name); root.removeAttribute(a.name); } });
  walk(root);
  const out = new XMLSerializer().serializeToString(root);
  return {url:'data:image/svg+xml;base64,' + btoa(unescape(encodeURIComponent(out))), removed};
}
const SAMPLE_LOGO = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" onload="alert(1)"><script>alert('x')</script><rect x="4" y="4" width="56" height="56" rx="14" fill="#F97316"/><path d="M18 40 L32 18 L46 40 Z" fill="#fff"/><image href="https://evil.example/track.png" width="1" height="1"/></svg>`;

function pgBrand(){
  const B = brandState(); const d = B.draft || Object.assign({}, B.pub.cfg);
  const mode = document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
  const rep = (m) => derive(d, m).rows.map(r => `<tr><td>${esc(r.pair)}</td><td class="mono">≥ ${r.need}:1</td><td class="mono">${r.given.toFixed(2)}</td><td class="mono">${r.used.toFixed(2)} ${r.used >= r.need ? '<span class="c-ok">✓</span>' : '<span class="c-bad">✗</span>'}</td><td>${r.from.toUpperCase() !== r.to.toUpperCase() ? `<span class="sw" style="background:${r.from}"></span>${esc(r.from)} → <span class="sw" style="background:${r.to}"></span>${esc(r.to)}` : '<span class="dim">未調整</span>'}${r.note ? `<div class="hint c-warn">${esc(r.note)}</div>` : ''}</td></tr>`).join('');
  const hueGap = (() => { const a = toOklch(d.primary).H, b = toOklch('#C4B5FD').H; const g = Math.abs(a - b); return Math.min(g, 360 - g); })();
  const dirty = B.draft && JSON.stringify(B.draft) !== JSON.stringify(B.pub.cfg);
  return `<div class="page">
    <div class="crumb"><a href="#/admin/fields">管理</a> › 品牌與外觀</div>
    <div class="row"><h1 style="margin:0">品牌與外觀</h1><span class="chip c-dim">站台層級（U28）</span><span class="chip c-dim">目前發布 v${B.pub.v}</span><span style="flex:1"></span>
      ${dirty ? `<button class="btn" data-a="br-preview">${B.preview ? '結束預覽' : '在整個介面預覽'}</button><button class="btn pri" data-a="br-publish">發布 v${B.pub.v + 1}</button><button class="btn ghost" data-a="br-discard">捨棄草稿</button>` : '<span class="dim" style="font-size:12.5px">改任何一項就會建立草稿</span>'}</div>
    ${B.preview ? '<div class="banner i">預覽中：整個介面套用草稿，<b>只有你看得到</b>。發布前其他人仍看到 v' + B.pub.v + '。</div>' : ''}
    <p class="muted">可以改的是 token 與資產，<b>不開放自訂 CSS／JS</b>：自訂 CSS 能藏起 agent 標記、核准 diff 與權限提示，也能在不執行 JS 的情況下外洩資料（研究 04 §2.3）。改了會走「草稿 → 預覽 → 發布」，每一版都能回溯。</p>
    <div class="grid2"><div>
      <div class="card"><div class="k" style="margin-bottom:6px">名稱與文字</div>
        <label class="f"><span>系統主標題</span><input type="text" data-br="title" value="${esc(d.title)}" maxlength="40"></label>
        <label class="f"><span>短名稱（分頁、行動版）</span><input type="text" data-br="short" value="${esc(d.short)}" maxlength="12"></label>
        <label class="f"><span>登入頁說明（限縮 Markdown）</span><input type="text" data-br="login" value="${esc(d.login)}" maxlength="120"></label>
      </div>
      <div class="card"><div class="k" style="margin-bottom:6px">Logo</div>
        <div class="row" style="gap:10px">${d.logo ? `<img src="${d.logo}" alt="目前 logo" style="height:40px;max-width:180px;object-fit:contain;background:var(--bg);padding:4px;border-radius:6px">` : '<span class="dim">未設定，使用預設圖示</span>'}</div>
        <div class="row" style="margin-top:8px"><label class="btn sm">上傳 PNG／SVG<input type="file" accept="image/png,image/svg+xml" data-br="logo" style="display:none"></label><button class="btn sm" data-a="br-sample">試一個含惡意內容的 SVG</button>${d.logo ? '<button class="btn sm ghost" data-a="br-nologo">移除</button>' : ''}</div>
        ${d.logoNote ? `<div class="hint" style="margin-top:6px">${d.logoNote}</div>` : ''}
        <p class="hint">SVG 用允許清單消毒後才保存，畫面上只用 &lt;img&gt; 顯示（不 inline）；email 和 PDF 改用 server 點陣化的 PNG（U30）。深色版 logo 選填。</p>
      </div>
      <div class="card"><div class="k" style="margin-bottom:6px">顏色（只要兩個）</div>
        <div class="row"><label class="row" style="gap:6px">主色 <input type="color" data-br="primary" value="${d.primary}"></label><span class="mono dim">${esc(d.primary)}</span>
          <label class="row" style="gap:6px;margin-left:14px">副色 <input type="color" data-br="secondary" value="${d.secondary}"></label><span class="mono dim">${esc(d.secondary)}</span></div>
        <div class="row" style="margin-top:6px">${['#67E8F9|#C4B5FD|預設','#F97316|#0EA5E9|橘＋藍','#22C55E|#64748B|綠＋石板','#A855F7|#F472B6|紫＋粉','#FACC15|#1E293B|黃＋深藍'].map(x => { const [p, s, t] = x.split('|'); return `<button class="btn sm" data-a="br-swatch" data-p="${p}" data-s="${s}"><span class="sw" style="background:${p}"></span><span class="sw" style="background:${s}"></span>${t}</button>`; }).join('')}</div>
        <p class="hint">系統從這兩色生成淺色與深色兩套色階；語意色（<span class="c-prop">提案</span>、<span class="c-ok">成功</span>、<span class="c-warn">警告</span>、<span class="c-bad">危險</span>）固定，不跟品牌走（U29）。</p>
        ${hueGap < 40 ? `<div class="banner w" style="margin-top:6px">主色和「提案紫」色相只差 ${Math.round(hueGap)}°。不擋發布：提案本來就靠「提案」文字、虛線框與 diff 辨識，不只靠顏色（WCAG 1.4.1）。</div>` : ''}
      </div>
    </div><div>
      <div class="card"><div class="k" style="margin-bottom:6px">對比報告（${mode === 'dark' ? '深色' : '淺色'}模式，依實際前景／背景配對；WCAG 2.2）</div>
        <table class="t"><tr><th>配對</th><th>門檻</th><th>你給的</th><th>實際使用</th><th>自動修正</th></tr>${rep(mode)}</table>
        <p class="hint">另一個模式也會一起檢查（右上角「淺／深」切換後看）。正式版還要涵蓋 hover、selected、錯誤與提案狀態，以及高對比模式。</p></div>
      <div class="card"><div class="k" style="margin-bottom:6px">預覽元件</div>
        <div class="brprev">${brandPreview(d)}</div></div>
      <div class="card"><div class="k" style="margin-bottom:6px">版本</div>
        <table class="t"><tr><th>版本</th><th>發布</th><th>標題</th><th>主／副色</th><th></th></tr>${B.versions.slice().reverse().map(x => `<tr><td>v${x.v}${x.v === B.pub.v ? ' <span class="chip c-ok">使用中</span>' : ''}</td><td>${esc(x.at)} · ${esc(uname(x.by))}</td><td>${esc(x.cfg.title)}</td><td><span class="sw" style="background:${x.cfg.primary}"></span><span class="sw" style="background:${x.cfg.secondary}"></span></td><td>${x.v !== B.pub.v ? `<button class="btn sm" data-a="br-rollback" data-v="${x.v}">回到這版</button>` : ''}</td></tr>`).join('')}</table></div>
    </div></div>
  </div>`;
}
function brandPreview(d){
  const mode = document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
  const v = derive(d, mode).vars;
  return `<div style="border:1px solid var(--line);border-radius:8px;overflow:hidden">
    <div style="background:${v['--chrome-bg']};color:${v['--chrome-fg']};padding:6px 10px;display:flex;gap:8px;align-items:center;font-weight:800">${d.logo ? `<img src="${d.logo}" alt="" style="height:18px">` : `<i style="width:12px;height:12px;border-radius:3px;display:inline-block;background:linear-gradient(135deg,${v['--act']},${v['--accent-2']})"></i>`}${esc(d.title)}</div>
    <div style="padding:10px;display:flex;gap:8px;flex-wrap:wrap;align-items:center">
      <button class="btn" style="background:${v['--brand-solid']};border-color:${v['--brand-solid']};color:${v['--on-brand']};font-weight:700">主要按鈕</button>
      <a href="javascript:void 0" style="color:${v['--act']}">連結文字</a>
      <span class="chip c-prop">提案</span><span class="chip c-ok">成功</span><span class="chip c-warn">警告</span><span class="chip c-bad">危險</span>
      <span class="agname">triage@carol</span> <span class="chip c-prop" style="padding:0 5px">agent</span>
    </div></div>`;
}

// ================= 表單設計（U31、U32） =================
const FORM0 = {rules_version:3, layout_version:5, fields:[
  {id:'subject', label:'標題', type:'text', group:'基本', required:true},
  {id:'design_link', label:'設計連結', type:'link', group:'設計', renderer:'preview', help:'貼設計稿連結；附檔則可建立版本基準'},
  {id:'review_type', label:'審查類型', type:'list', opts:['視覺','互動','無障礙'], group:'審查', required:true},
  {id:'wcag', label:'WCAG 準則編號', type:'text', group:'審查', show_if:{f:'review_type', eq:'無障礙'}, required_if:{f:'review_type', eq:'無障礙'}, help:'例如 1.4.3'},
  {id:'severity', label:'嚴重度', type:'list', opts:['Low','Medium','High'], group:'審查', readonly_for:['Reporter']},
  {id:'acceptance', label:'驗收條件', type:'textarea', group:'交接'},
]};
function formsState(){
  if (!UI.formsD){ const pub = JSON.parse(JSON.stringify(FORM0)); UI.formsD = {pub, draft:JSON.parse(JSON.stringify(pub)), role:'Developer', vals:{review_type:'無障礙'}, result:null, mcp:null, report:null}; }
  return UI.formsD;
}
const condOk = (c, vals) => !c || vals[c.f] === c.eq;
// 單一評估器：UI 送出與 MCP validate 呼叫同一個函式（正式版是 Rust，編成 WASM 給前端）
function validateForm(form, vals, role){
  const errs = [];
  for (const f of form.fields){
    const visible = condOk(f.show_if, vals);
    const req = f.required || (f.required_if && condOk(f.required_if, vals));
    const v = vals[f.id];
    if (!visible && v) errs.push({field:f.id, code:'field_not_applicable'});
    if (req && visible && !v) errs.push({field:f.id, code:'field_required'});
    if (f.readonly_for && f.readonly_for.includes(role) && v) errs.push({field:f.id, code:'field_readonly'});
  }
  return {ok:!errs.length, rules_version:form.rules_version, errors:errs};
}
function describeForm(form, role){
  return {tracker:'設計審查', rules_version:form.rules_version, layout_version:form.layout_version,
    fields:form.fields.map(f => Object.assign({id:f.id, type:f.type}, f.opts ? {values:f.opts} : {}, f.required ? {required:true} : {}, f.required_if ? {required_if:f.required_if} : {}, f.readonly_for && f.readonly_for.includes(role) ? {readonly:true} : {}, f.show_if ? {hint_show_if:f.show_if} : {}, {group:f.group}))};
}
function publishCheck(F){
  const errs = [];
  F.draft.fields.forEach(f => {
    if (f.show_if && f.required) errs.push(`「${f.label}」無條件必填，但設了顯示條件：會被藏起來而無法填寫（show_if 不能藏 L1 必填欄位）`);
    if (f.show_if && f.required_if && (f.show_if.f !== f.required_if.f || f.show_if.eq !== f.required_if.eq)) errs.push(`「${f.label}」的必填條件和顯示條件不一致：可能在隱藏時仍被要求必填`);
  });
  return errs;
}
const rulesOf = form => JSON.stringify(form.fields.map(f => [f.id, f.required || false, f.required_if || null, f.readonly_for || null, f.opts || null]).sort((a, b) => a[0] < b[0] ? -1 : 1));   // 順序屬於版面，不算資料規則
const layoutOf = form => JSON.stringify(form.fields.map(f => [f.id, f.group, f.show_if || null, f.renderer || null, f.help || null]));
function pgForms(){
  const F = formsState(); const dr = F.draft;
  const rulesDirty = rulesOf(dr) !== rulesOf(F.pub), layoutDirty = layoutOf(dr) !== layoutOf(F.pub);
  const condTxt = c => c ? `${esc(F.draft.fields.find(x => x.id === c.f).label)} = ${esc(c.eq)}` : '<span class="dim">—</span>';
  const rows = dr.fields.map((f, i) => `<tr>
    <td><button class="btn sm ghost" data-a="fm-mv" data-i="${i}" data-d="-1" ${i ? '' : 'disabled'}>↑</button><button class="btn sm ghost" data-a="fm-mv" data-i="${i}" data-d="1" ${i < dr.fields.length - 1 ? '' : 'disabled'}>↓</button></td>
    <td><b>${esc(f.label)}</b><div class="dim mono" style="font-size:11.5px">${f.id} · ${f.type}${f.renderer ? ' · 顯示成預覽卡' : ''}</div></td>
    <td><select data-fm="group" data-i="${i}">${['基本','設計','審查','交接'].map(g => `<option ${f.group === g ? 'selected' : ''}>${g}</option>`).join('')}</select></td>
    <td>${f.id === 'wcag' ? `<label class="row" style="gap:4px"><input type="checkbox" data-fm="show_if" data-i="${i}" ${f.show_if ? 'checked' : ''}> 只在 ${condTxt({f:'review_type', eq:'無障礙'})}</label>` : '<span class="dim">一律顯示</span>'}</td>
    <td>${f.id === 'subject' ? '<span class="chip c-act">必填</span>' : f.id === 'wcag' ? `<label class="row" style="gap:4px"><input type="checkbox" data-fm="required" data-i="${i}" ${f.required ? 'checked' : ''}> 一律必填</label><label class="row" style="gap:4px"><input type="checkbox" data-fm="required_if" data-i="${i}" ${f.required_if ? 'checked' : ''}> ${condTxt({f:'review_type', eq:'無障礙'})} 時必填</label>` : f.id === 'severity' ? 'Reporter 唯讀' : `<label class="row" style="gap:4px"><input type="checkbox" data-fm="required" data-i="${i}" ${f.required ? 'checked' : ''}> 必填</label>`}</td>
  </tr>`).join('');
  const v = F.vals; const vis = f => condOk(f.show_if, v);
  const groups = [...new Set(dr.fields.map(f => f.group))];
  const input = f => { const ro = f.readonly_for && f.readonly_for.includes(F.role); const req = f.required || (f.required_if && condOk(f.required_if, v));
    const lab = `<span>${esc(f.label)}${req ? ' <span class="c-bad">*</span>' : ''}${ro ? ' <span class="chip c-dim">唯讀</span>' : ''}</span>`;
    if (f.type === 'list') return `<label class="f">${lab}<select data-fv="${f.id}" ${ro ? 'disabled' : ''}><option value="">（未選）</option>${f.opts.map(o => `<option ${v[f.id] === o ? 'selected' : ''}>${o}</option>`).join('')}</select></label>`;
    if (f.type === 'textarea') return `<label class="f">${lab}<textarea data-fv="${f.id}" rows="2">${esc(v[f.id] || '')}</textarea></label>`;
    return `<label class="f">${lab}<input type="text" data-fv="${f.id}" value="${esc(v[f.id] || '')}" ${ro ? 'disabled' : ''}>${f.help ? `<span class="hint">${esc(f.help)}</span>` : ''}${f.renderer === 'preview' && v[f.id] ? '<div class="card" style="margin:4px 0 0;padding:6px 8px;font-size:12.5px">🖼 設計預覽卡：只有連結、<b>未追蹤變更</b>（沒有版本基準）</div>' : ''}</label>`; };
  const res = (r, who) => r ? `<div class="card ${r.ok ? '' : 'b'}" style="margin-top:8px"><div class="k">${who}</div>${r.ok ? '<span class="c-ok">✓ 通過</span>' : r.errors.map(e => `<div><span class="mono c-bad">${e.code}</span> ${esc(dr.fields.find(x => x.id === e.field).label)}</div>`).join('')}<div class="dim mono" style="font-size:11.5px">rules_version ${r.rules_version}</div></div>` : '';
  return `<div class="page">
    <div class="crumb"><a href="#/admin/fields">管理</a> › 表單設計</div>
    <div class="row"><h1 style="margin:0">表單：設計審查</h1><span class="chip c-dim">rules v${F.pub.rules_version}</span><span class="chip c-dim">layout v${F.pub.layout_version}</span><span style="flex:1"></span>
      ${rulesDirty || layoutDirty ? `<button class="btn pri" data-a="fm-publish">發布（${rulesDirty ? '資料規則＋' : ''}版面）</button><button class="btn ghost" data-a="fm-discard">捨棄草稿</button>` : '<span class="dim" style="font-size:12.5px">沒有未發布的變更</span>'}</div>
    <p class="muted">分兩層（U31）：<b>資料規則</b>（必填、唯讀、條件必填）由 server 強制，畫面、REST、MCP、匯入都一樣；<b>版面</b>（分組、順序、顯示條件、說明）只影響呈現。改版面只升 layout_version，不影響 agent 寫入；改資料規則升 rules_version，要看影響報告。</p>
    ${F.report ? F.report : ''}
    <div class="card"><table class="t"><tr><th></th><th>欄位</th><th>分組 <span class="chip c-dim">版面</span></th><th>顯示條件 <span class="chip c-dim">版面</span></th><th>必填／唯讀 <span class="chip c-act">資料規則</span></th></tr>${rows}</table>
      <p class="hint">條件語言有上限：只看同一張單的欄位，eq／in／is_set，最多兩層 AND/OR，不能寫腳本。</p></div>
    <div class="grid2"><div>
      <div class="card"><div class="row"><div class="k">預覽（草稿）</div><span style="flex:1"></span><span class="dim" style="font-size:12.5px">以角色檢視</span><select data-fm="role">${['Developer','Reporter'].map(r => `<option ${F.role === r ? 'selected' : ''}>${r}</option>`).join('')}</select></div>
        ${groups.map(g => { const fs = dr.fields.filter(f => f.group === g && vis(f)); return fs.length ? `<fieldset class="fg"><legend>${g}</legend>${fs.map(input).join('')}</fieldset>` : ''; }).join('')}
        <div class="row"><button class="btn pri" data-a="fm-submit">在畫面送出</button><button class="btn" data-a="fm-mcp">用 MCP 送同樣內容</button></div>
        ${res(F.result, '畫面送出的結果')}
      </div>
    </div><div>
      <div class="card"><div class="k" style="margin-bottom:6px">agent 看到的（MCP）</div>
        <pre class="term" style="max-height:260px;overflow:auto">● oxledger.describe_form(tracker="設計審查", op="create")\n${esc(JSON.stringify(describeForm(dr, F.role), null, 1))}</pre>
        ${F.mcp ? `<pre class="term">● oxledger.validate(${esc(JSON.stringify(F.mcp.vals))})\n${esc(JSON.stringify(F.mcp.r, null, 1))}</pre><div class="hint">${F.result ? (JSON.stringify(F.result.errors) === JSON.stringify(F.mcp.r.errors) ? '<span class="c-ok">✓ 和畫面送出得到相同的錯誤碼</span>（同一個評估器）' : '<span class="c-warn">畫面和 MCP 結果不同：表單內容在兩次送出之間改過</span>') : '先在畫面送出一次，可以比較兩邊結果'}</div>` : ''}
      </div>
    </div></div>
  </div>`;
}

// ================= AI 團隊（U33–U35） =================
function pgTeam(r){
  const T = teamState();
  if (r.parts[1]) return pgBot(r.parts[1]);
  const st = b => T.paused[b.id] ? '<span class="chip c-warn">已暫停</span>' : '<span class="chip c-ok">運作中</span>';
  const reqs = T.requests.filter(x => x.state === 'sent');
  return `<div class="page">
    <div class="row"><h1 style="margin:0">AI 團隊</h1><span class="hint">類 Grok Bot 的角色化 agent。每個角色是一個 agent principal，綁一位負責的人（sponsor），權限＝sponsor 的權限 ∩ 角色允許清單（U33）。</span></div>
    <div class="card"><table class="t"><tr><th>角色</th><th>身分</th><th>sponsor</th><th>執行環境</th><th>狀態</th><th title="被交辦或觸發、還沒處理">佇列</th><th>今日寫入</th><th>待審</th><th>被擋</th><th title="7 天內被人回復的比例：可驗證的品質指標，不用模型自報的成功率">7 天回復率</th></tr>
      ${ROLE_BOTS.map(b => `<tr><td><a href="#/team/${b.id}"><b>${esc(b.title)}</b></a></td><td>${actorHtml(b.id)}</td><td>${esc(uname(b.onBehalf))}</td><td class="dim" style="font-size:12.5px">${esc(b.client)}</td><td>${st(b)}</td><td>${b.m.queue}</td><td>${b.m.today}</td><td>${b.m.pend + T.pending.filter(p => p.bot === b.id && p.state === 'proposed').length}</td><td>${b.m.blocked}</td><td>${b.m.undo}</td></tr>`).join('')}</table>
      <p class="hint">人和 agent 的成員名單分開列（左欄「成員」不含 agent），活動與歷史則混排並標〔agent〕。個人委派的 coding agent（例如 claude-code@alice）在「Agent 委派」，不算團隊角色。</p></div>
    ${reqs.length ? `<div class="card"><div class="k">你提出、等 sponsor 接受的請求</div>${reqs.map(x => `<div>${actorHtml(x.bot)}：「${esc(x.text)}」 <span class="dim">→ 已送到 ${esc(uname(botById(x.bot).onBehalf))} 的「需要你決定」</span></div>`).join('')}</div>` : ''}
    <div class="card"><div class="k" style="margin-bottom:6px">交接關係（誰能把工作交給誰）</div>
      <div class="row" style="gap:6px;flex-wrap:wrap">${ROLE_BOTS.filter(b => b.handoff.length).map(b => b.handoff.map(t => `<span class="chip c-act">${esc(b.handle)} → ${esc(botById(t).handle)}</span>`).join('')).join('')}<span class="dim" style="font-size:12.5px">其他角色沒有交接對象；agent 寫入產生的事件預設不會觸發其他 agent（U34）。</span></div>
      <p class="hint">要畫成流程說明文件，可以在專案的「圖件」區用 BPMN 畫（研究 04 §1.4）；實際生效的仍是這裡的設定，不承諾 BPMN 等於設定。</p></div>
  </div>`;
}
function pgBot(id){
  const T = teamState(); const b = botById(id); if (!b) return '<div class="page">找不到這個角色。</div>';
  const isSponsor = b.onBehalf === ME;
  const canPause = isSponsor || b.projects.some(p => (ADMIN_OF[ME] || []).includes(p));
  const myProjects = b.projects.filter(p => D.P[p] && D.P[p].audience.includes(ME));
  const hiddenProjects = b.projects.length - myProjects.length;
  const shared = ROLE_BOTS.filter(x => x.runtime === b.runtime && x.id !== b.id);
  const perm = [
    ['查看名單與安全狀態', true, '專案成員'],
    ['查看活動、diff、統計', true, '逐筆只顯示你本來就看得到的專案與欄位'],
    ['查看完整指示與設定', isSponsor || myProjects.some(p => (ADMIN_OF[ME] || []).includes(p)), isSponsor ? '你是 sponsor' : '專案管理員只看自己專案那一部分'],
    ['交辦工作（觸發）', isSponsor, isSponsor ? '你是 sponsor' : '只有 sponsor；你可以「提出請求」，由 sponsor 接受'],
    ['改指示、政策、交接對象', isSponsor, '只有 sponsor'],
    ['核准它的待審', isSponsor, 'U6 未放寬前只有 sponsor'],
    ['暫停', canPause, 'sponsor、專案管理員、站台管理員'],
  ];
  const pend = T.pending.filter(p => p.bot === b.id);
  return `<div class="page">
    <div class="crumb"><a href="#/team">AI 團隊</a> › ${esc(b.title)}</div>
    <div class="row"><h1 style="margin:0">${actorHtml(b.id)}</h1>${T.paused[b.id] ? '<span class="chip c-warn">已暫停</span>' : '<span class="chip c-ok">運作中</span>'}<span style="flex:1"></span>
      ${canPause ? (T.paused[b.id] ? `<button class="btn" data-a="bot-resume" data-id="${b.id}" ${isSponsor ? '' : 'disabled title="恢復只有 sponsor 能做（停比開容易）"'}>恢復</button>` : `<button class="btn bad" data-a="bot-pause" data-id="${b.id}">暫停寫入</button>`) : ''}</div>
    <div class="grid2"><div>
      <div class="card"><div class="k">安全摘要（所有成員看得到）</div>
        <p style="margin:6px 0">${esc(b.duty)}</p>
        <div class="dim" style="font-size:12.5px">sponsor：${esc(uname(b.onBehalf))} · 執行環境：${esc(b.client)}${b.trigger ? ` · 觸發：${esc(b.trigger)}` : ''}</div>
        ${shared.length ? `<div class="banner w" style="margin:8px 0 0">和 ${shared.map(x => esc(x.handle)).join('、')} 共用同一台電腦（記憶、檔案、登入）。它們<b>共用一個安全脈絡</b>：任一個讀過的受限來源，全部都受限制；換 principal 不能洗掉來源（研究 04 §3.5）。</div>` : ''}
      </div>
      <div class="card"><div class="k" style="margin-bottom:6px">允許清單（完整列出；未列出＝不允許）</div>
        <table class="t"><tr><th>能力</th><th>政策</th></tr>${POL_ROWS.map(([k, t]) => `<tr><td>${t}</td><td>${isSponsor ? `<select data-botpol="${k}" data-id="${b.id}">${['direct','review','off'].map(m => `<option value="${m}" ${b.pol[k] === m ? 'selected' : ''}>${MODE_LABEL[m]}</option>`).join('')}</select>` : modeChip(b.pol[k])}</td></tr>`).join('')}
          <tr><td class="dim">之後新增的命令或欄位</td><td><span class="chip c-dim">不允許</span> <span class="hint">固定</span></td></tr></table>
        <p class="hint">由「${esc(b.title)}」範本建立，起點是全部不允許（不同於個人 coding agent 的 D20「全部直接寫入」）。範本之後改版不會靜默放寬這裡。${b.pol.create_issue !== 'off' ? '建單會一併檢查標題等初始欄位：任一項需核准，整份建單就送審。' : ''}</p>
      </div>
    </div><div>
      <div class="card"><div class="k" style="margin-bottom:6px">你能做什麼</div>
        <table class="t">${perm.map(([t, ok, why]) => `<tr><td>${t}</td><td>${ok ? '<span class="c-ok">可以</span>' : '<span class="dim">不行</span>'}</td><td class="dim" style="font-size:12.5px">${esc(why)}</td></tr>`).join('')}</table></div>
      <div class="card"><div class="k" style="margin-bottom:6px">${isSponsor ? '交辦工作' : '提出請求'}</div>
        ${T.paused[b.id] ? '<span class="muted">已暫停，不能交辦。</span>' : isSponsor ? (b.id === 'ag-qa' ? `<p class="muted" style="margin:0 0 6px">觸發條件是「轉到待驗證」。也可以手動交辦：</p><button class="btn pri" data-a="bot-run" data-id="${b.id}">讓 QA 檢查 AUTH-110</button>` : '') : `<div class="row"><input type="text" id="bot-req" placeholder="例如：幫我看 AUTH-133 要補什麼資訊" style="flex:1"><button class="btn" data-a="bot-req" data-id="${b.id}">送給 ${esc(uname(b.onBehalf))}</button></div><p class="hint">只有 sponsor 能直接交辦；請求會進 sponsor 的「需要你決定」。agent 只聽交辦者，issue 內文與留言都當資料，不當指令。</p>`}
        ${pend.map(p => `<div class="card p" style="margin-top:8px"><div class="row"><span class="chip c-act">需要你核准</span>${esc(p.text)}</div>${p.state === 'proposed' && isSponsor ? `<div class="row" style="margin-top:6px"><button class="btn prop" data-a="bot-approve" data-p="${p.id}">核准並套用</button><button class="btn ghost" data-a="bot-reject" data-p="${p.id}">拒絕</button></div>` : `<div class="dim">${p.state === 'applied' ? '已套用' : p.state === 'rejected' ? '已拒絕' : '等 sponsor'}</div>`}</div>`).join('')}
      </div>
      <div class="card"><div class="k" style="margin-bottom:6px">完整指示與設定</div>
        ${isSponsor || myProjects.some(p => (ADMIN_OF[ME] || []).includes(p)) ? `<div style="font-size:12.5px">${myProjects.map(p => `<div><b>${p}</b>：照「${esc(b.title)}」職責處理 ${p} 的新單；priority 依 SLA 表；不碰私有 note。</div>`).join('')}</div>${hiddenProjects ? `<div class="hint">另有 ${hiddenProjects} 個專案的設定已隱藏：你沒有那些專案的讀取或管理權（跨專案共用的指示只有 sponsor 看得到）。</div>` : ''}` : '<span class="muted">只有 sponsor 和相關專案管理員看得到；你可以看上方的安全摘要與允許清單。</span>'}
      </div>
      ${b.id === 'ag-qa' ? loopDemo() : ''}
    </div></div>
  </div>`;
}
function loopDemo(){
  const T = teamState(); const c = T.chain;
  return `<div class="card"><div class="k" style="margin-bottom:6px">防迴圈（U34）</div>
    <p class="muted" style="margin:0 0 6px">QA 有一條觸發規則「priority 變成 Urgent 時跑回歸」。如果改 priority 的是另一個 agent 呢？</p>
    <label class="row" style="gap:6px;font-size:12.5px"><input type="checkbox" data-a="chain-allow" ${T.chainAllow ? 'checked' : ''}> 在 triage@carol 的交接對象加入 qa@alice</label>
    <button class="btn sm" data-a="chain-sim" style="margin-top:6px">模擬：triage@carol 把 AUTH-127 改成 Urgent</button>
    ${c ? `<table class="t" style="margin-top:8px"><tr><th>事件</th><th>origin</th><th>depth</th><th>結果</th></tr>${c.map(x => `<tr><td>${esc(x.e)}</td><td>${esc(x.o)}</td><td class="mono">${x.d}</td><td>${x.r}</td></tr>`).join('')}</table><p class="hint">root_event_id、parent、origin 由 server 記錄；重試、核准後套用、交接都不會洗掉 origin。每條因果鏈觸發與寫入各上限 20。只保證站內事件不會自動連鎖。</p>` : ''}
  </div>`;
}

// ================= Triage 收件匣（U26） =================
const TRI_SUG = {
  'AUTH-125':{f:{priority:'High', 'cf:severity':'High'}, note:'CSP 擋掉 script，可能和 11/05 的 CSP 調整有關。', assign:'u2'},
  'AUTH-127':{f:{priority:'Urgent', 'cf:severity':'Critical'}, note:'安全相關：離職帳號被重新啟用。', assign:'u5'},
  'AUTH-129':{dup:'AUTH-91', note:'和 AUTH-91 同樣是 callback 處理問題，可能重複。'},
  'AUTH-132':{f:{priority:'Urgent', 'cf:severity':'Critical'}, note:'安全相關：備用碼可重複使用。', assign:'u2'},
  'AUTH-133':{note:'描述不足：請補瀏覽器、時間點與大約等多久。'},
  'AUTH-134':{note:'這是功能需求，建議轉成 Feature 交給 PM 排序。'},
};
function pgTriage(){
  const T = teamState();
  const xs = Object.values(D.I).filter(i => i.project === 'AUTH' && i.status === '新建' && !i.assignee && !T.snoozed[i.key]);
  const sugCell = i => { const s = TRI_SUG[i.key]; if (!s) return '<span class="dim">—</span>';
    if (!T.triRun) return '<span class="dim">（還沒跑）</span>';
    return `<div style="font-size:12.5px">${s.note ? esc(s.note) : ''}${s.assign ? `<div><span class="chip c-act">需要核准</span> 建議指派 ${esc(uname(s.assign))} <span class="dim">→ 等 Carol</span></div>` : ''}</div>`; };
  return `<div class="page">
    <div class="row"><h1 style="margin:0">Triage 收件匣 · AUTH</h1><span class="chip c-dim">本週負責：Carol Hsu（PM）</span><span style="flex:1"></span>
      ${T.triRun ? '<span class="dim" style="font-size:12.5px">triage@carol 已跑過（直接寫入的可在歷史回復）</span>' : '<button class="btn" data-a="tri-run">模擬：Carol 交辦 triage@carol 跑一次</button>'}</div>
    <p class="muted">新進、未指派的單先停在這裡。人做決定：接受、拒絕、標重複、稍後；triage bot 只補 priority、severity、note 與重複標記，<b>指派一律交給人</b>（照 Linear 的做法；U26、§3.4 範本）。</p>
    <div class="card"><table class="t"><tr><th>單號</th><th>標題</th><th>優先度／嚴重度</th><th>triage@carol</th><th>決定</th></tr>
      ${xs.map(i => `<tr><td>${keyLink(i.key)}</td><td>${esc(i.subject)}${i.duplicateOf ? `<div class="hint">已標為 ${i.duplicateOf} 的重複</div>` : ''}</td><td>${esc(i.priority)} ／ ${esc(i.cf.severity || '—')}${i.origin && i.origin.priority && AGENTS[(D.execs.find(e => e.id === i.origin.priority) || {}).actor] ? ' <span class="chip c-prop" style="padding:0 5px">agent</span>' : ''}</td><td>${sugCell(i)}</td>
        <td style="white-space:nowrap"><select data-tri-as="${i.key}" style="width:auto"><option value="">接受並指派…</option>${D.P.AUTH.assignable.map(u => `<option value="${u}">${esc(uname(u))}</option>`).join('')}</select> <button class="btn sm" data-a="tri-decline" data-k="${i.key}">拒絕</button> <button class="btn sm ghost" data-a="tri-snooze" data-k="${i.key}">稍後</button></td></tr>`).join('') || '<tr><td colspan="5" class="muted">收件匣清空了。</td></tr>'}</table></div>
  </div>`;
}

// ================= 交接卡（U25）：放在 issue 頁中間 =================
function handoffCard(key){
  const T = teamState(); const h = T.handoff[key]; if (!h) return '';
  const base = h.arts.find(a => a.kind === 'base'); const changed = base && base.now !== base.ver;
  const me = h.to === ME;
  return `<div class="card handoff ${changed ? 'warn' : ''}">
    <div class="row"><span class="chip c-act">交接</span><b>${esc(uname(h.from))}（設計師）→ ${esc(uname(h.to))}（工程師）</b><span class="dim">${esc(h.at)}</span><span class="chip ${h.state === '已接手' ? 'c-ok' : h.state === '已退回' ? 'c-warn' : 'c-dim'}">${esc(h.state)}</span></div>
    ${changed ? `<div class="banner w" style="margin:8px 0 0">交接後設計稿變了：交接時是 <b>${base.ver}</b>，現在是 <b>${base.now}</b>。請確認實作要跟哪一版。</div>` : ''}
    <table class="t" style="margin-top:8px">${h.arts.map(a => `<tr><td>${esc(a.name)}</td><td>${a.kind === 'base' ? `<span class="chip c-ok">有版本基準</span> <span class="mono dim">${a.ver} · ${a.hash}</span>` : `<span class="chip c-dim">僅有連結、未追蹤變更</span> <span class="dim" style="font-size:12px">${esc(a.url)}</span>`}</td></tr>`).join('')}</table>
    <div style="margin-top:6px"><span class="k">驗收條件</span><div style="font-size:13px">${esc(h.accept)}</div></div>
    <div class="row" style="margin-top:8px">${me && h.state === '交接中' ? `<button class="btn pri" data-a="ho-take" data-k="${key}">接手</button><button class="btn" data-a="ho-back" data-k="${key}">退回重交</button>` : ''}${changed && me ? `<button class="btn" data-a="ho-ack" data-k="${key}">改用 ${base.now} 當基準</button>` : ''}<span style="flex:1"></span><button class="btn sm ghost" data-a="ho-sim" data-k="${key}" title="原型：模擬設計師更新附檔">模擬：Erin 上傳 v${(+((base && base.now) || 'v3').slice(1)) + 1}</button></div>
    <p class="hint">「僅有連結」的產物，系統偵測不到變更，所以不會顯示成「沒變」。</p>
  </div>`;
}

// ================= 心智圖輕量示範（U27 待選） =================
function pgPlan(){
  const T = teamState();
  const kids = [['AUTH-91', 70], ['AUTH-123', 130], ['AUTH-129', 190], ['AUTH-140', 250]];
  const node = (x, y, w, t, cls) => `<g class="${cls}"><rect x="${x}" y="${y}" width="${w}" height="30" rx="8"/><text x="${x + 10}" y="${y + 20}">${esc(t)}</text></g>`;
  return `<div class="page">
    <div class="crumb">AUTH › 規劃</div>
    <div class="row"><h1 style="margin:0">心智圖：SSO 登入改版</h1><span class="chip c-dim">U27 已決定：心智圖排在 list／board／triage、roadmap、Gantt 之後</span></div>
    <p class="muted">PM 探索期拆需求用（研究 04 §1.4）。節點就是 issue（投影視圖）；<span class="c-prop">紫色虛線</span>是 AI 建議、還沒建立的節點，按了才建立。拖節點改 parent 要走提案，這裡不示範。</p>
    <div class="card"><svg viewBox="0 0 640 320" class="mm" role="img" aria-label="SSO 登入改版心智圖">
      ${kids.map(([k, y]) => `<path d="M190 160 C 260 160, 260 ${y + 15}, 330 ${y + 15}"/>`).join('')}<path d="M190 160 C 260 160, 260 ${T.mm.created ? 325 : 305}, 330 ${T.mm.created ? 325 : 305}" class="${T.mm.created ? '' : 'sug'}"/>
      ${node(20, 145, 170, 'SSO 登入改版（epic）', 'root')}
      ${kids.map(([k, y]) => `<a href="#/issue/${k}">${node(330, y, 290, k + ' ' + D.I[k].subject.slice(0, 16), 'n')}</a>`).join('')}
      ${T.mm.created ? node(330, 310, 290, T.mm.created + ' IdP 設定精靈', 'n') : node(330, 290, 290, '建議：IdP 設定精靈（未建立）', 'sug')}
    </svg>
    ${T.mm.created ? `<div class="banner i">已建立 ${keyLink(T.mm.created)}（你按的，記在你名下）。</div>` : '<div class="row"><button class="btn prop" data-a="mm-create">建立建議節點</button><span class="hint">建立後才會變成正式 issue</span></div>'}</div>
  </div>`;
}

// ================= 首頁：介面角色與範本（U24） =================
const PERSONA_TPL = {
  pm:['decide', 'triage', 'health', 'aiteam', 'agent'],
  design:['decide', 'handoff', 'reviewme', 'recent'],
  eng:['decide', 'mine', 'handoff', 'agent', 'cont'],
};
function personaDefault(roles){
  roles = roles || teamState().persona; const ids = [];
  // 多個角色輪流取，避免上限 6 個時後選的角色整個被擠掉
  for (let i = 0; i < 6; i++) roles.forEach(r => { const id = (PERSONA_TPL[r] || [])[i]; if (id && !ids.includes(id)) ids.push(id); });
  if (!ids.includes('decide')) ids.unshift('decide');   // 保護清單：一定保留
  return ids.slice(0, 6).map((id, i) => ({id, key:'p' + i + id, wide:id === 'decide'}));
}
Object.assign(WIDGETS, {
  health:{t:'專案健康度', desc:'各專案 On track／At risk／Off track（人判斷，AI 只起草）'},
  handoff:{t:'交接', desc:'交給你或你交出去的工作，含產物是否變更'},
  reviewme:{t:'等我簽核', desc:'設計 QA、設計審查等你決定的單'},
  aiteam:{t:'AI 團隊', desc:'每個角色今天做了什麼'},
});
function wTeam(it){
  const T = teamState();
  switch (it.id){
    case 'health': return `<table class="t"><tr><th>專案</th><th>健康度</th><th>最新更新</th></tr><tr><td>AUTH</td><td><span class="chip c-warn">At risk</span></td><td style="font-size:12.5px">SSO 改版卡在 Safari 重導（AUTH-123）；pm-assist@carol 起草了週報待 Carol 確認</td></tr><tr><td>WEB</td><td><span class="chip c-ok">On track</span></td><td style="font-size:12.5px">—</td></tr></table>`;
    case 'handoff': return Object.entries(T.handoff).map(([k, h]) => { const b = h.arts.find(a => a.kind === 'base'); return `<div class="row">${keyLink(k)}<span>${esc(uname(h.from))} → ${esc(uname(h.to))}</span><span class="chip ${h.state === '已接手' ? 'c-ok' : 'c-dim'}">${esc(h.state)}</span>${b && b.now !== b.ver ? '<span class="chip c-warn">設計稿已變更</span>' : ''}</div>`; }).join('');
    case 'reviewme': return `<div class="row">${keyLink('AUTH-140')}<span>設計 QA：實作完成後由 Erin 簽核</span></div><div class="hint">design-review@erin 只留意見，通過與否由人決定。</div>`;
    case 'aiteam': return ROLE_BOTS.map(b => `<div class="row" style="font-size:12.5px">${actorHtml(b.id)}<span class="dim">今天 ${b.m.today} 筆${b.m.pend ? `、待審 ${b.m.pend}` : ''}</span>${T.paused[b.id] ? '<span class="chip c-warn">已暫停</span>' : ''}</div>`).join('') + '<a class="btn sm" href="#/team" style="margin-top:6px">AI 團隊</a>';
  }
  return '';
}
function personaBar(){
  const T = teamState();
  return `<div class="pbar2"><span class="dim">介面角色</span>${Object.entries(PERSONA).map(([k, t]) => `<button class="btn sm ${T.persona.includes(k) ? 'on' : ''}" data-a="pz-toggle" data-r="${k}" aria-pressed="${T.persona.includes(k)}">${t}</button>`).join('')}
    <span class="hint">只決定預設首頁與捷徑，不影響權限（U24）</span><span style="flex:1"></span>
    <span class="dim">以角色預覽</span><select data-a="pz-preview" style="width:auto"><option value="">（我的首頁）</option>${Object.entries(PERSONA).map(([k, t]) => `<option value="${k}" ${T.personaPreview === k ? 'selected' : ''}>${t} 的預設</option>`).join('')}</select></div>
    ${T.personaAsk ? `<div class="banner i">介面角色改成「${T.persona.map(r => PERSONA[r]).join('＋')}」。要套用這個角色的預設首頁嗎？<button class="btn sm pri" data-a="pz-apply">套用新預設</button><button class="btn sm" data-a="pz-keep">保留我的</button><span class="hint">「需要你決定」不論哪個都會保留</span></div>` : ''}
    ${T.personaPreview ? `<div class="banner w">正在預覽「${PERSONA[T.personaPreview]}」的預設首頁（唯讀）。內容仍依<b>你的</b>權限過濾，不會因為預覽而看到更多。<button class="btn sm" data-a="pz-exit">回到我的首頁</button></div>` : ''}`;
}
const _homePage = homePage;
homePage = function(){
  const T = teamState(); const H = homePrefs();
  if (T.personaPreview){ const keep = {items:H.items, edit:H.edit}; H.items = personaDefault([T.personaPreview]); H.edit = false; const out = _homePage(); H.items = keep.items; H.edit = keep.edit; return `<div class="page" style="padding-bottom:0">${personaBar()}</div>` + out.replace('自訂首頁</button>', '自訂首頁</button>').replace(/<button class="btn sm" data-a="w-edit">自訂首頁<\/button>/, ''); }
  return `<div class="page" style="padding-bottom:0">${personaBar()}</div>` + _homePage();
};
// 首頁沒存過就用角色範本
(function(){ let saved = null; try { saved = localStorage.getItem('ox.home.alice'); } catch (e){} if (!saved && !UI.home) UI.home = {items:personaDefault(), edit:false, adding:false}; })();

// ================= 路由與動作 =================
function teamPage(r){
  const p = r.parts[0];
  if (p === 'brand') return pgBrand();
  if (p === 'forms') return pgForms();
  if (p === 'team') return pgTeam(r);
  if (p === 'triage') return pgTriage();
  if (p === 'plan') return pgPlan();
  return null;
}
function brandDraft(){ const B = brandState(); if (!B.draft) B.draft = Object.assign({}, B.pub.cfg); return B.draft; }
function actTeam(a, el){
  const d = el.dataset; const T = teamState();
  switch (a){
    // 品牌
    case 'br-swatch': { const x = brandDraft(); x.primary = d.p; x.secondary = d.s; render(); return true; }
    case 'br-sample': { const x = brandDraft(); const s = sanitizeSvg(SAMPLE_LOGO); x.logo = s.url; x.logoNote = `已消毒：移除 ${s.removed.map(esc).join('、')}`; render(); return true; }
    case 'br-nologo': { const x = brandDraft(); x.logo = null; x.logoNote = ''; render(); return true; }
    case 'br-preview': { const B = brandState(); B.preview = !B.preview; applyBrand(); render(); return true; }
    case 'br-discard': { const B = brandState(); B.draft = null; B.preview = false; applyBrand(); render(); return true; }
    case 'br-publish': { const B = brandState(); const v = {cfg:Object.assign({}, B.draft), v:B.pub.v + 1, at:clock(), by:ME}; B.versions.push(v); B.pub = v; B.draft = null; B.preview = false; try { localStorage.setItem('ox.brand', JSON.stringify(v)); } catch (e){} applyBrand(); toast(`已發布品牌 v${v.v}：所有人重新整理後生效`, 'ok'); render(); return true; }
    case 'br-rollback': { const B = brandState(); const old = B.versions.find(x => x.v === +d.v); const v = {cfg:Object.assign({}, old.cfg), v:B.pub.v + 1, at:clock(), by:ME}; B.versions.push(v); B.pub = v; B.draft = null; try { localStorage.setItem('ox.brand', JSON.stringify(v)); } catch (e){} applyBrand(); toast(`已用 v${old.v} 的內容發布成 v${v.v}`, 'ok'); render(); return true; }
    // 表單
    case 'fm-mv': { const F = formsState(); const i = +d.i, j = i + (+d.d); const fs = F.draft.fields; [fs[i], fs[j]] = [fs[j], fs[i]]; render(); return true; }
    case 'fm-discard': { const F = formsState(); F.draft = JSON.parse(JSON.stringify(F.pub)); F.report = null; render(); return true; }
    case 'fm-publish': {
      const F = formsState(); const errs = publishCheck(F);
      if (errs.length){ F.report = `<div class="banner b" style="display:block"><b>不能發布</b>${errs.map(e => `<div>• ${esc(e)}</div>`).join('')}</div>`; render(); return true; }
      const rulesDirty = rulesOf(F.draft) !== rulesOf(F.pub);
      if (rulesDirty && !d.ok){ F.report = `<div class="card" style="border-color:var(--warn)"><div class="k">影響報告（資料規則變更）</div><div>• 既有 3 張設計審查單不符新規則（缺必填欄位）</div><div>• 1 份待審提案會標成「規則已變，需要重新檢視」</div><div>• 2 筆 30 天內可回復的 agent 寫入：回復時若舊值違反新規則，會回 <span class="mono">undo_not_applicable（rules_changed）</span></div><div class="row" style="margin-top:6px"><button class="btn pri" data-a="fm-publish" data-ok="1">接受並標記，發布</button><button class="btn" data-a="fm-discard">取消</button></div></div>`; render(); return true; }
      const layoutDirty = layoutOf(F.draft) !== layoutOf(F.pub);
      const nv = JSON.parse(JSON.stringify(F.draft)); nv.rules_version = F.pub.rules_version + (rulesDirty ? 1 : 0); nv.layout_version = F.pub.layout_version + (layoutDirty ? 1 : 0);
      F.pub = nv; F.draft = JSON.parse(JSON.stringify(nv)); F.report = null; F.result = null; F.mcp = null;
      toast(`已發布：rules v${nv.rules_version}、layout v${nv.layout_version}${rulesDirty ? '' : '（只改版面，不影響 agent 寫入）'}`, 'ok'); render(); return true;
    }
    case 'fm-submit': { const F = formsState(); F.result = validateForm(F.draft, F.vals, F.role); render(); return true; }
    case 'fm-mcp': { const F = formsState(); const vals = Object.fromEntries(Object.entries(F.vals).filter(([k, v]) => v)); F.mcp = {vals, r:validateForm(F.draft, F.vals, F.role)}; render(); return true; }
    // AI 團隊
    case 'bot-pause': T.paused[d.id] = true; toast(`${botById(d.id).handle} 已暫停寫入；讀取照常`, 'ok'); render(); return true;
    case 'bot-resume': T.paused[d.id] = false; toast('已恢復'); render(); return true;
    case 'bot-req': { const v = ($('#bot-req').value || '').trim(); if (!v) return true; T.requests.push({bot:d.id, text:v, state:'sent'}); toast(`已送到 ${uname(botById(d.id).onBehalf)} 的「需要你決定」`, 'ok'); render(); return true; }
    case 'bot-run': {
      runOps([{kind:'add_note', target:'AUTH-110', text:'QA 回歸：中華電信門號 3/5 次仍收不到簡訊（staging，11-07 14:40）。建議退回處理中。'}], {actor:'ag-qa', onBehalf:'u1', via:'agent_direct', ctx:'ctx-team', title:'QA：AUTH-110'});
      T.pending.push({id:'tp' + Date.now(), bot:'ag-qa', text:'AUTH-110 狀態 待驗證 → 處理中（QA 退回）', key:'AUTH-110', to:'處理中', state:'proposed'});
      toast('QA 已留言（直接寫入）；退回狀態送你核准', 'ok'); render(); return true;
    }
    case 'bot-approve': { const p = T.pending.find(x => x.id === d.p); runOps([{kind:'transition', target:p.key, to:p.to}], {actor:p.bot, onBehalf:'u1', approver:'u1', via:'changeset', ctx:'ctx-team', where:' AI 團隊頁', title:'QA：' + p.key}); p.state = 'applied'; toast('已套用', 'ok'); render(); return true; }
    case 'bot-reject': { const p = T.pending.find(x => x.id === d.p); p.state = 'rejected'; render(); return true; }
    case 'chain-allow': T.chainAllow = el.checked; T.chain = null; render(); return true;
    case 'chain-sim': T.chain = T.chainAllow
      ? [{e:'e1 AUTH-127 priority → Urgent', o:'triage@carol', d:0, r:'寫入'}, {e:'e2 觸發 qa@alice 回歸', o:'triage@carol', d:1, r:'<span class="c-ok">觸發</span>（交接清單有列）'}, {e:'e3 qa@alice 留言', o:'triage@carol', d:1, r:'寫入'}, {e:'e4 e3 觸發 triage@carol', o:'triage@carol', d:2, r:'<span class="c-warn">抑制</span>（自我觸發／沒列交接）'}]
      : [{e:'e1 AUTH-127 priority → Urgent', o:'triage@carol', d:0, r:'寫入'}, {e:'e2 觸發 qa@alice 回歸', o:'triage@carol', d:1, r:'<span class="c-warn">抑制</span>：agent 寫入產生的事件預設不觸發 agent'}];
      render(); return true;
    // Triage
    case 'tri-run': {
      Object.entries(TRI_SUG).forEach(([k, s]) => { const is = D.I[k]; if (!is || is.assignee) return; const ops = []; if (s.f) ops.push({kind:'update_fields', target:k, fields:s.f}); if (s.dup) ops.push({kind:'duplicate_of', source:k, target:s.dup}); if (s.note) ops.push({kind:'add_note', target:k, text:s.note}); if (ops.length) runOps(ops, {actor:'ag-triage', onBehalf:'u4', via:'agent_direct', ctx:'ctx-team', title:'triage：' + k}); });
      T.triRun = true; toast('triage@carol 直接寫了 priority、severity、note 與重複標記；指派建議等 Carol 核准', 'ok'); render(); return true;
    }
    case 'tri-decline': runOps([{kind:'transition', target:d.k, to:'已拒絕'}], {actor:'u1', via:'human_direct'}); toast(`${d.k} 已拒絕`); render(); return true;
    case 'tri-snooze': T.snoozed[d.k] = true; toast(`${d.k} 明天再出現在收件匣`); render(); return true;
    // 交接
    case 'ho-take': T.handoff[d.k].state = '已接手'; D.I[d.k].events.push({at:clock(), actor:'u1', via:'human_direct', text:'接手交接（基準 ' + T.handoff[d.k].arts[0].now + '）'}); toast('已接手', 'ok'); render(); return true;
    case 'ho-back': T.handoff[d.k].state = '已退回'; D.I[d.k].events.push({at:clock(), actor:'u1', via:'human_direct', text:'退回交接：驗收條件需要補充'}); toast('已退回給 Erin'); render(); return true;
    case 'ho-sim': { const b = T.handoff[d.k].arts.find(a => a.kind === 'base'); b.now = 'v' + (+b.now.slice(1) + 1); render(); return true; }
    case 'ho-ack': { const b = T.handoff[d.k].arts.find(a => a.kind === 'base'); b.ver = b.now; b.hash = Math.random().toString(16).slice(2, 8) + '…'; D.I[d.k].events.push({at:clock(), actor:'u1', via:'human_direct', text:'交接基準改成 ' + b.now}); render(); return true; }
    // 心智圖
    case 'mm-create': { const r = runOps([{kind:'create_issue', project:'AUTH', tracker:'Feature', bind:'x', fields:{subject:'IdP 設定精靈', description:'從心智圖建議節點建立。'}}], {actor:'u1', via:'human_direct'}); T.mm.created = r.exec.created[0]; render(); return true; }
    // 介面角色
    case 'pz-toggle': { const i = T.persona.indexOf(d.r); if (i >= 0){ if (T.persona.length > 1) T.persona.splice(i, 1); } else T.persona.push(d.r); try { localStorage.setItem('ox.persona.alice', JSON.stringify(T.persona)); } catch (e){} T.personaAsk = true; render(); return true; }
    case 'pz-apply': { const H = homePrefs(); H.items = personaDefault(); saveHome(); T.personaAsk = null; toast('已套用角色預設首頁', 'ok'); render(); return true; }
    case 'pz-keep': T.personaAsk = null; render(); return true;
    case 'pz-exit': T.personaPreview = null; render(); return true;
  }
  return false;
}
document.addEventListener('change', e => {
  const t = e.target;
  if (t.dataset.br){
    const x = brandDraft();
    if (t.dataset.br === 'logo'){
      const f = t.files[0]; if (!f) return;
      const rd = new FileReader();
      if (f.type === 'image/svg+xml'){ rd.onload = () => { const s = sanitizeSvg(rd.result); if (s.err){ toast(s.err, 'bad'); return; } x.logo = s.url; x.logoNote = s.removed.length ? `已消毒：移除 ${s.removed.map(esc).join('、')}` : 'SVG 消毒後沒有需要移除的內容'; render(); }; rd.readAsText(f); }
      else if (f.type === 'image/png'){ rd.onload = () => { x.logo = rd.result; x.logoNote = 'PNG'; render(); }; rd.readAsDataURL(f); }
      else toast('只接受 PNG 或 SVG', 'bad');
      return;
    }
    x[t.dataset.br] = t.value; if (UI.brandS.preview) applyBrand(); render(); return;
  }
  if (t.dataset.fm){
    const F = formsState(); const f = F.draft.fields[+t.dataset.i];
    if (t.dataset.fm === 'group') f.group = t.value;
    else if (t.dataset.fm === 'show_if') f.show_if = t.checked ? {f:'review_type', eq:'無障礙'} : undefined;
    else if (t.dataset.fm === 'required') f.required = t.checked;
    else if (t.dataset.fm === 'required_if') f.required_if = t.checked ? {f:'review_type', eq:'無障礙'} : undefined;
    else if (t.dataset.fm === 'role') F.role = t.value;
    F.report = null; render(); return;
  }
  if (t.dataset.fv){ formsState().vals[t.dataset.fv] = t.value; render(); return; }
  if (t.dataset.botpol){ botById(t.dataset.id).pol[t.dataset.botpol] = t.value; toast('已更新政策（下一筆寫入生效）'); render(); return; }
  if (t.dataset.triAs && t.value){ runOps([{kind:'update_fields', target:t.dataset.triAs, fields:{assignee:t.value}}, {kind:'transition', target:t.dataset.triAs, to:'處理中'}], {actor:'u1', via:'human_direct'}); toast(`${t.dataset.triAs} 已接受並指派給 ${uname(t.value)}`, 'ok'); render(); return; }
  if (t.dataset.a === 'pz-preview'){ teamState().personaPreview = t.value || null; render(); return; }
});

// ================= 流程 =================
FLOWS.unshift(
  {id:'f20', t:'品牌：主副色、logo、主標題', ref:'研究 04 §2.1–2.3；U28–U30', steps:['左欄「管理 › 品牌」','改主標題、選一組主副色（試「黃＋深藍」看自動修正）','看對比報告：你給的色不夠對比時，實際使用的色與差異都列出來','「試一個含惡意內容的 SVG」→ 消毒後只留下圖形','「在整個介面預覽」（只有你看得到）→ 發布 → 版本表可回到舊版'], go:'/brand'},
  {id:'f21', t:'表單：設計審查（資料規則 vs 版面）', ref:'研究 04 §2.4–2.5；U31、U32', steps:['審查類型選「無障礙」→ 出現 WCAG 欄位且必填','留空按「在畫面送出」→ field_required','按「用 MCP 送同樣內容」→ 同樣的錯誤碼（同一個評估器）','把 WCAG 欄位改成「一律必填」但保留顯示條件 → 發布被擋','只調順序或分組 → 只升 layout_version；改必填 → 先看影響報告'], go:'/forms'},
  {id:'f22', t:'團隊流程：Triage → 設計交接 → QA', ref:'研究 04 §1；U25、U26', steps:['Triage 收件匣：模擬 Carol 交辦 triage@carol → 直接寫 priority、note、重複；指派等人','用「接受並指派…」或拒絕、稍後','開 AUTH-140：交接卡分「有版本基準」「僅有連結、未追蹤」','「模擬：Erin 上傳 v4」→ 提醒基準已變更；接手或退回重交','AI 團隊 › QA：讓 QA 檢查 AUTH-110 → 留言直接寫入、退回狀態送你核准'], go:'/triage'},
  {id:'f23', t:'介面角色與首頁範本', ref:'研究 04 §1.3；U24', steps:['首頁上方「介面角色」：加選 PM → 問你套用新預設或保留','「以角色預覽」設計師 → 看設計師的預設首頁（唯讀、仍依你的權限）','套用後「需要你決定」一定在','角色不影響權限：PM 看得到的不會因此變多'], go:'/home'},
  {id:'f24', t:'AI 團隊：角色、sponsor、權限、防迴圈', ref:'研究 04 §3；U33–U35', steps:['左欄「AI 團隊」：名單與人分開，成功率用 7 天回復率','點「分流」：triage@carol 和其他 Carol 的 bot 共用一台電腦 → 共用安全脈絡','你不是 sponsor：只能提出請求；完整設定只看到你有權的專案','你是 AUTH 管理員：可以暫停，但恢復只有 sponsor','QA（你是 sponsor）：允許清單可改；防迴圈模擬'], go:'/team'},
);

applyBrand();

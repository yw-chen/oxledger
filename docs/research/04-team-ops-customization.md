# 研究 04：團隊運作模式、高度客製化、AI 團隊

- 日期：2026-11-07
- 狀態：**5 輪完成**（第 1 輪三路平行研究；第 2–5 輪 GPT 獨立審查，第 5 輪 pass）；決定項 U24–U37 等使用者確認
- 視覺版：[04-team-ops-customization.html](04-team-ops-customization.html)
- 觸發：使用者 2026-11-07：「目前 bpmn 或心智圖這些會使用在哪裡？要從整個團隊的運作模式去設計介面的呈現與內容：1. 介面要高度可客製化，包含主副顏色、logo、系統主標題等；2. 頁面的內容或表單也要高度可客製化；3. 團隊運作模式，依照有 PM、設計師、工程師、以及類 grok bot 的 AI 團隊。先多輪研究，再修改介面。」

標記：**【事實】** 有來源；**【事實・廠商】** 廠商自述；**【推論】** 判斷，沒有直接來源。

---

## 0. 結論先講

1. **BPMN 和心智圖現在在哪裡**：都只在目標設計（設計 01 §14），第一切片和原型沒有。心智圖是「投影視圖」（M1 後續），BPMN 是「編寫型圖件」（M2）。產品文件顯示軟體團隊每天用的是收件匣、列表、看板；BPMN 有（偏舊的）證據顯示主要用在流程文件；**心智圖的使用頻率沒有調查可依**，「PM 探索期低頻工具」是推論。**使用者 2026-11-07 選 U27 (b)：依日常頻率排，推翻前端 Q11。**
2. **角色只決定預設，不決定權限或入口**。版面骨架所有人相同；角色（PM／設計師／工程師，可複選）只決定首次的首頁小工具、左欄捷徑、預設視圖；之後個人自訂。任何人都能「以某角色預覽」別人的版面。
3. **品牌客製只在站台層級，只開放 token 與資產，不開放自訂 CSS／JS**。輸入主色、副色（加中性色色調），server 端生成淺色和深色兩套色階，以 WCAG 2.2 門檻自動修正並列在影響報告。語意色（提案紫、成功、警告、危險）不跟品牌走。
4. **表單客製分「資料規則」和「呈現規則」兩層**。只有資料規則有強制力，而且對 UI、REST、MCP、匯入一體生效；MCP 加 `describe_form`、`validate`。
5. **AI 團隊＝一組角色化的 agent principal**，每個角色一個身分、綁一位人類 sponsor；名單和人分開列、活動混排；agent 寫入產生的事件預設不觸發其他 agent。

---

## 1. 團隊運作模式（第 1 輪 · 研究路線 A）

### 1.1 各角色的循環與常看畫面

| 角色 | 循環【推論，依據右欄】 | 最常看的畫面 | 依據【事實】 |
|---|---|---|---|
| PM | intake → 分流 → spec／拆解 → 排序、roadmap → 追健康度 → 對外更新 → 回顧 | Triage 收件匣、project 健康度彙總、roadmap、卡住／逾期例外 | Linear Triage（輪值、accept／decline／duplicate／snooze）https://linear.app/docs/triage ；Plane Intake https://docs.plane.so/core-concepts/intake ；Jira Product Discovery「ready for delivery」https://www.atlassian.com/software/jira/product-discovery/guides/delivery ；Linear 三級健康度 https://linear.app/docs/initiative-and-project-updates |
| 設計師 | 釐清問題 → 探索 → design review → 定稿拆任務 → 交接 → design QA | 我的設計任務、等我簽核、帶設計連結的 issue | Linear 設計流程 https://linear.app/method/manage-design-projects ；Jira 雙 board 陷阱 https://community.atlassian.com/t5/Jira-questions/How-to-manage-design-amp-development-boards-on-a-project/qaq-p/931275 ；design QA（實務意見）https://product.hubspot.com/blog/avoid-design-debt-with-better-design-qa |
| 工程師 | 接單 → 理解脈絡 → 開發 → PR／review → 部署 → 結單 | My issues、目前 cycle board、review 收件匣 | PR 事件自動轉 In Review https://linear.app/changelog/2019-08-25-github-pull-request-improvements ；cycle 範圍蔓延圖 https://linear.app/docs/cycle-graph ；Shortcut workflow https://www.shortcut.com/blog/how-and-why-to-customize-your-shortcut-workflows/ |

設計 01 §3 的使用者表**沒有設計師**，要補一列（U25）。

### 1.2 交接點

| 交接 | 工具上的呈現【事實】 |
|---|---|
| 外部 → 團隊 | Triage／Intake 收件匣 |
| PM → 設計／工程 | JPD idea 標 ready for delivery 後連 epic；Linear spec 掛在 project |
| 設計 → 工程 | Figma for Jira 追蹤「Ready for Dev」，Pro 版可**鎖定設計版本**避免實作途中被改 https://marketplace.atlassian.com/apps/1221981/figma-for-jira-pro |
| 工程 → QA／設計 | PR 事件轉狀態；design gap 改指派回設計師 |
| 人 → agent | Linear：指派給 agent 時填的是 `delegate`，人仍是負責人 https://linear.app/docs/agents-in-linear |

【推論】每個交接都是「狀態改變＋可引用的產物＋下一個負責人」。最大的失敗模式是交接後產物還在變。→ U25 交接事件化。

【第 2 輪修正】只存 URL＋時間偵測不到變更。交接卡分兩種狀態，**不能把「沒偵測到變更」顯示成「沒有變更」**：
- **有版本基準**：產物有版本 ID、快照或雜湊（例如 oxledger 內的附件、設計稿匯出檔、Figma 版本 ID）→ 之後變了就提醒接手人。
- **僅有連結、未追蹤變更**：只有 URL → 卡片明寫「未追蹤變更」，接手人可要求交出者補基準。

交接紀錄至少保存：產物與基準、交出者、接手者、驗收條件；支援「退回重交」。

### 1.3 角色導向介面：寫死還是自訂

【事實】Jira 首次登入問角色，只用來推薦範本，**換角色後不再提示** https://support.atlassian.com/jira-cloud-administration/docs/custom-onboarding/ ；Asana、Plane、OpenProject 的首頁是個人小工具；Linear 不分角色。NN/g：多數人不自訂，好的預設最重要 https://www.nngroup.com/articles/customization-of-uis-and-products/ 。一人多角的個案 https://www.katehuezo.com/blog/when-one-user-is-two-people 。

【推論】主流做法是「同一介面＋角色只影響首次預設＋個人自訂」。寫死角色會造成：一人多角卡住、換角色預設不更新、資訊孤島、角色被誤當權限。→ U24。

### 1.4 視圖：誰在什麼階段用（回答「BPMN、心智圖用在哪」）

| 視圖 | 主要使用者／階段【推論】 | 證據【事實】 |
|---|---|---|
| list／table | 所有人每天；PM 批次分流 | GitHub Projects table 定位為大量編輯 https://docs.github.com/en/issues/planning-and-tracking-with-projects/learning-about-projects/about-projects |
| board | 工程（cycle 中）、設計（設計 view） | Shortcut、Jira、GitHub board |
| inbox／triage | PM、輪值的人，每天 | Linear、Plane |
| roadmap／健康度 | PM 每週；對內排序、對外溝通 | Linear Roadmap、ProductPlan 調查 https://productplan.com/?p=17941 |
| Gantt | 跨團隊依賴、有合約日期的計畫經理 | Jira Plans 定位給 program manager https://www.atlassian.com/software/jira/guides/advanced-roadmaps/overview |
| mindmap／story map | PM 探索期拆需求，低頻 | Jira 沒內建，是 Marketplace 外掛 https://marketplace.atlassian.com/apps/881553806/mind-map-studio ；**找不到 PM 使用率調查** |
| BPMN | BA、營運的流程規範與文件，很少進 sprint | Recker 2010 https://emerald.com/insight/content/doi/10.1108/14637151011018001/full/html ；Ambysoft 2013 約 1/4 IT 人員沒聽過 BPMN https://ambysoft.com/surveys/modelingDocumentation2013.html （證據偏舊） |

**在 oxledger 團隊裡的具體位置【推論】**：

- **心智圖**：PM 在探索期把 epic 拆成 story（設計 01 sim 的 M 步），也是 AI 拆需求後給人「看全局」的視圖。放在專案的「規劃」分頁，不放首頁。
- **BPMN**：團隊自己的流程規範，例如「發布流程」「QA 驗證流程」「客戶回報處理流程」，放在專案的「文件／圖件」區；節點可連 issue。對 AI 團隊特別有用的一點：**AI 角色的交接規則（誰交給誰）可以用 BPMN 圖件畫成說明文件**，但執行仍以角色設定頁為準（不承諾 BPMN 即設定，同 §14.2）。
- **ontology graph**：找關聯、阻擋鏈，定位為「分析」，和 AI 找相關紀錄綁在一起，是差異化。

---

## 2. 高度客製化（第 1 輪 · 研究路線 B）

### 2.1 品牌主題：各家怎麼做【事實】

| 產品 | 能改什麼 | 層級 | 自訂 CSS／JS |
|---|---|---|---|
| Redmine | 主題目錄（CSS＋`theme.js`）、favicon | 站台 | **主題就能帶 JS** https://www.redmine.org/projects/redmine/wiki/HowTo_create_a_custom_Redmine_theme |
| OpenProject Enterprise | 3 套內建主題、主按鈕／accent／頂欄等 hex、桌面與行動 logo、favicon、PDF 樣式與字型＋demo 預覽；按鈕太淺時文字自動改黑 | 站台 | 無 https://www.openproject.org/docs/system-admin-guide/design/ |
| GitLab | logo、favicon、site name、頁首頁尾橫幅、登入頁 Markdown、PWA；SVG logo 在 email 會退回預設 | 站台＋個人導覽主題 | 無 https://docs.gitlab.com/administration/appearance/ |
| Jira Cloud | logo、站名、favicon；顏色選項很少 | 站台 | 無 |
| Plane | 個人主題：5 個 hex 或系統／淺／深／高對比 | 個人 | 無 https://docs.plane.so/core-concepts/account/settings |
| Linear | 98 個變數縮成 base、accent、contrast 3 個輸入，LCH 生成 | 個人 | 無 https://linear.app/blog/how-we-redesigned-the-linear-ui |
| Discourse | 主題含 SCSS／HTML／JS；顏色走 CSS 變數，淺深兩套調色盤 | 站台＋個人選調色盤 | **完整 JS**，靠 CSP 與 repo 允許清單 |

Discourse 的「3 色生成整套」被回報難以預測、有對比問題 https://meta.discourse.org/t/it-is-hard-to-make-a-good-palette/253284 。

### 2.2 色階與對比

- Material Color Utilities：HCT 從種子色生 tonal palette，`contrastLevel` −1～1 https://github.com/material-foundation/material-color-utilities/blob/main/make_schemes.md
- Radix Colors：12 階、各階有固定用途 https://www.radix-ui.com/colors/docs/overview/custom-palettes
- WCAG 3 仍是草案、對比演算法未定；合規依據仍是 WCAG 2.2（文字 4.5:1、UI 元件 3:1）https://adrianroselli.com/2026/04/wcag3-contrast-as-of-april-2026.html
- 【推論】server 端以 OKLCH 生成 12 階 × 淺深兩套；WCAG 2.2 是硬門檻，不合格沿 L 軸自動修正，影響報告寫「你給 #X，實際用 #Y」。
- 【第 2 輪修正】對比的驗收單位是**實際的前景／背景配對**，不是色階本身：定義 semantic token 配對與元件測試矩陣（淺／深／高對比 × 一般、hover、selected、focus、錯誤、提案狀態），影響報告列出無法滿足的配對，不籠統宣稱「整套合格」。WCAG 1.4.1：資訊不能只靠顏色，所以**提案的辨識靠固定文字標記、圖示、虛線邊框與 diff 結構**，不靠紫色；品牌主色是紫色時只提示，不擋發布。https://www.w3.org/WAI/WCAG22/Understanding/non-text-contrast.html

### 2.3 安全

- SVG 上傳 XSS：Discourse CVE-2026-53962、CVE-2023-30538；TypiCMS CVE-2026-27621。OWASP：允許清單消毒或點陣化、`nosniff`、sandbox CSP https://cheatsheetseries.owasp.org/cheatsheets/File_Upload_Cheat_Sheet.html
- 自訂 CSS：OWASP WSTG 列為高風險，不用 JS 也能用屬性選擇器外洩 token https://owasp.org/www-project-web-security-testing-guide/stable/4-Web_Application_Security_Testing/11-Client-side_Testing/05-Testing_for_CSS_Injection
- 【推論・oxledger 特有】自訂 CSS 能藏起權限提示、ChangeSet diff、agent 標記，破壞設計 01 §7「提案看得出來」；headless 截圖給 agent 時也會把竄改後的畫面當真相。

### 2.4 表單與頁面：各家怎麼做【事實】

- Jira：field configuration → scheme → screen → screen scheme → issue type screen scheme，6 層 https://confluence.atlassian.com/adminjiraserver/project-screens-schemes-and-fields-938847220.html ；Cloud issue layout 可**個人釘選欄位** https://support.atlassian.com/jira-cloud-administration/docs/configure-issue-layout/
- **規則只在 UI 生效的教訓**：ScriptRunner Behaviours 不套用到 REST https://docs.adaptavist.com/sr4js/latest/features/behaviours/behaviour-limitations ；JSM 透過 API 可建立必填留空的單 https://jira.atlassian.com/browse/JSDCLOUD-14869 ；ServiceNow 分 UI Policy 與 Data Policy（後者管所有寫入）https://www.servicenow.com/docs/r/australia/platform-administration/c_DataPolicy.html ；Salesforce 頁面可見性規則不等於資料保護
- OpenProject：每個 type 的表單分組，**API schema 用 `_attributeGroups` 反映分組** https://www.openproject.org/docs/development/concepts/resource-schemas/
- Redmine：workflow 欄位權限（tracker × status × role 唯讀／必填），多角色時要全部唯讀才算唯讀 https://redmine.org/issues/11887
- Linear 表單範本、GitHub issue forms（YAML，沒有條件邏輯）、Notion 頁面版面、Airtable interface（有越權編輯的回報）
- 代價：Jira 自訂欄位 800 以下理想、1,200 以上超標 https://confluence.atlassian.com/enterprise/managing-custom-fields-in-jira-effectively-945523781.html ；Salesforce 四十個 layout 的遷移地獄

### 2.5 層級模型【推論】

| 層 | 內容 | 誰能改 | 強制力 | 版本化 |
|---|---|---|---|---|
| L0 欄位定義 | 型別、合法值、適用範圍、可見性（已有 E9／D14） | 站台管理員 | **server 強制**：讀取、寫入、`describe_form`、`validate`、錯誤訊息都套可見性（U2：讀不到不能寫） | `rules_version`；§5.7 草稿 → 影響報告 → 模擬 → 發布 |
| L1 資料規則 | tracker × status × **授權角色**的必填、唯讀，有限的 `required_if`、`allowed_values_if` | 站台管理員；專案只能收緊 | **server 對 UI、REST、MCP、匯入一體強制** | 同上（和 L0 共用 `rules_version`） |
| L2 FormLayout | 欄位、分組、順序、說明、頁籤、多欄、欄位 renderer、`show_if`、建立／編輯兩模式 | 站台；專案「選用或整份覆寫」，不 deep merge | 只影響呈現；`show_if` 不能藏 L1 必填欄位 | `layout_version`；草稿 → 預覽 → 發布 |
| L3 PageLayout | issue 頁區塊、專案總覽小工具、角色首頁**範本** | 站台預設，專案管理員可調 | 呈現；**不能移除保護清單** | 同上 |
| L4 個人 | 自己的首頁、釘選欄位、收合、密度、淺／深／高對比 | 個人 | 只有自己；**首頁屬於個人**，站台／專案只提供可套用的範本，改範本不覆寫既有個人首頁 | 不版本化，可重設 |
| B 品牌 | token、資產、文字 | 站台管理員 | 呈現 | 草稿 → 預覽 → 發布（不需模擬） |

**角色名詞分清楚**：U24 的「介面角色」（PM／設計師／工程師）只影響預設；L1 的「授權角色」是 Redmine 的 role。多個授權角色合併照 Redmine（全部唯讀才唯讀，任一必填即必填）；agent 承接 sponsor 的授權角色與 L1 規則。

**保護清單（L2、L3、L4、範本、匯入都不能移除或隱藏）**：「需要你決定」（U22）、agent 身分標示、核准 diff、權限／失敗提示、待審與暫停狀態、提案色與文字標記。重設首頁或重套範本後這些仍在。

**規則版本與 D20 生命週期**（第 2 輪新增）：
- 所有提交、核准後套用都在交易內用**當下**的 `rules_version` 重新驗證；送審後規則變了、使已審內容不再合法時，提案標成「規則已變，需要重新檢視」，不靜默套用也不靜默丟掉。
- **undo 例外**：回復 30 天內 agent 寫入時，若舊值已被新規則禁止（例如合法值清單移除了 A），undo 回 `undo_not_applicable`（reason `rules_changed`），並在預覽列出；不繞過 L1。
- 發布影響報告要列：既有資料不合新規則的筆數、受影響的待審提案、受影響的可回復紀錄；管理員選「擋下發布／一併遷移／接受並標記」。
- `form_version` 拆成 `rules_version`（合約，寫入可帶，不符回 `form_rules_changed`）和 `layout_version`（只是提示，不擋寫入）。

**不開 CSS 也能「高度客製」的能力清單**：主副色與中性色調、圓角與密度、logo／favicon／站名／登入頁／公告、頁籤、多欄與欄寬、分組與說明區塊、欄位 renderer（例如連結欄位顯示成設計預覽卡）、表單範本、條件顯示與條件必填、頁面小工具、專案總覽版面、個人首頁與視圖。

條件語言上限：只參考同一張單的欄位；`eq`、`in`、`is_set`、`not_set` 加數字日期比較；最多兩層 AND/OR、每條 5 個條件；不能腳本；發布時檢查環。評估器一份（Rust，編 WASM 給前端），或兩份共用 conformance 測試向量。

---

## 3. AI 團隊（第 1 輪 · 研究路線 C）

### 3.1 Grok Bot 現況與 Aura 研究 30 的差異

【事實・廠商】2026-08-11 early beta；每個 bot 是常駐具名隊友；內建 8 種角色範本（含 Bug Reproduction、Product Performance、Chief of Staff），**沒有經理 bot 類型**；2–6 個 bot 可進群組聊天互相交接；沒有 dry-run。https://x.ai/news/introducing-grok-bot 、https://www.eesel.ai/blog/grok-bot

【事實・廠商，第 2 輪查證 docs.x.ai】核准與安全邊界分四層，不只是「寫在指示裡」：
- **共用電腦**：同一使用者的所有 bot 共用一台雲端電腦，檔案、瀏覽器 session、命令列憑證都共用；官方明說「不要把不同 bot 當安全邊界」，刪 bot 不會清掉共用電腦上的東西。https://docs.x.ai/grok-bot/approvals-security-and-privacy
- **產品核准卡**：Auto Review 預設開啟，每個動作前檢查，需要時出核准卡（Allow once／Always allow／Deny）；Ask first 規則優先於 Allow automatically。Auto Review 是模型式審查，不等於確定性的授權政策。
- **管理員強制**：管理員可把設定設成「Required by your admin」；團隊規則一律套用，個人規則只能更嚴。
- **Shared Bots**：團隊共用的 bot，擁有者與 Manager 能改指示、skills、記憶、apps，只有擁有者能刪；擁有者加的 apps 大家共用，但**個人連線只代表正在被回覆的那個人**，用之前先問；核准卡**只有提問的人能按**；用量算在提問者頭上。https://docs.x.ai/grok-bot/shared-bots （頁面沒寫推出日期）

和 Aura 研究 30 的差異：方案已擴大（SuperGrok、Cursor 方案都含）；共用電腦已由官方確認；多了 Auto Review、管理員強制、Shared Bots；Slack／PR 監看的明文沒找到。GPT 提到的「Audit Logs 與 Bot Action Recording（預設關、保留 90 天）」**在上述官方頁沒查到，未採用**。

**對 oxledger 的意義【推論】**：Grok Bot 的角色 bot 共用一台電腦與憑證，所以 oxledger 不能假設「不同 principal＝不同資料脈絡」（見 §3.5）。Shared Bots 的「觸發者是誰、就用誰的連線、由誰核准」是值得照抄的模型。

### 3.2 其他產品的 AI 隊友【事實】

| 產品 | 形態 | 監督 |
|---|---|---|
| Linear | agent 是 app actor，可 @、可委派，**不能當成員或 lead**；委派填 `delegate`，人仍是 assignee；Agent Session 6 種狀態；活動分 thought／action／elicitation／response／error；最佳實務：自動化委派的單留在 triage，指派交給人 | `app:assignable`／`app:mentionable` scope；「你仍然要負責」 https://linear.app/developers/agent-interaction 、https://linear.app/developers/agent-best-practices |
| Jira Rovo | agent 在 assignee 下拉和人並列；work item 有 Agents 區塊；可掛在 workflow transition 自動跑；輸出只有觸發者看得到，按 Draft comment 才分享 | Rovo credits 80%／100% 通知 https://support.atlassian.com/jira-software-cloud/docs/collaborate-on-work-items-with-ai-agents/ |
| GitHub Copilot | 指派 issue；commit 作者是 Copilot、指派者 co-author；即時 session log、Stop；`.github/agents/*.md` 自訂 agent | **請求者也不能核准它的 PR**；CI 要人按 Approve and run https://docs.github.com/en/copilot/concepts/security-governance-and-network-settings/risks-and-mitigations |
| Asana AI Teammates | 像成員被指派；30+ 預建角色（PM、Work Intake…）；設定＝角色、guidance、skills、存取權；checkpoints | 權限永遠受觸發者限制 https://help.asana.com/s/article/understanding-access-control-for-ai-teammates |
| Notion Custom Agents | 排程或事件觸發；每次執行有紀錄 | credit 用完自動暫停，**不補跑** https://www.notion.com/blog/introducing-custom-agents |
| Claude Tag（Slack） | 頻道裡用服務帳號，一對一 DM 才用交辦者本人帳號；以交辦者為準，其他人的貼文當任務資訊；**其他人在 thread 提出的請求用「頻道自己的存取權」執行，不用交辦者的個人連線**；「能做什麼不會因為誰叫它而改變」 | 交辦者個人連線只用在他自己交辦的任務，先問、記在他名下 https://claude.com/docs/claude-tag/concepts/agent-identity （第 2 輪查證修正） |
| Microsoft Entra Agent ID | agent 建立時**必須指定人類 sponsor**，sponsor 離職自動轉主管 | https://learn.microsoft.com/en-us/training/modules/establish-agent-identities-entra-agent-id/1-introduction |
| 迴圈先例 | GitHub Actions：`GITHUB_TOKEN` 觸發的事件不會再建立 workflow run | https://docs.github.com/en/actions/concepts/security/github_token |

本輪**沒找到一般 PM 工作項目上、具獨立審查者與強制核准語意的完整「AI 互審」產品模型**；程式碼領域則有先例：GitHub Copilot cloud agent 開的 PR 可由 Copilot code review 審查（官方用量指標有這一項）。Grok Bot 有 bot 群組聊天。**AI 審查意見不等於人類核准**。

### 3.3 在 D20 下的建模【推論】

- **每個角色一個 agent principal**（例如 `triage@pm-lead〔agent〕`），另存角色設定物件。理由：D20 的配額、熔斷、暫停、事故回復都綁 principal；一個 agent 兼多角色時，暫停會連帶停掉沒出事的角色。
- **第一版走 A 路線**：角色 bot 綁一位 sponsor（委派者），權限＝sponsor ∩ 政策，不改 D20 的權限模型。sponsor 權限變動時 bot 停（U1）；sponsor 離職要有接手流程。B 路線（團隊服務身分，權限由管理員設）會推翻「agent 沒有自己的角色」，另開決策。
- **執行者**：第一切片的角色 bot 都是外部 agent（Claude Code routine、Grok Bot 等）；內建 oxledger-ai 角色化要觸發引擎＋前端 Q2 開啟，延後。

### 3.4 角色範本（建議的操作表預設）

**範本是完整允許清單**（第 2 輪修正）：先全部 off，只開表內列出的；未列出的命令、欄位與**之後新增的能力一律 off**。這只是角色 bot 的起始設定；個人 coding agent 仍照 D20「全 direct」。建立時顯示最終政策全表，由 sponsor 確認；範本之後改版，**不會靜默放寬既有角色 bot**（只提示「範本有新版，要不要套用」）。建立 issue 的初始欄位照 U16 套同一政策：範本要**明列建單命令與每個初始欄位**，沒列的初始欄位不能由 agent 提供、改用 server 預設；整份建單只要任一項需核准，就整份走同一個待審提案。範本發布時檢查「存在合法的最小建單內容」（例如 subject 必須可寫），不合格就擋。

| 角色 bot | 職責 | 直接寫入 | 需要核准 | 其他（off） |
|---|---|---|---|---|
| triage | 補標籤、priority、標 duplicate、建議指派 | 標籤、priority、duplicate、note | 指派 | 其餘全部 |
| PM 助理 | 起草描述與驗收條件、週報 | note | description；建立 issue（含初始欄位 subject、description、tracker、parent） | 其餘全部（其他初始欄位用 server 預設） |
| 設計審查 | 對照設計規範留意見 | note | — | 其餘全部 |
| QA | 「已解決」後跑檢查並留言 | note | 轉狀態（只限退回） | 其餘全部 |
| 發布管家 | 整理 release notes | note | 版本欄位（待 U12） | 其餘全部 |
| 個人 coding agent（`claude-code@alice`） | D20 原設計 | D20 預設全 direct | 依委派者設定 | 依委派者設定 |

### 3.5 共用脈絡、權限與防迴圈（第 2 輪新增）

**(1) principal ≠ 安全脈絡**。把四件事分開建模：
- **principal**：顯示身分、配額、熔斷、暫停、回復的單位（每角色一個）。
- **委派**：sponsor 和政策。
- **security context**：U14–U15 的「曾授予來源集合」與失效條件。
- **runtime**：實際執行的環境（外部 agent 的電腦、記憶、憑證）。

規則：同一個 runtime（例如同一使用者的 Grok Bot 共用電腦，或同一台機器上的 Claude Code）裡的角色 bot **共用一個 security context**，來源集合取聯集；換 principal 不能重設來源。交接（A 交給 B）時，B 繼承 A 的來源集合。無法證明隔離時就視為共用。**sponsor 換人不清除來源歷史**（第 3 輪修正）：換人期間停用舊委派與未完成工作；沿用同一個 runtime（電腦、記憶、檔案）時，新委派繼承來源集合，並依新 sponsor 的權限重新驗證；只有換成隔離且沒有舊資料的新 runtime，或完成 U14 的清除宣告與重新驗證，才能從空來源開始。→ 代價：窄受眾的 bot 和寬受眾的 bot 放在同一台電腦，寬的會被拖窄（等於 U1 的受眾一致規則擴大到 runtime）。

**(2) 五種權限**（第一版預設）：

| 權限 | 誰有 |
|---|---|
| 查看名單與安全狀態（角色、sponsor、active／paused、待審數） | 專案成員 |
| 查看活動、diff、session、佇列與統計 | 只看**觀看者本來就看得到**的專案與欄位（逐筆過濾，不因查看 bot 而擴權） |
| 查看完整指示與設定 | sponsor；專案管理員**只看自己有權管理且有權讀取的專案那一部分**（bot 同時服務 A、B 時，只管 A 的管理員看不到 B 的設定與跨專案共用的指示）；其他成員只看安全摘要（職責、允許清單）。驗收案例：A 的管理員無 B 權限、bot 同時服務 A／B |
| 觸發（交辦工作） | **只有 sponsor**；其他人只能「提出請求」，進 sponsor 的「需要你決定」由 sponsor 接受 |
| 編輯指示、政策、交接對象 | 只有 sponsor（政策只能收緊的部分，專案管理員也可） |
| 核准待審 | 只有 sponsor（U6 未放寬前不開放同專案互審） |
| 暫停 | sponsor、專案管理員、站台管理員（停比開容易） |

U19 未放寬前，原型不出現可操作的「@ agent」「指派給 agent」。

**(3) 防迴圈契約**：
- 每個事件由 server 指派並保存 `root_event_id`、`parent_event_id`、`origin_actor`、執行 principal、`depth`；重試、核准後套用、交接都**不洗掉來源**（人核准 agent 提案後套用的寫入，origin 仍是 agent）。
- 預設抑制**所有** agent 衍生事件觸發 agent，包含自我觸發；只有角色設定明列的交接例外；depth 上限 2。
- 加事件去重、每條因果鏈的觸發數與寫入數上限（建議各 20）。
- 外部 agent 自己在 oxledger 外面串連的部分無法追蹤：第一版**只保證站內事件不會自動連鎖**，在畫面上明講。

---

## 4. 建議的決定項（U24–U37，等使用者確認）

| # | 決定 | 建議預設 | 取捨 |
|---|---|---|---|
| U24 | 角色的作用 | 介面角色（PM／設計師／工程師，可複選）只決定首次首頁、左欄捷徑、預設視圖、右欄優先內容；不影響權限；可隨時重套（提示「套用新預設／保留我的」，保護清單一定保留）；任何人可「以某角色預覽」 | 多一個 onboarding 步驟 |
| U25 | 設計師與交接 | 01 §3 補設計師；交接卡記產物、基準、交出者、接手者、驗收條件；分「有版本基準」和「僅有連結、未追蹤變更」兩種狀態；可退回重交 | 外部設計工具的版本要整合，第一版多半是「未追蹤」 |
| U26 | Triage 收件匣 | 一級畫面，動作 accept／decline／duplicate／snooze，負責人第一版手動指定 | 輪值排班延後 |
| U27 | 視圖優先順序 | **已決定 (b)（使用者 2026-11-07）**。原本兩個選項：(a) 維持前端 Q11：mindmap／ontology → BPMN → Gantt；(b) 依日常頻率：list／board／triage → roadmap＋健康度 → Gantt → mindmap → BPMN。不論選哪個，原型都保留一條輕量的「心智圖拆需求 → 建 issue」示範 | 心智圖頻率沒有調查可依；(b) 降低 AI-native 展示性 |
| U28 | 品牌層級與邊界 | 只做站台品牌；專案只能改專案色與圖示；**不開放自訂 CSS／JS**；改用 §2.5 末的能力清單達成高度客製 | 失去 Redmine 主題生態相容 |
| U29 | 顏色生成 | 主色＋副色＋中性色色調 → OKLCH 12 階 × 淺深兩套；以實際配對矩陣驗 WCAG 2.2、自動修正並列報告；語意色固定；提案辨識靠文字與結構，品牌紫只提示不擋 | 管理員看到的色可能和輸入不同 |
| U30 | 資產與發布 | logo 淺色版必填、深色版選填；SVG 消毒、只以 `<img>` 顯示；email／PDF 用點陣 PNG；品牌走草稿 → 預覽（淺深、登入頁、email、PDF）→ 發布，可回溯 | 多一個點陣化元件 |
| U31 | 表單層級 | L0–L4（§2.5）；**L0 與 L1 由 server 強制、全通道一致**，L2–L4 只管呈現；保護清單不可移除；首頁屬於個人；條件語言有上限 | 做不出任意腳本邏輯 |
| U32 | MCP 與表單 | 加 `describe_form`、`validate`；`rules_version`（合約）與 `layout_version`（提示）分開；送審與套用都用當下規則重驗；undo 遇新規則回 `rules_changed`；發布影響報告含待審與可回復紀錄 | 多兩個工具、兩個版本號 |
| U33 | AI 角色建模 | 每角色一個 principal＋sponsor（A 路線）；principal、委派、security context、runtime 分開，同 runtime 共用來源集合；角色範本是完整允許清單（未列出＝off，範本改版不靜默放寬）；觸發、設定、核准第一版只給 sponsor；成員看名單與安全摘要，活動逐筆依觀看者可見範圍過濾；專案管理員只看自己專案的設定，跨專案共用的指示只有 sponsor 看得到；暫停也開放給專案與站台管理員；換 sponsor 不清除來源歷史（§3.5） | principal 變多；同電腦的 bot 會互相拖窄受眾 |
| U34 | 防迴圈 | server 保存因果鏈（root、parent、origin、depth），重試／核准套用／交接不洗來源；所有 agent 衍生事件（含自我觸發）預設不觸發 agent；明列交接例外；depth 2；每條鏈觸發與寫入各 20 上限；只保證站內 | 自動化連鎖要逐條設 |
| U35 | AI 團隊頁 | 左欄「AI 團隊」和「成員」分開；名單欄位：角色、sponsor、狀態、佇列、今日寫入、待審、被擋、7 天 undo 率；成功率只用可驗證指標；**issue 時間線保留所有實際變更、核准、undo、交接、重要失敗**（每筆有 agent、sponsor、direct／review、diff、回復入口），只把逐字對話與內部執行細節收到 session 詳情 | 指標間接，要說明文字 |
| U36 | 負責人與執行者 | 放寬 U19 時加 `delegate` 欄位，`assigned_to_id` 永遠是人 | 多一個欄位 |
| U37 | 內建 AI 角色化 | 延到前端 Q2 開啟後；附預算、80%／100% 通知、用完暫停不補跑 | 第一版角色 bot 都靠外部 agent |

和既有決定的關係：U24 延伸 U22（首頁固定「需要你決定」）；U33 修改 D20 的預設行為（不改權限模型）；U27 推翻前端 Q11 的建議預設；U36 依賴 U19 放寬；U37 依賴前端 Q2。

---

## 附錄 A：研究過程紀錄

### 第 1 輪（2026-11-07，三路平行 subagent）

| 路線 | 問題 | 查詢數 | 主要來源 | 死路 |
|---|---|---|---|---|
| A 團隊運作 | 各角色循環、交接、角色導向介面、各視圖誰在用 | 26 | Linear docs／method、Plane、JPD、Figma for Jira、NN/g、Recker 2010、Ambysoft 2013 | Height 已停止服務；Jira 依角色首頁只有功能請求；PM 用 mindmap 的調查找不到；BPMN 在敏捷團隊的使用率只有間接證據 |
| B 客製化 | 品牌主題、色階與對比、安全、表單層級、反模式 | 35 | OpenProject design、GitLab appearance、Discourse、Material、Radix、OWASP、Jira／ServiceNow／Salesforce／OpenProject／Redmine 表單文件 | Linear 主題文件 404；GitHub issue forms 的 API 驗證沒寫；Salesforce Dynamic Forms 是否套 API 沒官方說法 |
| C AI 團隊 | Grok Bot 現況、AI 隊友產品、資訊架構、D20 建模 | 13＋直接讀頁 | x.ai news、Linear developers、Atlassian、GitHub docs、Asana、Notion、Claude Tag、Entra | Asana help 正文被截斷；Grok Bot 電腦共用或各自一台來源矛盾；主流產品沒有 AI 互審 |

可信度提醒：多數來源是廠商自述；學術證據偏舊；所有建議都還沒經過真實使用者驗證。

### 第 2 輪

見附錄 B。

---

## 附錄 B：獨立審查

### 第 2 輪（GPT gpt-6.1-sol，thread `oxledger-r04-team-ops`，effort high，可上網）

結論 **fail**，13 條（10 必改、3 建議），全部採納：

| # | 發現 | 處理 |
|---|---|---|
| 1 | Grok Bot 核准／安全邊界過時 | 查證 docs.x.ai 兩頁後改寫 §3.1；GPT 說的 Audit Logs／Bot Action Recording 在官方頁查不到，**不採用** |
| 2 | Claude Tag 指令來源寫錯；AI 互審否定太廣 | 查證後改寫 §3.2 |
| 3 | principal 沒處理共用記憶／runtime 的受眾繞道 | §3.5(1) |
| 4 | 沒定義誰能觸發、設定、核准團隊 bot | §3.5(2) |
| 5 | 防迴圈不可執行（自我觸發、來源洗掉、分支） | §3.5(3)、U34 |
| 6 | 範本以全 direct 為底，未列能力仍是 direct | §3.4 改完整允許清單 |
| 7 | 「只有 L1 有強制力」和 L0 衝突 | §2.5、U31 |
| 8 | 規則發布和待審、undo 生命週期沒對齊 | §2.5「規則版本與 D20 生命週期」、U32 |
| 9 | PageLayout 沒有保護清單、首頁所有權不清 | §2.5 保護清單、L4 |
| 10 | 「時間線只放交接紀錄」會藏掉 agent 實際寫入 | U35 |
| 11 | 只存 URL 偵測不到產物變更 | §1.2、U25 |
| 12 | 對比驗收單位未定義；色相排斥不可靠 | §2.2、U29 |
| 13 | 高度客製沒變成可驗收流程；心智圖低頻是推論 | §0、§2.5 能力清單、U27 改兩選項；原型優先流程見 §5 |

### 第 3 輪（同 thread）

結論 **fail**：13 條中 10 條 resolved、3 條 partial。GPT 撤回第 2 輪「Audit Logs／Bot Action Recording 已查證」的說法。剩下 3 條必改，全部採納：
1. sponsor 換人會清掉來源歷史 → §3.5(1) 改成不清除，只有隔離新 runtime 或 U14 清除宣告才從空來源開始。
2. 查看權過廣、U33 摘要和權限表矛盾 → 權限表拆三種查看，活動逐筆依觀看者可見範圍過濾；U33 同步。
3. PM 助理範本缺建單必要欄位 → §3.4 明列初始欄位，發布時檢查最小合法建單。

### 第 4 輪（同 thread）

結論 **fail**：第 3 輪 #1、#3 resolved；#2 partial：專案管理員查看「完整指示與設定」沒有跨專案範圍限制。已修正：專案管理員只看自己有權管理且讀取的專案那一部分，跨專案共用的指示只有 sponsor 看得到；U33 同步；加驗收案例。

### 第 5 輪

見下方。

---

## 5. 原型要先展示的流程（依第 2 輪建議）

1. **品牌**：管理員改主標題、logo、主副色 → 預覽（淺深、對比報告）→ 發布。
2. **表單**：不寫程式為「設計審查」建表單，設條件必填 → UI 送出與 MCP `validate` 得到同樣結果。
3. **團隊流程**：PM 分流（triage）→ 設計交接（有基準／未追蹤）→ 工程 → QA bot 留言 → triage bot 寫入、待審、暫停與回復。
4. **角色首頁**：選介面角色 → 套用範本 → 「以某角色預覽」→ 重設後保護清單仍在。
5. **AI 團隊頁**：名單、角色設定（允許清單）、sponsor、交接對象、只有 sponsor 能觸發。
6. **心智圖輕量示範**：探索期把 epic 拆成 story，建議節點經核准才建 issue。

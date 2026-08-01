# agent-bridge TUI dashboard 設計正本（`agent-bridge ui`）

- 狀態：**待定案**（使用者確認前不動 `crates/`／`tests/`）
- 依據：五輪三廠討論收斂 ＋ `docs/herdr-probe.md` 實測 ＋ 定案前跨廠終審一輪
  （codex `20260731T111221Z-e536`：1 blocker＋7 major，經本棒逐條對碼驗證後採納；
  agy `20260731T111222Z-ba61`：零 finding、接受雙軸裁定——其對 §5「單純曝露」的
  背書被 codex 的程式碼證據反證，不採）
- 使用者拍板句：「TUI 做 dashboard 使用，原先協定功能仍不變，
  且依賴相對 herdr 或其他競品低」

---

## 1. 目標與非目標

**目標**：一個給人看、給人介入的 dashboard——回答「誰在替誰做什麼、
哪裡卡住了、我現在要不要出手」。

**非目標**（越線即設計錯誤）：

- 不是新的協定層：send／receive／reply／relay／evict **既有 invocation 的語意零改變**；
  唯一允許的協定側動作是 additive 參數（見 §5 CLI-EVICT-4），不帶新參數時行為與現行完全相同
- 不是第二套 multiplexer：不管 pane 佈局、不代管 PTY
- 不上 daemon、不開 socket、不做遠端：狀態來源就是磁碟上的 task-plane
- 不做自動清理／自動決策：TUI 呈現證據，人下判斷

## 2. 形狀

`agent-bridge ui` ＝ **PTY 無關的 alternate-screen 全螢幕 TUI**（lazygit 形），
落在既有佔位 crate `crates/ab-tui`。

- **部署形態（P0 後補裁定）**：`ab-tui` 是 lib crate，`ab` 新增 `ui` 子指令
  呼叫 `ab_tui::run()`——部署邊界是 `cp target/release/ab bin/ab` 單一 binary，
  第二個 binary 會在部署時失蹤。

- `tmux display-popup -E 'agent-bridge ui'` 只是 binding 層的召喚方式，
  **程式不知道自己在不在 popup 裡**。
- 唯一需要模式感知的是 `Enter` focus（popup 下＝關 popup ＋ select-pane），
  那屬啟動器協定（binding 傳個 flag／env），不進核心。
- 退出後 tmux 的 pane／window id、layout 與 geometry 必須不變（見 P1 gate；
  「畫面 byte 級不變」不是 alternate screen 可承諾的不變量——shell 會留下
  啟動命令與新 prompt）。

### 版面：四面板，以 current owner 為根

```
┌ OWNERS ─────────┬ WORKERS ──────────────────────┬ DETAIL ─────────────────────────┐
│ ▸ ab-tui-lead ● │ ▸ plan-review   codex  ⚙ run  │ name  : plan-review             │
│   other-owner   │ │  └ 0731T105646-f006 review… │ pane  : %105   gen: t1753…      │
│ ✗ dead-owner    │ ▸ agy-opinion   agy    · idle │ task  : …-f006  running 3m12s   │
│                 │ ▸ builder-2     claude ⚙ run⛔ │ blocker: none                   │
│                 ├ TASKS ────────────────────────┤ evidence:                       │
│                 │ ⚙ …-f006 plan-review running  │   $ agent-bridge read …-f006    │
│                 │ ✓ …-bc8d agy-opinion completed│   $ agent-bridge status …-f006  │
│                 │ ⚙ …-52c9 builder-2  running ⛔ │                                 │
└─────────────────┴───────────────────────────────┴─────────────────────────────────┘
 Tab/j/k 導航 · Enter focus · r read · i explain · c 複製證據 · n spawn · s send
 x cancel · e evict · ? 合法鍵 · q 離開                        [poll 500ms]
```

- 展開軸：`owner → worker → in-flight task ＋ visible blocker`。
  使用者的原始痛點是**關聯可見性**（誰派給誰、卡在哪），不是列表效率。
- **task status 一律顯示權威字**（`queued/delivered/running/completed/failed/cancelled`，
  見 `spec/state.md`／`task.rs`）；blocker 是**另一軸的 glyph／欄位**（如 `running ⛔`），
  MUST NOT 寫成 task status（不存在 `blocked` 這個 task 狀態）。縮寫（`run`）只是
  寬度不足時的 presentation alias，不得引入權威字以外的新詞。
- **selection model**：OWNERS／WORKERS 的 worker 列與其下的 in-flight task 列
  **皆為可選取列**；task 列自帶 immutable task id。`x` 的合法目標只有 task 列
  （worker 列上按 `x` 無效並提示）。此 schema 自第一縱切即生效，否則 `x` 沒有唯一目標。

### `Enter` focus 的語意（跨 window／session）

`select-pane` 只改該 window 的 active pane，不保證 client 跟著切過去。定義為：

1. 目標 pane 在 current session → `select-window` ＋ `select-pane`
2. 在其他 session → `switch-client` 後同上
3. linked window 多處出現（多個 WHERE）→ 優先 current session 的 location，
   否則取第一個 live location；不彈詢問框（dashboard 不該為邊緣案例打斷人）

驗收以 current client 實際的 `session:window.pane` 為準（見 P1 gate）。

### 薄殼透明包 CLI（守住的原則）

TUI 每個動作都等於一條 `agent-bridge` 命令，且**在畫面上留下該命令原文**
（DETAIL 面板 evidence 區＋動作確認框內）。這保證 TUI 永遠只是加速器、
CLI 才是介面；任何 TUI 能做的事，人離開 TUI 都做得到。

## 3. 鍵位表

| 鍵 | 動作 | 等價 CLI | 破壞性 |
|---|---|---|---|
| `Tab` | 面板間切換（正向 `OWNERS → WORKERS → TASKS`） | — | — |
| `Shift+Tab` | 面板間切換（反向） | — | —（**P4 效率量測（首輪 57%）驅動的 additive 補入**：量到的 4 步裡有 2 步是「從初始 WORKERS 單向繞到 OWNERS」的固定開銷；純鍵位面，不動 selection 起點與任何協定語意） |
| `j`/`k`（`↓`/`↑`） | 面板內移動 | — | — |
| `Enter` | focus 選中 worker 的 pane | §2 focus 語意（popup 下先關 popup） | — |
| `r` | 讀選中 task 全文 | `agent-bridge read <task-id>` | —（注意：`read` 會追加 read event，非純唯讀；gate 據此設計） |
| `i` | explain 選中 worker | v1 以 status＋registry 摘要頁替代（無 `explain` 子指令，見 §10） | — |
| `c` | 複製**證據** | 複製 task id／pane id／`read`／`status` 命令原文 | **MUST NOT 複製 mutation 命令** |
| `n` | spawn worker | `agent-bridge spawn …`（表單填參數） | 建立性 |
| `s` | send 任務給選中 worker | `agent-bridge send <name> --from <me> …` | 建立性 |
| `x` | cancel 選中 task 列 | `agent-bridge cancel <task-id>` | ✔ 單確認，綁 immutable task id |
| `e` | evict 選中 worker | `agent-bridge evict <name> --expect-pane <pane> --expect-generation <tag>` | ✔ 證據框＋CAS |
| `?` | 顯示**當前選中項**的合法鍵 | — | — |
| `q` | 離開 | — | — |

**`c` 的複製後端（v1 定案）**：寫入 **tmux buffer**（`set-buffer`），不引入
clipboard crate、不依賴 OSC52／Wayland；人要出 tmux 世界自己 `paste-buffer`。
可測性：`tmux show-buffer` 讀回斷言。複製 payload 的組裝下沉到 action 層
（`Clipboard` trait 可注入假件），測試不經 render。

**裸 `despawn` 不進 TUI**：跳過收尾筆記、且最易被誤讀成「可安全刪除」。
要回收就走 evict 的兩階段協定（先派收尾任務、筆記落地、再回收）——
它是 ab 世界的 reflog，是敢做危險操作的底氣。

## 4. Read model 與證據優先序

> herdr 刮螢幕是因為它沒有協定；**ab 自己就是協定**。

1. **task-plane（`tasks/` status 檔）** — 唯一權威，任何訊號不得覆寫
2. **registry（`agents/`）＋ generation（`spawn_tag`）** — identity／provenance 權威；
   tmux live query 只補位置與 liveness
3. **fresh hook state（`state/`）** — activity 強輔助，不得改 task status
4. **screen matcher** — 最低層，只描述 `visible_blocker`／`occluded`

### 雙軸狀態（本鏈裁定；終審輪 agy 已明確接受）

顯示採 **ACTIVITY × BLOCKER 正交疊加**（如 `task=running + blocker=permission`），
不用單一 enum 強制覆蓋——後者會弄丟「任務其實在跑」這個事實，
而那正是人要判斷介不介入時最需要的資訊。

### v1 matcher 契約（終審後收窄，與 §7 對齊）

v1 只承諾兩件事，全部有現成實作來源：

- `visible_blocker ∈ {permission, plan}`：沿用現有硬編碼 `screen_has_prompt`
  ＋既有 fixture／canary 測試
- `occluded`：**結構性查詢**（`pane_in_mode`，AB-COPYMODE-1 已加），不靠畫面比對。
  occluded 時 blocker 軸回 `Occluded`，activity 沿用前值帶 TTL；
  **只存 TUI 記憶體、不寫 FS**；重開沒有前值就是 unknown。
  （copy-mode 是人在看，不是 worker 閒著。）

**移到 manifest 階段**（v1 明確不做，因為沒有分類規則、樣本分母與 runtime
版本紀錄就算不出 hit-rate）：per-runtime hit-rate canary 與 `matcher-stale(runtime vX)`
標示、transcript 檢視的 occlusion、協定外活動偵測。

### matcher 收窄（2026-08-01，使用者裁定立案；§7 manifest 化條款不動）

P4 把 BLOCKER 軸接進 TUI 之後，`screen_has_prompt` 的誤判從「偶爾漏送一則
通知」升級成「500ms／2s 輪詢下的常駐假 `⛔blocked`」。實測分母：一個正常
工作中的 coordinator pane，24 輪抽樣誤判 **19 次（79%）**；192 個真實畫面
樣本中 copy-mode 命中 0 次（第一道關卡不是通案原因）。根因是**整屏無錨
substring**——任何**談論**權限框的畫面都會命中，一行
`rg 'Requesting permission for:|Do you want to proceed'` 的指令回顯自己就
湊齊三組特徵。

收窄三條（實作在 `ab_core::notify`）：

1. **位置有錨**：只掃畫面下緣 `TAIL_LINES=14` 行
2. **鄰近成對**：同一特徵組的片段須落在 `PROXIMITY_LINES=12` 行的窗內
3. **單錨降級**：`Requesting permission for:` MUST 與同框的問句／選項行／
   footer 成對

兩個常數是量出來的，不是拍的：

| 來源 | 量測 | 距底行數 |
| --- | --- | --- |
| 真框（Claude Code v2.1.220，33×143） | 框出現時**取代**輸入框與 statusline，整框貼底 | `Esc to cancel` 0、問句 5、框頂 13 |
| 真框（agy，docs/agy-probe.md 十行框） | header 在框頂、footer 貼底 | header 9 |
| 誤判語料（19 幀，poll-133-*） | 指令回顯與散文引用，都在畫面上半部 | 最淺的一個 **16** |

`TAIL_LINES=14` 涵蓋距底 0–13（含）。兩側餘裕的**精確語意**（避免 off-by-one
誤讀）：真框側所需最深片段在 9，**還能再深 4 行仍被涵蓋、第 5 行開始漏判**；
誤判側語料最淺的一幀在 16，**要再往下移 3 行（到 13）才會重新被掃到**。
`PROXIMITY_LINES=12` 的下界是 agy 框的 header→footer 跨距 9。

**踩過的坑**：鄰近窗若允許越過下緣區界往上吃，19 個誤判幀有 12 個照樣命中
（窗結束在下緣區、起點卻回到上半部）。窗一律夾在下緣區內。另外
`capture-pane` 會把 pane 底部沒寫到的列補成空行，尾端空白行必須先剝掉，
否則「距底幾行」被整批推高、下緣錨等於白設——那個方向是**漏判**。

**TUI 側去抖**：screen-matcher 來源的 `Prompt` 連續 `K=2` 輪命中才升旗，
未達門檻顯示為「沒有可見 blocker」（**不是** unknown——該輪畫面確實讀到了，
謊報成沒訊號會讓 §5 三態語意失真）；降旗即時。結構性的 `occluded`
（`pane_in_mode`）**不去抖**：它沒有單幀誤判面，去抖只會白白延後。
延遲代價要照實說：升旗是「**首次命中後再等一輪**」。以 2s 一輪計，框可能剛好
在一輪剛掃完之後才出現，**自框出現算起最壞未滿 4s**（≈一輪等到首次命中，
再一輪確認），不是 2s。假警報則最多閃一輪就消失。worker 停在權限框是分鐘級
的等待，4s 對人的判斷沒有影響——但低報成 2s 會讓下一個人以為餘裕比實際多一倍。

**留下的風險（誠實記錄）**：兩個常數是對「這套 statusline ＋ 33 列 pane」
的量測；換一套更高的 footer、或框本身變高，餘裕會被吃掉。方向是**漏判**
（最壞方向），故未來若要再調，應以真框幾何為準、而非以乾淨度為準。
`notify-failed` 現在帶 `reason=`（spec HOOK-NOTIFY-4），下次要重驗這件事
不必再靠事後統計反推。

### bounded-read 硬條款（終審補入）

**TUI read model 消費的每一條 tmux 查詢 MUST 有上限**（`run_bounded`／
`AGENT_BRIDGE_TMUX_TIMEOUT` 語意），逾時該欄降級 unknown、**不得凍結整個 UI**。
現況：`capture_pane`／`pane_in_mode`／`send_keys`／`list-panes` 已 bounded，
但泛用 `exec` 與 `resolve_pane`（`tmux.rs`）仍是無界 `.output()`——
TUI 動工前必須補齊或繞開。機器 gate：hanging tmux shim 下 UI 仍在指定時限內完成一輪刷新。

### 更新方式：輪詢，不上事件系統

500ms 輪詢磁碟 task-plane ＋ registry（都是小 JSON 檔），
tmux 查詢節流（liveness 每 2s 一輪；**節流不等於 timeout**，兩者都要）。
herdr 實測 idle→working 1.56s，500ms 輪詢的體感已優於競品，
**不需要 daemon／inotify／socket**。

## 5. Mutation 與安全

第一版就有 mutation（三廠一致推翻唯讀立場）：唯讀版的「複製命令」是自我拆台
——複製 `evict foo`→切走→貼上→執行，TOCTOU 完全相同，多一層人工轉寫錯誤，
且是唯一做不到 CAS 的版本。

### TOCTOU 解法：compare-and-act

確認當下**重讀**狀態，mutation 帶世代識別，不符即 `selection stale` 中止：

- `cancel <task-id>`：task id 本身 immutable，天然 CAS，單確認即可。
- `evict <name> --expect-pane <pane> --expect-generation <spawn_tag>`（**CLI-EVICT-4，
  additive 新設計**）。終審 blocker 釐清了範圍，不是「單純曝露既有機制」：
  - 既有 `spawn_tag` 綁定（`spawn.rs` despawn 路徑、鎖內比對）只保護
    「收尾任務送出後 → despawn」這段；**「TUI 選中 → evict 呼叫」這段目前無保護**
    ——現行 `cmd_evict` 在入口無鎖讀當代 registry 就 `do_send`，selection stale
    時會把收尾任務派給新世代並回收它。
  - 因此 expect 驗證 MUST 在**任何副作用之前**執行，且與收尾任務的建立
    在**同一 registry 鎖內**完成（或等價的原子手段），堵住「驗完到 do_send
    之間換代」的第二個 race window。
  - 不符 → `selection stale`、非 0 退出，**不建 task、不通知、不 kill、不動 registry**。
  - 不帶 expect 參數時行為與現行完全相同；既有的送出後綁定照舊（兩個 race
    window 各自有各自的防線）。
  - spec 側新增 CLI-EVICT-4；`spec/traceability.md` 同步。

### 顯示紀律（硬原則：「沒有任何訊號」≠「可安全刪除」）

這條管**顯示**不管**行動**：禁止任何欄位替人下判斷，不禁止人看完證據後自己決定。

- evict 確認框措辭 MUST 表達「派收尾任務後回收」語意（P4.6 起 chrome 為英文，
  正典短語：`wrap-up task, then reclaim`），MUST NOT 出現任何語言的「安全刪除」
  語彙（測試同時擋中英兩套）
- 不得以顏色／排序暗示可刪度；不得因 idle／disposable 預選；不做批次自動清理
- `c` 只複製 immutable 證據，MUST NOT 複製 mutation 命令

## 6. 依賴清單（「相對 herdr 低」的具體化）

herdr 的包袱：自帶 PTY server、遠端 manifest、socket API、外掛市集。
ab-tui 的上限：**新增的每個 crate 依賴都要能一條條說出理由**，並排除整類機制。

| 依賴 | 理由 | 排除的替代 |
|---|---|---|
| `ratatui` | 面板佈局／樣式／alternate screen 的業界標準，手刻 ANSI 等於自造劣化版 | 手刻 escape codes（維護黑洞） |
| `crossterm` | ratatui 的預設 backend，raw mode ＋ key event ＋ `event::poll` 輪詢 | termion（平台面窄）、tokio 事件流 |
| `ab-core`（path） | 既有 read model／registry／tmux 邊界全在這，TUI 只是消費者 | 重複實作（違反薄殼原則） |

**明確不加**：async runtime（tokio 等——輪詢迴圈用 `event::poll` timeout 即可）、
daemon／socket／IPC、clipboard crate（走 tmux buffer，見 §3）、
設定檔格式新依賴、任何網路 crate。
版本於實作時以 `cargo add` 當下查證鎖定，不在本文件預寫版號。

## 7. screen matcher manifest 化：**不進第一版**（本棒裁定，終審有條件維持）

前鏈已裁定 manifest 化方案（TOML 作者格式、build-time embed、執行期零探索），
方案本身健全、**保留備用**。終審條件：v1 的 matcher 契約必須同步收窄到
§4 所列兩項（硬編碼 blocker ＋ 結構性 occlusion），**不得保留 manifest 等級的
驗收卻沒有實作來源**。hit-rate canary／`matcher-stale`／transcript occlusion／
協定外活動偵測與 manifest 同進退，待 runtime 版本更迭實際打痛再一起啟動。

## 8. 第一縱切（實作起手式，非本階段動工項）

`OWNERS | WORKERS` 兩欄（含 §2 的 task 可選取列）＋ `Enter` focus ＋ `x` cancel
——一次驗證四個地基：read model（磁碟→畫面）、alternate screen 進出、
focus 跨 window 語意、CAS（cancel 綁 id）。
之後依序補 TASKS／DETAIL＋`r`/`i`/`c`，最後 evict 證據框＋CLI-EVICT-4。

## 9. 驗收判準

固定 fixture：3 owner／12 worker／20 task／3 異常
（1 dead owner、1 blocked prompt、1 orphaned worker）。

### Phases × Gates

| Phase | 內容 | Gate（可機器判定除非另註） |
|---|---|---|
| P0 設計定案 | 本文件＋Artifact 版 | **human judgment**：使用者明確表示可動工（定案的定義即是使用者確認，無機器替代） |
| P1 第一縱切 | 兩欄＋focus＋cancel | (a) cancel 指定 task 列後 task 檔狀態轉 `cancelled`；(b) 退出後 pane／window id、layout string 與 geometry 不變、alternate screen 還原（正常退出與錯誤退出／raw-mode cleanup 各測一次）；(c) `Enter` 後 current client 的 `session:window.pane` 等於目標（含跨 window 案例） |
| P2 四面板＋唯讀鍵 | TASKS／DETAIL＋`r`/`i`/`c` | (a) `c` 後 `tmux show-buffer` 內容斷言不含任何 mutation 子指令字串（action 層另以假 Clipboard 斷言 payload 組裝）；(b) `r` 在 **action 層**取得的 response bytes 與 `agent-bridge read` 一致（render 層另做 terminal snapshot 測試，不做逐字節比對） |
| P3 evict CAS | 證據框＋CLI-EVICT-4 | 三則：(a) expect 相符→evict 成功；(b) **invocation 前已換代**→`selection stale` 非 0 退出，且不建 task、無通知、pane 未 kill、registry 未動；(c) 送出後→despawn 前換代（既有 window）→照舊拒收（回歸既有 CLI-EVICT-3 行為） |
| P4 效率驗收 | 三異常定位 | 兩份**固定 replay script**（baseline：`list --long`＋合法唯讀命令序列；TUI：key 序列）＋明確成功 marker（三個異常 id 均被輸出／複製）；計數規則＝script 內按鍵／命令步數；TUI ≤ baseline 的 50%，正確率 100% |
| P4.5 視覺樣式層 | `theme` 模組＋ANSI 16 語意色（additive 純樣式：狀態字上色但權威字原文不變、liveness／blocker 語意色、focus Thick 邊框、選取列背景色、footer 警告 Yellow——顏色編碼軸固定五種：status／liveness／blocker／focus／warning；不動 selection 起點／鍵位／面板順序） | (a) render 單元測試（ratatui `TestBackend`）：指定 cell 的 fg/bg/modifier 符合 theme 對映（每個語意色至少一條）＋buffer 字元內容仍含六個權威字原文與 ●/✗/⛔ glyph；(b) P4 replay 重跑步數不變仍達標（樣式不進 capture 計數，重跑確認非假設） |
| P4.6 UX truthfulness | 使用者實跑回饋（2026-08-01，9 題）第一段：ORIGINS 平坦視圖取代假 owner 樹（origin 誠實標籤 `session:window-name`＋live/gone/unknown，不做推測分組）；DETAIL 拆 agent state／origin window 兩列；Enter 分面板（OWNERS→WORKERS、worker→focus、task→read，`r` 保留）＋contextual footer；chrome 全英文；TASKS scrollbar＋`N/total`＋PgUp/PgDn/Home/End；pager markdown-lite 高亮（標題/fence/清單/中繼標頭；diff 僅限明確 diff 區段；bytes 不變，色彩契約加第六軸 content-syntax）；selection 改存 stable key（task id／(name,spawn_tag)）；freshness 顯示（disk/tmux age，逾時 stale 降級）。零協定改動 | (a) P1 gate 重跑（Enter matrix 動 focus 語意）；(b) P2 read bytes gate 重跑；(c) P4 replay 依新鍵位/文字重寫後 TUI ≤ baseline 50% 且正確率 100%；(d) render 測試新增斷言：英文 chrome（buffer 無中文 chrome 字元）、origin 標籤三態、scrollbar 首/中/末 thumb、reload 重排後 selection 依 stable key 不跳列、stale 降級顯示；(e) pager 高亮測試：同一 bytes 前後字元層完全相同＋fence/標題命中＋散文 +/- 不染 |
| P4.7 lineage provenance | 第二段（**待 P4.6 收斂後使用者再批**）：registry additive optional 欄位 `lineage_root`＋`parent_agent`（spawn 依 SPAWN_TAG 精確繼承；legacy 標示不 backfill；欄位僅 provenance/display，不升格 auth/CAS）；TUI 依 lineage 分組＋DETAIL breadcrumb；`/` literal filter；copy-mode info banner；`L` 尾行預覽（one-shot、行/byte/時間三重有界）；TASKS All/Unattached scope | (a) lineage fixture root→A→B→C：移除 A/B registry 後 C 及其 tasks 仍歸同一 lineage、breadcrumb 留 tombstone、不得由 task from 推導；(b) 新舊 registry 混合：缺欄位列標 legacy 不誤併，既有 owner／spawn／evict／CAS 行為逐字不變；(c) P4 fixture 異常定義重寫（dead owner 已失真）後重量 ≤50%；(d) filter/L/banner 各自 bounded 斷言 |
| P5 理解驗收 | 歸屬樹測驗（**順延至 P4.7 之後**，rubric 改驗 lineage→worker→task 歸屬；2026-08-01 使用者裁定） | **human judgment**：受測者 10 秒內畫出 lineage→worker→in-flight task 正確歸屬樹（理由：關聯可見性是原始痛點，步數量不到它）；rubric 見下（P4.7 落地時同步改寫） |

P4 附註：replay script 是 fixture 的一部分（固定初始 selection 與異常排序位置），
量的是「固定操作序列下的步數差」，不宣稱量到任意操作者的自由行為。

**P4 量測史（本輪實測，分組 44）**：首輪 Tab 單向循環下 TUI 4 步／baseline
7 步＝**57%，未達標**；量到的缺口是「初始焦點在 WORKERS，單向繞到 OWNERS 要
兩步」的固定開銷（三個異常裡有兩個在步 0 就已可見）。據此補入 `Shift+Tab`
反向循環（§3 鍵位表，純 additive），同一份 replay script 降為 3 步＝3/7
≈ **43%，達標**（分組 44 以整數截斷印為 42%）。baseline 兩輪都是 7 步（`list --long` 一次判定 orphan 與 dead owner，
再逐 pane `capture-pane` 找 blocked prompt——CLI 沒有 blocker 軸，掃到第 6 個
命中）。BLOCKER 軸（§4 v1 契約）在本輪一併補齊，否則 TUI 側定位不到
blocked prompt，量到的會是功能缺口而不是效率差距。

### P5 rubric（每條可由證據答是／否）

1. 受測者畫出的樹中，3 個 owner 的 worker 歸屬全部正確？
2. 3 個異常標的（dead owner／blocked prompt／orphaned worker）全部被指認？
3. 每個 in-flight task 都掛在正確的 worker 下？
4. 作答時間 ≤10 秒（碼表計時）？

## 10. 開放問題（已全數定案；2026-08-01 使用者拍板回填）

四項原為「待拍板」，對應實作已隨 P0–P3 落地；2026-08-01 使用者確認
「已拍板，順帶回填文件」，據此改記為定案：

1. `i` explain 在 v1 以 status＋registry 摘要頁替代（不新增協定子指令）——**定案：接受**
2. manifest 化延後＋v1 matcher 契約收窄到硬編碼 blocker＋結構性 occlusion（§4／§7）——**定案：同意**
3. 第一縱切範圍（§8，含 task 可選取列）——**定案：同意**
4. `c` 複製後端定為 tmux buffer（不進系統剪貼簿，§3）——**定案：接受**

## 11. P4.6/P4.7 立案記錄（2026-08-01）

使用者實跑後給 9 題回饋；經 codex＋agy 兩廠獨立提案＋交叉輪（2/5 輪收斂）
成 §9 的 P4.6/P4.7。根因判定：registry `owner`＝spawn 當下 window id，是物理
位置非邏輯 principal，relay 鏈使其斷裂（題 2/5/7 同根）。使用者四裁定：
P4.6 先行（P4.7 待其收斂後再批）／chrome 全英文／`L` peek 進 P4.7／
P5 順延至 P4.7 後。全文：session scratchpad `ui-feedback-proposal.md`。

**Backlog（非相位項）**：`notify-failed` 的 `pane-gone` 桶混淆「tmux 查詢失敗」
與「pane 真不存在」（codex sandbox 擋 tmux socket 時誤報 pane-gone，實例
2026-08-01T13:03:58Z pane=%139 活著）——建議拆出 `query-failed` reason。

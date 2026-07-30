# Rust 遷移架構設計（ab-core / ab / ab-tui）

- 地位：**結構錨**。行為錨是 `spec/`（90 條款）＋`tests/run-tests.sh`（40 編號
  分組）；本文件回答「Rust 側怎麼組織」，行為疑義一律回 spec 與 bash 正本
  （`bin/agent-bridge`，M4 cutover 前不動）。
- 計畫正本：`~/.claude/plans/cheeky-waddling-meadow.md`（phases × gates）。
- 判準：M1 實作若與本文件衝突，先改文件再改碼（文件是 review 對象，不是擺設）；
  模組命名與邊界變更須在 PR 說明列出對映表 diff。

## 1. Crate 邊界

| crate | 角色 | 依賴 |
|---|---|---|
| `ab-core` | 領域邏輯庫：狀態、儲存、hook 核心、tmux client | `serde_json`（`preserve_order`，M1 起；使用者裁決 2026-07-31）外不引入依賴 |
| `ab` | CLI binary：argv 解析、輸出格式化、dispatch | `ab-core`＋`libc`（M2 起，僅為 §5 的 `signal(SIGPIPE, SIG_DFL)` 一行；零傳遞依賴） |
| `ab-tui` | 佔位；M5 後、fzf/tmux popup 原型驗證有價值才動工 | `ab-core` |

## 2. ab-core 模組對映表（模組 ↔ spec 域 ↔ bash 函式群）

| 模組 | 職責 | spec 域 | 對映 bash 函式（bin/agent-bridge） |
|---|---|---|---|
| `paths` | 資料目錄佈局（agents/ tasks/ state/ locks/）、`AGENT_BRIDGE_DATA` 解析 | state.md、env.md | `ensure_dirs`:148 |
| `config` | `AGENT_BRIDGE_*` 全部 env 的讀取與驗證；**變數名保留字面字串**（check-contract check 1 的 grep 面） | env.md 17 條 | 散落各處的 `${AGENT_BRIDGE_*:-}` 讀取點 |
| `fsio` | `atomic_write`（tmp＋rename 同目錄）、payload byte 流原樣搬運 | state.md STATE-GEN-2 | `atomic_write`:172、`write_message`:276 |
| `lock` | mkdir 鎖：佔用重試＋「非佔用即權限」die 分流；RAII guard 只是便利層，**語意不依賴 Drop**（SIGKILL 殘鎖行為＝bash 現況；stale 回收是 M5 行為變更，parity 期禁做） | state.md STATE-LOCK-* | `acquire_lock`:227、`release_lock`:217 |
| `registry` | agent 註冊表 CRUD、pane 解析、owner/actor 審計欄位 | state.md STATE-AGENT-* | `cmd_register`:476 核心、`is_spawned`:153、`caller_owner`:884、`log_agent_event`:898 |
| `task` | task 目錄結構、id 生成/驗證、狀態機轉換（queued→delivered→running→終態）、events.log、殘缺 task 目錄清理、**gc（終態 task 清理）** | state.md STATE-TASK-*、cli.md 轉換條款 | `write_message`:276、`log_event`:251、`update_meta_status`:261、`check_task_id`:420、`last_task_at`:457、`send_rollback`:202（清 metadata/status 未寫齊的殘缺 task 目錄，屬 task 寫入交易邊界；交易背景註解始於 :195）、`cmd_gc`:1683 |
| `notify` | 送鍵通知：權限框雙掃、失敗語意（notify-failed 可復原）、`notify_or_defer` 的 TTL/state 新鮮度 gate | hooks.md HOOK-NOTIFY-*、env.md ENV-TTL-1/2、ENV-NOTIFY-1 | `notify_pane`:330、`screen_has_prompt`:318、`notify_or_defer`:371 |
| `hook` | hook 三事件核心：身分解析、state 單一寫者、owner gate（session_id 所有權）、oldest-queued；**失敗一律就地吞掉（不上拋 `Result`）**，`run()` 回 `HookOutcome{Silent,Block}`；exit 0 與 panic 兜底在 `ab` dispatch 層（M2 修正，見 §4） | hooks.md 14 條、state.md STATE-CHAN-* | `hook_agent_name`:2010、`hook_write_state`:2035、`hook_owner_gate`:2069、`hook_oldest_queued`:2098、`cmd_hook`:2131 |
| `spawn` | worker 生命週期：cap、pane 建立、brief 注入、ready 探針、原子回滾、出身防護（tag）、evict/idle/disposable | cli.md spawn 群、env.md ENV-SPAWN/READY/TAG | `cmd_spawn`:1063、`spawn_rollback`:940、`rb_kill_tagged`:928、`spawn_wait_ready`:1038、`worker_prompt_arg`:1007、`relay_prompt_arg`:1024、`cmd_despawn`:1476、`cmd_evict`:1866、`disposable_effective`:444 |
| `tmux` | `TmuxClient` trait＋`SubprocessTmux` 實作；**以裸名 `tmux` 經 PATH spawn**（測試 shim 攔截前提，tests/run-tests.sh:91-93）；argv 陣列傳參 | （載具，無獨立 spec 域） | `tmux` token 91 次（command-shaped 呼叫約 65；分佈於 notify/spawn/despawn/registry 各函式群） |
| `time` | ISO 8601 UTC 時戳（`now_iso`）、epoch 換算、TTL 新鮮度；顯式 UTC、不受 locale/TZ | state.md ts 格式 | `now_iso`:144、TTL epoch 比對段 |

覆蓋核對：bash 58 函式中，`cmd_*` 21 個屬 `ab` dispatch 面（邏輯下沉
ab-core 對應模組）、`err/die/info/usage` 屬 `ab` 輸出層、其餘 helper 已列入
上表對映欄。M1 實作時逐函式打勾，未列者（如 `rand_suffix`:146、
`require_jq`:166——Rust 無此依賴、`validate_ready_opts`:988、
`parse_message_opts`:664、`respond_task`:686）歸入所屬模組不另立。

## 3. 核心型別與所有權

- `AgentName`／`TaskId`：newtype over `String`，建構即驗證（對齊
  `check_task_id`:420 與 register 的名稱規則）；驗證失敗訊息逐字對齊 bash die。
- `TaskState`：`enum {Queued, Delivered, Running, Completed, Failed, Cancelled}`
  ——合法轉換表寫成 `TaskState::can_transition(to)`，非法轉換回傳
  `Error::IllegalTransition`（訊息對齊 bash）。磁碟表現＝小寫字串
  （`status` 檔與 metadata.json 逐字同 bash）。
- **Payload＝`Vec<u8>` 只存在於 `fsio` 邊界**：request/response 內文檔案
  read/write 原樣 byte 流（分組 6 驗保真），絕不經 `String`／lossy 轉換；
  結構化 metadata（JSON 欄位、id、時戳）為受控 ASCII/UTF-8，內部 `String`。
- Store 型別（`Registry`、`TaskStore`）持 `&Paths` 借用，不做全域單例——
  daemon 化（M5）時可多實例注入。

## 4. 錯誤模型 → exit code

- `ab-core` 全面 `Result<T, Error>`；`Error` 帶「使用者可見訊息」欄位，
  **訊息文字逐字對齊 bash die 的中文 stderr**（parity gate 驗這個）。
- `ab` dispatch 層統一收斂：`Err` → stderr 印訊息 → exit code。
  - 一般指令：die 對應非 0（各條款明定值以 spec/cli.md 為準；CLI-GEN-1 教訓
    ——呼叫端只能依賴「非 0＝失敗、124＝逾時」，不得依賴「失敗必為 1」）。
  - `hook` 子指令鐵律：**任何錯誤一律吞掉、exit 0**（bin/agent-bridge:2258-2261）。
    - M2 修正（原設計為「內部回 `Result`、dispatch 層吞」）：那個形狀表達不了
      bash 的實際語意。bash 是**逐步就地吞**——`hook_write_state` 寫失敗只讓
      state 停在舊值，後面的分支照跑；上拋成單一 `Err` 會把「這一步沒做成」
      誤升級為「整支中止」，兩者對 state 通道的終態不同。故 `ab-core::hook`
      每個失敗路徑就地收斂，`run()` 不回 `Result`。
    - dispatch 層兜底保留兩層：無條件 `ExitCode::from(0)`，外加 `catch_unwind`
      讓 panic 不會變成 101（panic 訊息照樣進 stderr，同 bash 出錯有輸出）。
  - 唯讀豁免：`status|await|idle|list|hook` 不建目錄；目錄缺失時以「找不到」
    語意收場（bin/agent-bridge:2266-2268 的 parity）。
- `panic!` 視為 bug：release 建置 `panic = "abort"` 候選（M1 定案），嚴禁把
  panic 當錯誤路徑。

## 5. 子行程／signal 政策

- **SIGPIPE**：Rust 預設忽略（寫端得 `EPIPE` Err）；`ab` main 起手以
  `libc::signal(SIGPIPE, SIG_DFL)` 顯式恢復預設處置，行為對齊 bash「隨管線
  死」。**M2 已落地**（`restore_sigpipe_default`）。
  - 裁決（M2）：為這一行引入 `libc`（零傳遞依賴），不採「每個 stdout 寫出點
    攔 `EPIPE` 再自行 `exit(141)`」——後者碼更多、每新增一個輸出點就多一個
    漏接機會，且自行退出與被訊號殺死對呼叫端的 `WIFSIGNALED` 仍不等價。
  - **驗證缺口**：測試套件**沒有任何一組**斷言 SIGPIPE。M1 交接檔稱「驗證
    錨點是分組 8」，經查不成立——分組 8 是「通知失敗路徑（pane 已死）」，
    與 SIGPIPE 無關。M2 以手動 canary 佐證（`ab read <大 payload> | head -c 1`
    的寫端退出碼 bash 141 vs Rust 141；移除該行後 Rust 退回 1 的 mutation
    反證亦跑過），但**沒有機器 gate 護著**，回歸不會變紅。
  - `catch_unwind`（hook 分支的 panic 兜底）與 §4 那條 `panic = "abort"` 候選
    互斥：真要改 abort，得先把 hook 的 panic 兜底換成別的做法。
  - **hook 分支不恢復 SIG_DFL**：SIG_DFL 之下 stdout 是已關閉管線時整個行程
    會被訊號殺死（141），違反 hook 的 exit 0 鐵律；bash 那邊 `jq … || true`
    把寫出失敗吞掉照樣 exit 0。故 `restore_sigpipe_default()` 排在 hook 分流
    之後（M2 codex 複核 finding 1）。

### 5.1 已裁決、刻意保留的 parity 偏離（M2）

兩項在 M2 codex 複核中被指出，判定為**不修**，理由記在這裡免得下一棒重開。

- **不模擬「jq 不在 PATH 就 no-op」**。bash `cmd_hook`:2142 有
  `command -v jq || exit 0`；Rust 沒有 jq 依賴，照抄等於讓一個無關工具的缺席
  癱瘓自己的 hook。§2 覆蓋核對表早已把 `require_jq`:166 記為「Rust 無此依
  賴」，本項沿用同一裁決。可觀察差異：PATH 無 jq ＋ 有 queued task 的 stop
  事件，bash 靜默 exit 0、Rust 照發 block JSON。
- **state 檔的 `ts` 只認 canonical ISO**，不模擬 GNU `date -ud` 的寬鬆方言
  （`now`、`yesterday` 等）。可觀察差異：異主 ＋ `ts:"now"` 時 bash 判為新鮮
  而擋下接管，Rust 判為不可解析而放行。不修的理由：`ts` 只由
  `hook_write_state` 自己寫、恆為 canonical，要踩到這條得先有人直接改寫
  state 檔——而能直接寫該檔的行為體本來就能把 `owner` 換成自己，gate 的威脅
  模型不涵蓋它。為了這條引進一個 GNU date 方言 parser 不成比例。
- **pipe 排空**：子行程（tmux capture 等）輸出一律先讀盡 stdout/stderr 再
  `wait()`，防 OS pipe buffer 滿載互等死鎖。
- **繼承環境**：子行程 spawn 不清洗環境（bash 同款）；`AGENT_BRIDGE_PASS_ENV`
  等穿透語意照 spec。
- **timeout 語意**：bash 用外部 `timeout`（124）之處，Rust 內建計時但**維持
  124 退出碼**（cli.md await 條款）。

## 6. 鎖語意（parity 紅線）

`lock::Guard` 釋放走顯式 `release()`＋`Drop` 雙保險，但正確性論證只允許引用
顯式路徑：SIGKILL／abort 時 Drop 不執行，殘鎖=bash 現況等價（mkdir 鎖天生
如此）。「非佔用即權限」的 die 分流（分組 8b）在 `acquire` 實作內逐字對齊
bin/agent-bridge:227 起的錯誤訊息。stale-lock 偵測／回收＝行為變更，屬 M5
提案範圍，parity 期不得混入。

## 7. 輸出 parity 策略（jq 對拍）

- bash 以 jq 產生的 JSON（metadata.json、state/*.json、list 輸出）逐測例
  fixture 對拍：先用 bash 版產 fixture（`jq` 實際輸出存檔），Rust 序列化結果
  `diff` 零差異才進 gate。
- **解析端走 `serde_json`（`preserve_order`）**（M1 裁決，使用者 2026-07-31）：
  欄位序＝插入序，重複鍵取後值，與 jq 一致；`Map::insert` 對既有鍵保留原
  位置，正是 `update_meta_status`（jq `.status = $s`）要的賦值語意。
- **輸出形狀仍由 `json::render_pretty` 掌控**，不用 `to_string_pretty`：要對齊
  的是 jq 的形狀而非某個 pretty printer 的預設（該預設隨版本可變）。自家
  render 讓形狀成為本 repo 測得到、改得動的東西；jq fixture 對拍是 gate。
- 換 parser 時，M0.5 的邊界測試（重複鍵、裸控制字元、落單 surrogate、非
  object 根）全數留下改為斷言 serde_json 的行為——它們守的是本專案依賴的
  JSON 語意，不是某一個實作。

## 8. 流程圖

三張核心流程圖（畫的是 bash 現行為，Rust 實作照此 parity）：

1. send→notify_or_defer→hook 通知鏈 — `docs/rust/flow-notify.md`
2. spawn 生命週期（cap／ready 探針／原子回滾／出身防護）— `docs/rust/flow-spawn.md`
3. state 通道 TTL 判定（owner gate 含邊角）— `docs/rust/flow-ttl.md`

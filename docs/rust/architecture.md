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
| `ab-core` | 領域邏輯庫：狀態、儲存、hook 核心、tmux client | std only（M1 起視需要 `serde_json`） |
| `ab` | CLI binary：argv 解析、輸出格式化、dispatch | `ab-core` |
| `ab-tui` | 佔位；M5 後、fzf/tmux popup 原型驗證有價值才動工 | `ab-core` |

## 2. ab-core 模組對映表（模組 ↔ spec 域 ↔ bash 函式群）

| 模組 | 職責 | spec 域 | 對映 bash 函式（bin/agent-bridge） |
|---|---|---|---|
| `paths` | 資料目錄佈局（agents/ tasks/ state/ locks/）、`AGENT_BRIDGE_DATA` 解析 | state.md、env.md | `ensure_dirs`:148 |
| `config` | `AGENT_BRIDGE_*` 全部 env 的讀取與驗證；**變數名保留字面字串**（check-contract check 1 的 grep 面） | env.md 17 條 | 散落各處的 `${AGENT_BRIDGE_*:-}` 讀取點 |
| `fsio` | `atomic_write`（tmp＋rename 同目錄）、payload byte 流原樣搬運 | state.md STATE-GEN-2 | `atomic_write`:172、`write_message`:276 |
| `lock` | mkdir 鎖：佔用重試＋「非佔用即權限」die 分流；RAII guard 只是便利層，**語意不依賴 Drop**（SIGKILL 殘鎖行為＝bash 現況；stale 回收是 M5 行為變更，parity 期禁做） | state.md STATE-LOCK-* | `acquire_lock`:227、`release_lock`:217 |
| `registry` | agent 註冊表 CRUD、pane 解析、owner/actor 審計欄位 | state.md STATE-AGENT-* | `cmd_register`:476 核心、`is_spawned`:153、`caller_owner`:884、`log_agent_event`:898 |
| `task` | task 目錄結構、id 生成/驗證、狀態機轉換（queued→delivered→running→終態）、events.log、殘缺 task 目錄清理 | state.md STATE-TASK-*、cli.md 轉換條款 | `write_message`:276、`log_event`:251、`update_meta_status`:261、`check_task_id`:420、`last_task_at`:457、`send_rollback`:202（清 metadata/status 未寫齊的殘缺 task 目錄，屬 task 寫入交易邊界；交易背景註解始於 :195） |
| `notify` | 送鍵通知：權限框雙掃、失敗語意（notify-failed 可復原）、`notify_or_defer` 的 TTL/state 新鮮度 gate | hooks.md HOOK-NOTIFY-*、env.md ENV-TTL-1/2、ENV-NOTIFY-1 | `notify_pane`:330、`screen_has_prompt`:318、`notify_or_defer`:371 |
| `hook` | hook 三事件核心：身分解析、state 單一寫者、owner gate（session_id 所有權）、oldest-queued；**任何錯誤收斂 exit 0 的鐵律在 `ab` dispatch 層兜底、此處以 Result 上拋** | hooks.md 14 條、state.md STATE-CHAN-* | `hook_agent_name`:2010、`hook_write_state`:2035、`hook_owner_gate`:2069、`hook_oldest_queued`:2098、`cmd_hook`:2131 |
| `spawn` | worker 生命週期：cap、pane 建立、brief 注入、ready 探針、原子回滾、出身防護（tag）、evict/idle/disposable/gc | cli.md spawn 群、env.md ENV-SPAWN/READY/TAG | `cmd_spawn`:1063、`spawn_rollback`:940、`rb_kill_tagged`:928、`spawn_wait_ready`:1038、`worker_prompt_arg`:1007、`relay_prompt_arg`:1024、`cmd_despawn`:1476、`cmd_evict`:1866、`cmd_gc`:1683、`disposable_effective`:444 |
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
  - `hook` 子指令鐵律：**任何 `Err` 一律吞掉、exit 0**（bin/agent-bridge:2258-2261）；
    此兜底放 `ab` 的 hook 分支，`ab-core::hook` 內部照常回 `Result`（單元測試
    可斷言內部錯誤，黑箱面永遠 0）。
  - 唯讀豁免：`status|await|idle|list|hook` 不建目錄；目錄缺失時以「找不到」
    語意收場（bin/agent-bridge:2266-2268 的 parity）。
- `panic!` 視為 bug：release 建置 `panic = "abort"` 候選（M1 定案），嚴禁把
  panic 當錯誤路徑。

## 5. 子行程／signal 政策

- **SIGPIPE**：Rust 預設忽略（寫端得 `EPIPE` Err）；`ab` main 起手顯式恢復
  `SIG_DFL`，行為對齊 bash「隨管線死」。M1 落實後以分組 8 通知失敗路徑驗。
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
  fixture 對拍：M1 起手先用 bash 版產 fixture（`jq` 實際輸出存檔），serde
  序列化結果 `diff` 零差異才進 gate。
- `serde_json` 欄位序採 insertion order（`preserve_order`）比對 jq 產物；
  縮排/空白形狀以 fixture 為準，必要時手寫 serializer helper，不遷就
  serde 預設。

## 8. 流程圖

三張核心流程圖（畫的是 bash 現行為，Rust 實作照此 parity）：

1. send→notify_or_defer→hook 通知鏈 — `docs/rust/flow-notify.md`
2. spawn 生命週期（cap／ready 探針／原子回滾／出身防護）— `docs/rust/flow-spawn.md`
3. state 通道 TTL 判定（owner gate 含邊角）— `docs/rust/flow-ttl.md`

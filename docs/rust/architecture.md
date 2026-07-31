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
| `spawn` | worker 生命週期：cap、pane 建立、brief 注入、ready 探針、原子回滾、出身防護（tag）、idle/disposable（**evict 的三段式編排在 `ab` CLI 層**，見 §9） | cli.md spawn 群、env.md ENV-SPAWN/READY/TAG | `cmd_spawn`:1063、`spawn_rollback`:940、`rb_kill_tagged`:928、`spawn_wait_ready`:1038、`worker_prompt_arg`:1007、`relay_prompt_arg`:1024、`cmd_despawn`:1476、`cmd_ready`:1594、`cmd_disposable`:1629、`cmd_idle`:1800、`disposable_effective`:444 |
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

## 9. M3（spawn 生命週期）的裁決與偏離

### 9.1 執行檔擺放位置是 brief 解析的前提

bash 以 `readlink -f "$BASH_SOURCE"` 反推 `REPO_ROOT`（:45-46），brief／hooks
settings 的預設值都掛在它下面。Rust 用 `current_exe()`（Linux 讀
`/proc/self/exe`，同樣解析完符號連結）套**同一條規則**：`REPO_ROOT ＝
dirname(dirname(執行檔))`。

代價是 `target/release/ab` 的祖父層是 `target/`，預設 brief 會指到
`target/share/…`。而測試 22f／23a／16a2 又分別用
`dirname(dirname($BRIDGE))/share` 與硬編的 `$ROOT/share/…` 反推正本位置，兩者
必須同時成立。故 **parity gate 的執行載具是 `.gate/ab`**（gitignored，由
`cargo build --release` 後 `cp` 過去）：

```
cargo build --release && mkdir -p .gate && cp -f target/release/ab .gate/ab
BRIDGE=$PWD/.gate/ab bash tests/run-tests.sh
```

這不是為了讓斷言變綠而搬位置：`bin/` 與 `share/` 是兄弟目錄是本專案的安裝
佈局，`.gate/` 只是讓建置樹長成那個形狀。M4 cutover 後執行檔就落在 `bin/`，
這層 staging 隨之消失。計畫原文寫的 `BRIDGE=$PWD/target/release/ab` 在 M3 起
不再適用（M1/M2 的分組不碰 brief 路徑，故當時看不出來）。

### 9.2 `printf %q` 逐位元重現

`spawn::shell_quote` 重現 bash `printf %q`：安全字元集
`%+-./0-9:=@A-Z_a-z`（本機 bash 5.3 全 byte 實測導出），其餘 printable 前置
反斜線，含控制字元則整串走 `$'…'`。分組 16a4／16a5 直接比對
`pane_start_command` 裡的片段（` no_proxy=st\ a\,b exec `），換成語意等價的
單引號形式那兩條會紅。

### 9.3 審計失敗在 spawn 必須翻盤

`log_agent_event` 在 despawn／disposable／evict 是「只揭露不翻盤」（不可逆
動作已完成），但在 **spawn 的 registry 寫入之後、回滾解除之前**必須上拋
（bash 在 `set -e` 下由 EXIT trap 完成回滾）。吞掉會留下一個沒有審計線、卻
佔著 cap 的 worker，而呼叫端看到成功。分組 19c/19c'/19d/19e 是這條的錨。

### 9.4 測試 harness 的兩處改動（M3）

parity gate 的前提是**測試對實作語言中立**。兩類斷言原本不是：

1. **注入點掛在 `date(1)`**（§19c'、§21）：那只是 bash 實作恰好會 fork 的外部
   指令；Rust 內建時戳，場景根本不會發生，測到的變成「實作有沒有用 date」。
   改掛 `tmux`——19c' 掛回滾必經的 `if-shell`，21 掛鎖內建 pane 的呼叫。
2. **`sed` 抽 shell 函式本體**（§30 CC canary、§31i 寫入順序）：源碼耦合檢查，
   與 check-contract 1–3 同類。抽取對象改為固定的 `SRC_BASH`（實作正本），
   **M4 cutover 時與 check-contract 1–3 一起改綁 Rust 源**。在那之前，Rust 側
   由兩個單元測試補上同一組不變量（`notify::tests::
   matcher_uses_the_canary_feature_strings`、`task::tests::
   status_is_written_before_metadata`）。

改後 bash 基準重跑仍為 756 PASS／0 FAIL。

### 9.5 codex 複核一輪的處置（2026-07-31）

rubric 六條判定 REJECTED／CONFIRMED／REJECTED／REJECTED／CONFIRMED／CONFIRMED。
兩個 blocker 與五個 should-fix 全數修復並重驗：

| 項目 | 症狀 | 處置 |
|---|---|---|
| blocker：despawn 謊報成功 | `remove_file` 的錯誤被吞，仍寫審計＋印「已 despawn」 | 上拋（`NotFound` 視為成功）；bash `:1580` 的裸 `rm -f` 在 `set -e` 下同樣帶走整支 |
| blocker：poll interval 非 UTF-8 | `env::var().unwrap_or_default()` 把它壓成「未設定」→退 1.0，evict 因此把 config 錯誤誤判成真逾時 | 改 `var_os`＋三態；非 UTF-8 走「值不合法」 |
| `//` 的空字串語意 | `jq_raw_field` 把 `""` 併進 None，鏈式 fallback 會多掉一層（idle 對 `spawned_at: ""` 印秒數，bash 印 `-`） | 新增 `json::jq_alt`（逐字 `//`），idle／`read_field`／`disposable_effective` 改用它；`jq_raw_field` 維持 `// empty` 形狀給 M2 的 hook 欄位 |
| `printf %q` 只吃 `&str` | proxy／PASS_ENV 值與 hooks 路徑先 lossy 再引號化，非 UTF-8 位元組變 U+FFFD | `shell_quote` 改收 `&[u8]`（`$'\NNN'` 產物是純 ASCII，故不必把啟動指令改成 bytes）；brief 路徑因為是**單引號字面值**無法跳脫，改 fail-closed 拒絕（相對 bash 的刻意偏離，方向是大聲失敗而非靜默注入錯誤守則） |
| ready interval 可 panic | `Duration::from_secs_f64(inf)` 在「registry 已寫、回滾已解除」之後 panic | 改 `try_from_secs_f64`，不可表示時退 `Duration::MAX`（＝bash 把超大值交給 `sleep` 的同一終態） |
| spawn tag 的熵可預測 | 沿用 `task::rand_suffix`，`/dev/urandom` 失敗時退回 pid⊕nanos——但 tag 是 despawn 的殺人依據 | 另立 `secure_hex12()`，讀不到熵直接 `Err`（bash 也是在建 pane 前死） |
| split 失敗仍重排 | `select-layout` 在傳播錯誤前執行 | 先 `?` 取 pane 再 layout（對齊 `:1299-1301`） |
| evict stderr 非逐字 | 內層 `cmd_send` die／`cmd_await` 逾時行被吞 | 兩處先 `err_line` 內層訊息再印 evict 的中止／逾時行 |

rubric 6（harness 改動正當性）codex 判 CONFIRMED，並建議 M4 讓
source-contract checker 顯式接收 `source-kind/source-path`，可行處優先改成
行為測試——記入 M4 待辦。

### 9.6 已收掉與仍開著的缺口

- **已收（M2 遺留）**：`--message` 的非 UTF-8 位元組。`cmd_send`／`cmd_reply`／
  `cmd_fail` 改收 `&[OsString]`，`MessageSource::Text` 帶 `Vec<u8>`，payload
  原樣落檔（架構 §3）。其餘指令的參數是 id／名稱／旗標，續用 lossy 視圖；
  錯誤文案裡的值一律 lossy（給人看的字串，不是 payload）。
- **仍開**：SIGPIPE 沒有機器 gate（§5 已記），M3 未新增測試組——那需要在
  套件裡加一組新分組，屬行為錨（spec/tests）的擴充而非 parity 工作。
- **仍開**：`meta_str` 對「欄位存在但型別非字串」回空字串（M1 起的已知差異）。
  M3 新寫的讀取面（`registry::read_field`、`task::last_task_at`、
  `spawn::idle`）一律走 `json::jq_raw_field`（`jq -r` 語意）；`meta_str` 未一併
  改，因為它的呼叫端（receive/read 標頭、`respond_task` 找 sender）另有「缺欄位
  印字面 `null`」的對齊要求，兩套語意合併需要各自的測例，留給 M4 與
  `--contract-manifest` 的形狀討論一起收。

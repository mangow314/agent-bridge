# agent-bridge

> English overview: [README.md](README.md) — 本檔為正典手冊：定位、操作、
> 完整已知限制。介面契約在 [spec/](spec/)，設計論證與量測在 [docs/](docs/)。

**AI coding agent 之間的任務信箱與交接協定。**

multiplexer 讓你一次看見所有 agent；agent-bridge 是它上面那層——讓 agent
之間**互相委派**：把一塊自足的活交出去、拿回誠實的答案（包含「我做不到」），
並讓那份工作活過發起它的 session 被清洗、壓縮或換人。委派的單位是**任務**
（檔案上的狀態機）而不是 pane。

![示範：真實 Claude Code session 就地 spawn codex worker、委派任務、讀回覆](docs/assets/hero.gif)

*只打一句人話 prompt，其餘全是兩個活 agent 自己的動作：真實 Claude Code
session `spawn` 出 codex worker（`--here` 預設同窗彈出）、`send` 任務、
`read` 回覆——這一輪還恰好踩到 worker 忙碌路徑：通知遞延、由 worker 的
Stop hook 自行從 mailbox 取件。真 API 錄製，等待時間以後製摺疊；劇本見
[docs/demo/](docs/demo/) 的 `real-*`（stub 版 tape 保留，供免 API 的可重現
錄影用）。*

<details>
<summary><b>更多情境錄影</b>——多輪討論 · 跨廠審查 · relay 交棒（同為
真實 session 錄製）</summary>

### 多輪討論——脈絡留在 pane

![示範：對同一個 worker 連問兩輪，第二輪不必重講前情](docs/assets/discussion.gif)

追問送進同一個 pane，第二輪不必重述第一輪：worker 的脈絡活在它自己的
pane，不佔你的 session 視窗。

### 獨立審查回合——結構上就是跨廠

![示範：codex pane 獨立審查未提交 diff 並回附驗證的裁決](docs/assets/review.gif)

主導 session 把審查委派給另一家廠牌的 pane；裁決以可留檔的任務紀錄回來，
而不是一段會被捲走的 scrollback。

### relay——把主導權交棒出去

![示範：task 還在跑就 relay 交棒，接手者收割前任派出的回覆](docs/assets/relay.gif)

主導 session 的 context 吃緊時：先派工、不等結果，寫好交接檔、`relay`
一個接棒 session——task 跨越交棒仍在跑。接手者讀交接檔、查 `status`、
收割前任派出的回覆；worker 與任務都原地活過了交棒。

</details>

## 為什麼不是內建 subagent

要「把活丟出去、拿回結論」，用內建 subagent 就好——更省、更簡單。
agent-bridge 只在你需要**至少一件**它做不到的事時才有意義：

1. **跨供應商**：worker 可以是 codex，orchestrator 是 claude（反之亦然）。
   內建 subagent 換不了廠。
2. **人類可視、可介入**：worker 跑在真的 tmux pane，看得到它此刻在做什麼，
   隨時切進去接手。subagent 只給你最後的結論。
3. **活過主 session 的清洗**：worker 的 context 活在自己的 pane 裡，主
   session `/clear`、被 compact、整個重開都不動它。
4. **再往下委派（第三層）**：worker 自己是完整 session，經授權能再 spawn
   自己的 bridge worker——下一層跟它同級：跨廠、可視、留著追問。（Claude
   Code 的 subagent 設 `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` 也能巢狀，
   但每層仍是同 runtime 的黑盒，逐層只回摘要。）

一句話：agent-bridge 是一層**活過主 session 清洗的 context**。這也是取捨
裁判——提議若不服務上面四件事之一，就不屬於這裡。

## 跟其他多 agent 做法的差別

**官方 agent teams**（Claude Code 實驗功能，開
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS` 才有）解的是另一個問題：一個 session
裡開一隊 Claude 協作。它的協調機制豐富得多——共享任務清單、隊員互傳訊息、
self-claim，也支援分割 pane 點進去介入。差別在三個硬邊界（v2.1.178
[官方文件](https://code.claude.com/docs/en/agent-teams)）：全隊只能是
Claude；lead 終身固定，`/resume` 不會把 in-process 隊員帶回來；隊員不能再
開自己的隊。agent-bridge 的 worker 是各自獨立的 CLI session：可以是
codex，不掛在任何 lead 底下，`relay` 就是把主導權交給下一棒，經授權還能
往下再開一層。取捨很直接——全用 Claude、要緊密協作，開 agent teams；要
跨廠、要 worker 活得比主 session 久，才輪到 agent-bridge。

**MCP 跨呼叫**（把 codex 包成 MCP server 給 claude 呼叫）：同步請求—回應，
呼叫方整個 turn 卡著等，回覆全文直接灌進 context——這正是想避免的事。
`send` 丟出去就走，回覆躺在 mailbox，要讀再 `read`。

**API 級框架**（LangGraph、AutoGen 這類）：編排的是 API 呼叫，key、tool、
context 都要自己管。agent-bridge 編排的是你手上在用的那個 CLI——worker
繼承各自 CLI 的設定、權限、hooks，跟你自己開 pane 敲指令同一件事。

## 架構

三個組成，全部本機、無常駐程序：

1. **薄 CLI**（`bin/agent-bridge`，exec shim 接到 `bin/ab`）：唯一進入點。
2. **檔案系統 mailbox**（預設 `~/.local/share/agent-bridge/`，可用
   `AGENT_BRIDGE_DATA` 覆蓋）：`agents/` 註冊表、`agents.log` append-only
   審計流、`tasks/<task-id>/` 每任務一目錄（request 與 response 原文
   byte-for-byte 保真）、`locks/` 狀態轉換用的 mkdir 鎖。欄位與事件詞彙表
   的權威定義見 [spec/state.md](spec/state.md)。
3. **通知：runtime 原生 hook 為主，tmux send-keys 為輔**。send / reply /
   cancel 寫完檔案後經 `notify_or_defer` 通知對方：已接 hooks 的 worker
   若 state 檔顯示新鮮的 `busy`（未超過 `AGENT_BRIDGE_STATE_TTL`，預設
   1800 秒），完全不送鍵、只記 `notify-deferred`——訊息已在 mailbox，對方
   的 Stop hook 在 turn 結束時自行取件。claude 與 codex 走同一條 hook 命令
   （實測 codex 0.145 的 Stop payload 與 Claude Code 同構）。state 通道
   整體是「建議非權威」：缺失、解析失敗、過期，一律退回 send-keys。

   **send-keys 路徑**（idle 喚醒、無 hook worker 的 fallback）：只送一行
   `agent-bridge receive <task-id>`（或 `read <task-id>`）加 Enter，訊息
   內容永遠走檔案。文字與 Enter 拆兩次送、隔 0.3 秒
   （`AGENT_BRIDGE_NOTIFY_DELAY` 可調）——REPL 會把同批文字+Enter 當貼上而
   吞掉 Enter。送鍵前先 `capture-pane` 確認對方沒停在權限框，有就降級
   notify-failed（見「已知限制」）。決策全圖見
   [docs/rust/flow-notify.md](docs/rust/flow-notify.md)。

狀態機：

```
queued --receive--> delivered --start--> running
delivered | running --reply--> completed   （終態）
delivered | running --fail---> failed      （終態）
queued | delivered | running --cancel--> cancelled（終態）
```

- 非法轉換（未 receive 直接 reply、對終態再操作）一律 stderr 報錯、非零
  退出、狀態檔不變；`receive` 對 delivered / running 冪等（記 `re-receive`）。
- `start` 可選：讓 sender 的 `status` 分得出「已送達」與「正在做」。
- `fail` 與 `reply` 同構：失敗原因寫入 `response.md`，`read` 可讀。
- `cancel` 由 sender 使用；已 cancelled 後 worker 的 reply / fail 被拒，
  worker pane 收到一行 `agent-bridge status <task-id>` 通知。

## 安裝

硬依賴：`tmux`，加上 shim 用的 `bash` 與建置用的 Rust toolchain
（edition 2024）。跑測試套件另需 `jq`。

```bash
git clone <repo-url> ~/projects/agent-bridge
cd ~/projects/agent-bridge
cargo build --release && cp -f target/release/ab bin/ab
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # 應解析到 symlink
```

執行檔必須放在 `bin/` 底下：`share/` briefs 的預設路徑是從它的祖父層目錄
反推的。

### 升級既有的 clone（多裝置必讀）

`bin/ab` 是建置產物、**不進版控**，每次 pull 之後都要重建：

```bash
git pull
cargo build --release && cp -f target/release/ab bin/ab
agent-bridge list   # 冒煙測試
```

漏了重建，shim 會 **exit 127 並印出建置指令**，而不是靜默降級——入口沉默
地做別的事，比直接壞掉更難查。（bash 正本自 M4 凍結後已自樹上移除；留下的
防線是「載具身分必須實測」：`$BRIDGE` 指到不認得 `__implemented-commands`
的東西一律非零退出。）

Claude Code 的委派協定 skill：把**整個 repo** symlink 成 skill 目錄
（SKILL.md 以相對路徑引用 `share/` 的 briefs，必須同目錄才解析得到）：

```bash
ln -s ~/projects/agent-bridge ~/.claude/skills/agent-bridge
```

## 指令

按工作流分組；精確 argv 與 MUST 語意的權威在 [spec/cli.md](spec/cli.md)。

**派任務（sender 側）**

| 指令 | 用途 |
|---|---|
| `send <agent> --from <me> --message-file -` | 委派；stdout 只印 task-id 一行 |
| `status <task-id>` | 只印裸狀態字 |
| `await <task-id> [--timeout <secs>]` | 唯讀輪詢至終態；逾時 exit 124 |
| `read <task-id>` | 讀回覆原文（completed 與 failed 都可讀） |
| `cancel <task-id>` | 取消（非搶佔，只翻狀態＋通知） |

**接任務（worker 側）**

| 指令 | 用途 |
|---|---|
| `receive <task-id>` | 取件：標頭走 stderr、request 原文走 stdout |
| `start <task-id>` | （可選）標記開工：delivered → running |
| `reply` / `fail <task-id> --message-file -` | 回覆／回報失敗（訊息＝失敗原因） |
| `ready <name>` | 回報就緒（spawn 的探針自動呼叫） |
| `disposable <name>` | 宣告本輪脈絡已無殘值，可即時回收 |

**worker 池（orchestrator 側）**

| 指令 | 用途 |
|---|---|
| `spawn <name> --runtime <codex\|claude\|agy> [--model <m>] [--here\|--window]` | 開 worker pane；stdout 只印 pane-id |
| `relay <name> --runtime … --handoff <path>` | 把主導權交給接手者（見 relay 節） |
| `register` / `unregister <name> [target]` | 手動掛入／移除既有 pane |
| `list [--long]` | 池況；`--long` 八欄含 where/owner/disposable/idle |
| `despawn <name>` | 回收 spawn 出身的 worker（人工註冊拒殺） |
| `evict <name> [--timeout <secs>]` | 派收尾任務 → 等筆記落地 → despawn |
| `idle` | 回收決策視圖 |
| `gc [--older-than <days>] [--apply]` | 清舊終態 task；預設試算 |
| `scan` / `ui` | 呼叫器掃描／alternate-screen 看板（Rust 獨有） |

跨指令規則：

- stdout 是機器可解析的最小輸出（task-id、pane-id、裸狀態字），其餘訊息
  走 stderr；所有錯誤路徑非零退出。
- 多行內容走 `--message-file -`（stdin heredoc），不塞 `--message`。
- `read` 對未回覆報「尚未回覆」、對 cancelled 報「已取消」，都非零退出；
  `await` 對三種終態都印狀態字並 exit 0，逾時且僅逾時 exit 124。
- `await` 與 `status` 唯讀（不取鎖、不寫事件），只讀 sandbox 也能用；輪詢
  間隔 `AGENT_BRIDGE_POLL_INTERVAL`（預設 1 秒）。
- agent 名（含 `--from`）限 `[A-Za-z0-9_-]+`。

## 兩個 pane 完整走一遍

假設 `%1` 是 planner、`%2` 是 worker（各自跑一個 claude）：

```bash
# 兩邊各自（或由任一邊）註冊；target 可用 %pane_id、session:window.pane 等寫法
agent-bridge register planner %1
agent-bridge register worker  %2

# planner 委派任務（多行內容走 stdin）
id=$(agent-bridge send worker --from planner --message-file - <<'EOF'
請幫我跑 tests/run-tests.sh 並回報失敗案例。
限制：不要改任何檔案。
EOF
)

# worker 的 pane 自動收到一行 `agent-bridge receive <task-id>`，
# request 原文出現在輸入流。worker 完成後回覆：
agent-bridge reply "$id" --message-file - <<'EOF'
測試全過（294 PASS / 0 FAIL），未修改任何檔案。
EOF

# planner 的 pane 自動收到 `agent-bridge read <task-id>`；也可手動查：
agent-bridge status "$id"   # completed
agent-bridge read "$id"     # response 原文

# 不想依賴 send-keys 通知（例如 worker 的 sandbox 發不出通知）時，
# 改在背景等終態，回來的就是裸狀態字：
agent-bridge await "$id" --timeout 600   # completed / failed / cancelled
```

通知失敗（對方 pane 不在了、tmux 不可用）時：檔案與狀態照常完成、exit 0，
stderr 印含 task-id 的警告，人工在對方 session 補跑 `receive` / `read` 即可。

## spawn／despawn：worker 生命週期

`spawn` 讓 orchestrator 直接開一個 worker pane 並註冊：

```bash
pane=$(agent-bridge spawn worker-1 --runtime codex)
agent-bridge list              # worker-1  %N  starting → ready
agent-bridge despawn worker-1  # 收尾：kill pane＋除名＋審計
```

**落點**：`spawn` 接受互斥的 `--here`／`--window`；兩者同時給出、或 here
layout 壞值，MUST 在建 pane 前拒絕、不留 pane／registry 痕跡。不給旗標時
auto 判定三選一：

- **呼叫者是人工 session**（`AGENT_BRIDGE_SPAWN_TAG` 未設定或空字串）且
  tmux 呼叫者可解析→落 `--here`：split 進呼叫者當前 window，依
  `AGENT_BRIDGE_HERE_LAYOUT` 重排——預設 `main-vertical`，合法值另有
  `main-horizontal|tiled|even-vertical|even-horizontal|none`（`none`＝只
  split 不重排）；空字串／非 UTF-8／表外值一律致命壞值，但只在真的會走
  here 落點的路徑上驗證——`--window`、下面的「spawn 出身」分支、tmux 外
  呼叫都不解析它，無關的壞值不封鎖這些逃生口。這個落點 MUST NOT 查或寫
  worker-window reuse 的信任根 `@ab_owner`，registry 的 `worker_window`
  欄留空。
- **呼叫者是 spawn 出身**→維持舊行為：同一 owner 既有 worker window
  （緊鄰其後、名為 `ab:<orchestrator window 名>`、tiled 均分）存活且其
  `@ab_owner` 印記等於本次解析出的 owner 才沿用（registry 內容本身不足以
  授權沿用，防 confused-deputy）；否則緊鄰 owner 視窗新建一個。
- **tmux 外呼叫**（含顯式 `--here` 但沒有可解析呼叫者）→退回「目前視窗
  往下 split」，不視為錯誤。

`AGENT_BRIDGE_SPAWN_TAG` 在這裡只是落點 provenance 的訊號、不是身分授權：
判斷錯了頂多是版面 UX 亂了，人可用 `--here`／`--window` 顯式修正，不為此
新增信任根。`--window` 一律開專屬視窗，不被後續 spawn 沿用。relay 的落點
規則與 spawn 完全共用（見下方 relay 節）。

**runtime 表**：

- `codex` → `codex --profile agent-worker`（approval never＋workspace-write，
  需自行加進 `~/.codex/config.toml`；樣板與理由見
  [docs/codex-worker-approval-proposal.md](docs/codex-worker-approval-proposal.md)）。
  hook 需要人互動式授信一次，見「已知限制」。
- `claude` → `claude --permission-mode auto`，並以
  `--settings share/claude-worker-hooks.json` 注入 worker 專屬 hooks（路徑
  可用 `AGENT_BRIDGE_CLAUDE_HOOKS` 覆蓋，指向合法空 JSON 即停用 state
  通道、退回 send-keys）。**不可用 `-p/--print`**：headless 跑完即退出，
  pane 留不下來。權限模式的取捨見下方護欄段。
- `agy` → `agy --dangerously-skip-permissions --sandbox [--model <m>]
  --prompt-interactive`（Antigravity CLI，量測正本
  [docs/agy-probe.md](docs/agy-probe.md)）。旗標姿態是使用者裁決：agy 只有
  粗粒度旗標；sandbox 實測不擋 `agent-bridge` 呼叫與專案內寫檔。⚠️
  `--prompt-interactive` **必須排在最後**：它吃下一個 token 當值，擺在
  `--model` 之前會把模型旗標吞掉、真 prompt 錯位。agy（實測 1.1.9）沒有
  hooks 介面，通知恆走 send-keys；權限框特徵見「已知限制」。
- 新增 runtime 前必須實測：位置參數確實被當第一則 user message 執行、且
  執行完 session 常駐。只吃 stdin 或需要別的旗標的 CLI 要另外長出注入方式。

**claude 權限模式護欄**：**auto** 是刻意的——讓 worker 零人工介入執行探針
（實測確認）；**不用 `bypassPermissions`**，理由只取
[官方文件](https://code.claude.com/docs/en/permission-modes)明載的部分：
該模式定位為「Isolated containers and VMs only」，而 worker pane 跑在
本機、與主 session 共用檔案系統與憑證；`auto` 保留背景安全檢查，protected
paths 在除 bypass 外的所有模式都不會被自動核准。⚠️ 別把理由寫成「bypass
會繞過 hooks／讓 ask 消失」——**都不對**：停用 hooks 的是另一個旗標
`--bare`，而 deny 與 explicit ask 規則官方明載「apply in every mode,
including `bypassPermissions`」。這段因果在本檔前後寫錯過兩次，都是獨立
複核對照官方文件抓出來的。hooks 分層是合併不是覆蓋（故**不加
`--setting-sources`**），全域安全設定原樣繼承。

**`--model` 指定 worker 的模型**（三個 runtime 都吃長旗標；codex／claude
實測 2026-07-23、agy 實測 2026-07-31）：不給＝繼承該 CLI 的使用者預設；
「規劃在主 session、執行下放」靠這個旗標落地（策略見
[share/orchestrator-brief.md](share/orchestrator-brief.md)）。值會拼進啟動
命令，驗 `[A-Za-z0-9._-]{1,64}` 且首字元英數（否則 `--model --bare` 等於
塞任意開關），不合法在建 pane 前拒絕。⚠️ claude 對輕量級模型會把 auto
**靜默降回 manual**，worker 卡死在權限框（2026-07-23 實測 Haiku 4.5）。

**proxy 環境穿透**：pane 的環境繼承自 tmux server，不是執行 spawn 的程序，
proxy 變數到不了 worker。故 spawn 把呼叫者環境裡**有設定的**標準 proxy
變數以 `printf %q` 跳脫後拼進啟動指令；跳脫防意外拆詞，不是防注入。

**指名穿透**（`AGENT_BRIDGE_PASS_ENV`，逗號分隔變數名）：同一條路徑的白名
單版，典型用途是 headless 姿態旗標——`CLAUDE_UNATTENDED=1` 不跟著過去，
接手者 pane 會靜默退回有人值守的寬鬆姿態。只帶有設定的變數、逐個驗名、
不合法 fail-closed；保留變數（`AGENT_BRIDGE_SPAWN_TAG`、
`AGENT_BRIDGE_RELAY_DEPTH`）靜默剔除＋警告，否則子代會頂著呼叫者的 tag
開起來。**不要拿來傳秘密**：值會出現在 pane 啟動指令裡；白名單也只能由
可信的 orchestrator 設定——`PATH`／`LD_PRELOAD` 這類變數足以改變 runtime
行為。

**啟動即注入 worker 守則**：spawn 出來的是零脈絡的全新 session，沒有守則
它會把 `receive` 通知當成對話（實測 codex 0.145 反覆回覆、永遠不 ready）。
啟動指令把 `share/worker-brief.md` 全文當 initial prompt 帶進去；**該檔是
worker 契約的唯一正本**，人工註冊的 worker 讀同一份，路徑可用
`AGENT_BRIDGE_WORKER_BRIEF` 覆蓋。

**就緒自報到**：spawn 註冊時 `ready: false`，以間隔重送探針
（`AGENT_BRIDGE_READY_TIMEOUT` 預設 30 秒、間隔 2 秒）。**逾時不回滾、僅
警告**，pane 留用供人工診斷（實測 codex 約 7 秒就緒）。對 `ready: false`
的 agent `send` 合法：警告但不拒送，通知可能延後。

**安全不變量**（完整論證見
[docs/lifecycle-safety.md](docs/lifecycle-safety.md)）：

- spawn cap（`AGENT_BRIDGE_MAX_SPAWN`，預設 4）在 registry 鎖內檢查，並行
  繞不過；人工註冊不計入。每筆 spawn/despawn 都落 `agents.log`
  （append-only，格式見 [spec/state.md](spec/state.md)）。
- 只有 spawn 出身能被 despawn；殺之前用一次性 `spawn_tag` 驗 pane 出身
  （pane id 會被 server 重發），驗證與 kill 原子完成。對不上時不動 pane、
  只清註冊，審計記 `despawn-stale`。
- 判不出來就不動手：registry 壞檔拒操作但照佔 cap；tmux 查不到或殺不掉
  一律保留 registry，不製造孤兒。spawn 中途失敗原子回滾（含夭折偵測與
  孤兒掃除）；殘留 ghost registry 用 `despawn` 清。
- brief 檔不是可讀普通檔案就不開 pane（檢查與讀檔間有已知 TOCTOU 空隙，
  刻意不修，見論證檔）；路徑含單引號一律拒絕。

### relay：把主導權交給下一棒

```bash
agent-bridge relay <name> --runtime <codex|claude|agy> [--model <m>] \
  --handoff <path> [--here|--window] [--no-select] [--self-exit <my-name>]
```

`spawn` 開的是**等派工的 worker**；`relay` 開的是**接手者**——一開始就拿到
交接檔路徑，讀完自己往下做，不等 `receive`。除了注入
`share/successor-brief.md` 與切焦點，pane 生命週期與落點解析（`--here`
同適用，auto 三選一判定同一套規則，見上方「落點」段）與 `spawn` 完全共用。

- `--self-exit <my-name>`：請接手者回收前一棒——自己殺自己的 pane 會被
  SIGHUP 帶走、審計線斷掉（論證見 lifecycle-safety.md）。第一棒是人工
  session 時 despawn 會拒絕，接手者守則明說這是正常的、不要繞。
- `--no-select` 是 orchestrator 驅動時的常態：主導者是 agent，切焦點沒有
  意義；互動接力則預設切過去。
- **接力鏈有深度上限**（`AGENT_BRIDGE_MAX_RELAY_DEPTH`，預設 10，`0`＝
  不設限）：接手者守則鼓勵「context 吃緊就交棒」，沒有上界就是無界遞迴。
  深度由 `AGENT_BRIDGE_RELAY_DEPTH` 逐棒下傳（人工第一棒＝深度 0），達
  上限在建 pane 前擋下。兩個變數只接受 1–9 位十進位數字，**空字串一樣
  被拒**——空值若被當預設等於靜默重置鏈深度。pane 內可自行改寫變數繞過：
  這道 cap 擋失控迴圈，不擋蓄意繞過。

## ui／scan：看板與呼叫器（Rust 獨有）

兩支分別回答：**我想看時看得到**（`ui`）、**我沒在看時它會來找我**（`scan`）。

**`ui`**：alternate-screen 看板（`q` 離開；agent session 不要跑它，會佔住
終端）。WORKERS 依 spawn lineage 分組（relay 一次物理位置就斷，血緣不會），
TASKS 列在飛任務，DETAIL 給 breadcrumb（缺代省略號、已除名留墓碑 `†`）；
`Enter` 跳到該 worker 的 pane，`x` 取消 task。讀盤為主、tmux 為有界側查
（`AGENT_BRIDGE_TMUX_TIMEOUT` 預設 5 秒）：tmux 卡住時退化的是死活欄，
不是整個畫面。設計正本見 [docs/tui-design.md](docs/tui-design.md)。

**`scan`**：事件恰兩類，不打算擴充——**task 進了 `failed`**，與 **pane 死
了卻還掛著非終態 task**。每則先落盤 `state/page-events.jsonl`，再沿階梯推
播一次：`AGENT_BRIDGE_NOTIFY_CMD`（可執行檔，argv 接 `<title> <body>`，
不經 shell）→ 桌面 notify-send（SSH 下跳過——遠端 `DISPLAY` 是沒人看的
螢幕）→ tmux status line。保證強度是 **durable record＋至多推播一次**：
去重軸是帶世代的 event key，notifier 壞掉就作罷，事件仍在磁碟上。每個
非唯讀且成功的子指令都順手掃一輪，顯式 `scan` 留給 tmux hook／cron。通知
標題帶「誰、出了什麼事、人在哪」，內文首行是失敗原因本身——這個形狀是實測
打出來的：受測者卡在「要不要切過去看」，因為通知從沒說過切去哪裡。

## disposable／idle／evict／gc：pane 的去留

worker 做完一件事不代表它該死——腦裡可能還留著沒寫進 response 的東西。
**預設保留**，判定者是 worker（`disposable` 宣告），最終回收權在
orchestrator。

```bash
agent-bridge idle                    # 看池況，挑一個
agent-bridge evict w1                # 派收尾任務 → 等筆記落地 → despawn
agent-bridge read <收尾 task-id>     # 讀它留下的筆記
```

- `disposable` 是建議不是保護；宣告後又被派新任務會自動轉 `expired`。
- `evict` 逾時**仍然 despawn**——否則不回話的 worker 把 cap 永久卡死；代價
  是筆記沒落地，審計分三種記號（見下表）。`spawn` 不會自動驅逐：殺與不殺
  永遠是獨立、可審計的決定。
- `gc` 預設試算、`--apply` 才刪；未完成的、evict 收尾筆記（`pinned`）、判
  不出年紀的、「宣告已失效」的證據任務一律不刪——刪了宣告會從 `expired`
  復活成 `yes`（論證見 [docs/lifecycle-safety.md](docs/lifecycle-safety.md)）。

`agents.log` 的回收記號，分得出這次回收有沒有丟東西：

| 記號 | 意思 |
|---|---|
| `despawned` | 乾淨回收：worker 宣告過 `disposable` 且仍有效 |
| `despawned-unsaved` | 回收了仍被視為有殘值的 worker——繞過了收尾流程 |
| `despawn-stale` | 註冊清掉了，但 pane 已不屬於這個 agent，未被回收 |
| `evicted` | 走完 evict，收尾筆記已落地 |
| `evicted-unfinished` | 收尾任務以 failed／cancelled 收場 |
| `evicted-timeout` | 等到逾時，pane 仍回收，筆記沒落地 |

策略層守則見 [share/orchestrator-brief.md](share/orchestrator-brief.md)
（主導者）與 [share/worker-brief.md](share/worker-brief.md)（worker）。

## 測試

```bash
tests/run-tests.sh
```

驅動腳本是 bash，需要 `tmux` 與 `jq`；整合測試用獨立 socket（`tmux -L
agent-bridge-test -f /dev/null`），不碰使用者真實 tmux server。

## 開發慣例

- **chezmoi 側變更走 topic branch + worktree**（2026-07-22 起）：配套變更
  一律在 chezmoi repo 開 topic branch 用獨立 worktree 作業，審過再併回。

## 已知限制

### 看板與通知介面

- **看板的歸屬可讀性尚未通過理解驗收**（2026-08-04，PD1 第一輪 1/3）：
  資料是對的（有機器不變式守著），但人讀不讀得出來是另一回事。三個未修
  缺口：`[unattached]` 不區分三種成因、直系 parent 是二級資訊、墓碑鏈缺
  兩代只看得見一代。詳見 [docs/tui-design.md](docs/tui-design.md) §9。
- **通知的 Enter 可能誤按對方的權限對話框**（2026-07-23 實測誤觸）：worker
  停在權限框時，send-keys 的 Enter 等於替它按下預設選項。防護是送文字前、
  送 Enter 前各 `capture-pane` 掃一次特徵（capture 失敗即 fail-closed
  降級）。特徵涵蓋 claude 權限框、plan mode 退出框（誤觸會把 worker 切進
  auto mode，比權限框更糟）；agy 的框 footer 是**小寫** `esc to cancel`，
  另加 header 錨與下緣備援對（pane 太矮時 header 捲出可見範圍，回歸鎖在
  分組 37e）。**侷限（刻意揭露）**：(1) 字串匹配對可見文字，改文案或本地
  化即失效，方向 fail-open——漏判比偽陽性（通知延後、可復原）更糟；
  workspace trust 框拿不到第一手特徵，不涵蓋。(2) 第二次掃描與 send-keys
  之間仍有微小 race。codex worker 走 approval never、不彈框，不受影響。
- **registry 讀取與 send-keys 之間有極小 race**：send 發通知前會重讀
  registry 取最新 pane，但兩步無法原子化，通知仍可能打進已易主的舊
  pane。只影響那一發按鍵；訊息本體在 mailbox。

### 通知原生化（state 通道）

- **agent 忙碌時通知會延後處理**：hook 接線的 worker 若 state 新鮮且
  `busy`，通知不送鍵、由對方 Stop hook 在 turn 結束取件。殘餘限制：
  (1) codex 的 hook 需要人互動式授信一次，未授信被 codex **靜默略過**
  （無警告無錯誤）；(2) hook 中途掛掉（stdin 非 JSON、缺 `jq`、state 寫
  不進去）時 state 停在舊的 `busy`，要等 TTL（預設 1800 秒）過期才退回
  send-keys；(3) 都不遺失訊息，只是即時性沒有保證。
- **巢狀 runtime 冒用 parent 身分（已修：owner/session_id 所有權閘門）**：
  worker session 裡再啟動一個載入同組 hooks 的 runtime，其 hooks 曾會用
  parent 的名字寫 state、甚至攔走任務。現行修法：state 檔記 `owner`（hook
  payload 的 `session_id`），先到先得認領，id 不符靜默擋下；owner 交還走
  TTL 時間窗（也是 `/clear` 換 id 後的自癒路徑）。殘餘邊界（刻意不美化）：
  (1) `/clear` 後最長一個 TTL 內 parent 的 hooks 被舊 owner 鎖住——**M5 對
  claude worker 關掉了這個窗**（hook 比對 runtime 的 `(pid, starttime)`
  即時放行）；**codex 不適用**（npm 的 node launcher 讓行程樹多一層，PPID
  對不上，落回 TTL 窗）；(2) 長壽巢狀 session 存活超過 TTL 且期間 parent
  零 hook 事件，仍可能奪走所有權；(3) 缺 `session_id` 的 payload 不參與
  state 通道，退回送鍵；(4) `owner` 欄與 state 檔同屬 worker 可寫互信域，
  防意外汙染不防蓄意偽造。完整分析與補測見
  [docs/codex-hooks-probe.md](docs/codex-hooks-probe.md)。
- **`notification_type` 欄位有可靠性缺口**：官方 payload 有時整個缺這欄
  （[issue #12048](https://github.com/anthropics/claude-code/issues/12048)，關聯 [#11964](https://github.com/anthropics/claude-code/issues/11964)）。
  fail-safe 是缺欄位不等於 `idle_prompt`、不動 state；代價是 state 可能停
  在舊值，TTL 到期後仍退回送鍵，不會永久卡住。
- **state 檔在 worker 可寫的資料目錄**：同 uid 的任何 worker 都寫得到別人
  的 state 檔。這條通道語意本來就是建議非權威。
- **hook 靠 `AGENT_BRIDGE_SPAWN_TAG` 析出身分**：讀不到就 no-op。人工
  register 的 worker 沒有這個變數，天然走 send-keys 路徑。
- **Stop hook 的續跑依賴 Claude Code 的 `decision: block` 語意**：語意若
  改變，失效方向是不再自動取件、退回 send-keys 或人工取件，不遺失資料。

### tmux 與生命週期

- **tmux server 重啟後 pane_id 全部失效**：重新 `register`；spawn 出身的
  用 `despawn` 清殘留 registry——新 pane 可能拿到同一個 `%N`，despawn 靠
  `spawn_tag` 認得出來，不會誤殺。
- **tmux 行為的驗證版本是 3.7b**：回滾掃孤兒依賴 `pane_start_command` 的
  存法；測試以不變量斷言鎖住，換版本改存法會直接測出來。未逐版驗證，故
  不宣告最低版本號。
- **ready 是自報到，不是健康檢查**：`ready: true` 只代表 REPL 曾執行過
  探針，探針重送只涵蓋「啟動期按鍵被吃」；逾時後 pane 留用，`list` 一直
  顯示 `starting`。
- **鎖可能殘留**：mkdir 鎖靠 `trap … EXIT` 清，SIGKILL／斷電時不執行，
  `locks/<id>.lock` 殘留、同類命令重試 25 次後失敗。刻意不做 stale-lock
  自動回收（會引入「誤刪別人正持有的鎖」這個更糟的失效方向）：確認沒有
  agent-bridge 在跑，手動刪殘留目錄。

### 信任域與 sandbox

- **cancel 是狀態宣告，不是搶佔**：只翻狀態並通知，執行中的 worker 不會
  被中斷。
- **request 對 receiver 是不可信輸入**：跨 agent 的 prompt injection 面，
  receiver 把內容當資料不當指令（見 [SKILL.md](SKILL.md)）。
- **同一個資料目錄＝同一個互信域**：所有 agent 同 uid、共用
  `AGENT_BRIDGE_DATA`，registry 對每個 worker 可寫。`spawn_tag` 擋**意外**
  殺錯與**跨 worker 借刀**，擋不住能連 tmux socket 的惡意 worker——它直接
  `tmux kill-pane` 就行；真正的邊界是 socket 存取權。**worker brief 也在
  互信域內**：被控制的 worker 可改寫它左右下一個 worker 的啟動 prompt；
  想收緊就把 brief 放唯讀位置（連同父目錄）並以
  `AGENT_BRIDGE_WORKER_BRIEF` 指過去。
- **通知協定假設 `agent-bridge` 在對方 pane 的 PATH 上**（裝好 symlink 即
  成立）。
- **runtime 的 sandbox 必須允許寫資料目錄**：codex `workspace-write` 預設
  擋 workspace 外寫入，要在 `[sandbox_workspace_write] writable_roots` 加
  `AGENT_BRIDGE_DATA` 路徑，否則 receive 建鎖失敗（status 唯讀不受影響）。
- **sandbox 擋 socket 時該 agent 發不出通知**：codex `workspace-write` 預
  設 `network_access = false`，seccomp 擋 `connect()`（含 unix socket，
  `writable_roots` 救不了）。收任務、reply 都正常，只有通知走降級。
  **建議 sender 端用 `await`**（唯讀輪詢，不依賴對方發通知）；開
  `network_access = true` 可全自動，但 sandbox 內可對外連網，非必要不建議。

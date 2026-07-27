# agent-bridge

> English overview: [README.md](README.md) — 本檔為完整正典文件（含設計取捨與已知限制的工程程記）。

跨 tmux pane 的 agent 任務委派橋（core MVP）。讓分跑在不同 tmux pane 的多個
claude / codex CLI session 互相委派任務與回覆，把工作拆細、讓每個 agent 的
context 保持短而乾淨。

![示範：spawn worker pane、委派任務、讀回覆](docs/assets/demo.gif)

*示範以 stub runtime 錄製、未打真實 API；錄影劇本與腳本見
[docs/demo/](docs/demo/)。*

## 為什麼不是內建 subagent

要「把一塊活丟出去、拿回結論」，用 agent runtime 內建的 subagent 就好——更省、
更簡單。agent-bridge 只在你需要下面**至少一件**原生 subagent 做不到的事時才有
意義：

1. **跨供應商**：worker 可以是 codex，orchestrator 是 claude（反之亦然）。內建
   subagent 綁在同一個 runtime 裡，換不了廠。
2. **人類可視、可介入**：worker 跑在一個真的 tmux pane，你看得到它此刻在做什麼，
   隨時能切進去接手或糾正。subagent 是黑盒，你只拿得到它最後吐出的結論。
3. **活過主 session 的清洗**：worker 的 context 活在它自己的 pane 裡，主 session
   `/clear`、被 compact、甚至整個重開都不動它。subagent 的生命週期綁在召喚它的
   那一次 turn，主 session 一清就沒了。
4. **再往下委派（第三層）**：worker 自己是完整 session，能再 spawn 它自己的
   worker 或 subagent。Claude Code 的 subagent 巢狀預設關閉、設
   `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` 也開得起來，但長出來的每一層仍是
   同 runtime 的黑盒，逐層只回摘要；worker 的下一層跟它自己同級——可以跨廠、
   可以被看見、可以留著追問。

一句話：agent-bridge 是一層**活過主 session 清洗的 context**。這也是它的取捨
裁判——一個提議若不服務上面四件事之一，就不屬於這裡。

## 跟其他多 agent 做法的差別

**官方 agent teams**（Claude Code 實驗功能，開 `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`
才有）解的是另一個問題：一個 session 裡開一隊 Claude 協作。它的協調機制比
agent-bridge 豐富得多——共享任務清單、隊員互傳訊息、self-claim，也支援 tmux
分割 pane 讓你直接點進去介入，這點跟 agent-bridge 一樣。差別在三個硬邊界
（v2.1.178 [官方文件](https://code.claude.com/docs/en/agent-teams)）：全隊只能是
Claude；一個 session 恰好一隊，lead 終身固定、不能把主導權交出去，session 結束
隊伍就散（`/resume` 不會把 in-process 隊員帶回來）；隊員不能再開自己的隊。
agent-bridge 的 worker 是各自獨立的 CLI session：可以是 codex，不掛在任何 lead
底下，主 session `/clear` 或整個重開都動不到它；`relay` 就是把主導權交給下一棒；
worker 經授權還能往下再開一層。所以取捨很直接——全用 Claude、要的是一場緊密
協作，開 agent teams；要跨廠、要 worker 活得比主 session 久，才輪到 agent-bridge。

**MCP 跨呼叫**（把 codex 包成 MCP server 給 claude 呼叫）：同步請求—回應，
呼叫方整個 turn 卡著等，回覆全文直接灌進呼叫方的 context——這正是想避免的事。
agent-bridge 的 `send` 丟出去就走，回覆躺在 mailbox，要讀的時候再 `read`。

**API 級框架**（LangGraph、AutoGen 這類）：編排的對象是 API 呼叫，key、tool、
context 管理都要自己來。agent-bridge 編排的是你平常手上在用的那個 CLI——worker
繼承各自 CLI 的設定、權限、hooks，跟你自己開一個 pane 敲指令是同一件事。

## 架構

三個組成，全部本機、無常駐程序：

1. **薄 bash CLI**（`bin/agent-bridge`）：二十個子指令，唯一的進入點。
2. **檔案系統 mailbox**（預設 `~/.local/share/agent-bridge/`，可用環境變數
   `AGENT_BRIDGE_DATA` 覆蓋）：
   - `agents/<name>.json`：agent 註冊表（`{name, pane_id, registered_at}`；
     spawn 出身的另有 `spawned: true`、`runtime`、`model`（空字串＝runtime
     預設）、`spawned_at`、`ready`、`spawn_tag`、`owner`（spawn 呼叫者的
     `session:@window_id`，tmux 外為空）、`worker_window`（該 owner 共用
     worker window 的 `@id`；`--window` 專屬視窗與 tmux 外 spawn 為空））
   - `agents.log`：spawn/despawn 審計流，每行固定 6 欄
     `<ISO8601Z> <action> <name> <pane> <runtime> <actor>`，action 詞彙表：
     `spawned`、`despawned`、`despawned-unsaved`、`despawn-stale`、
     `disposable`、`evicted`、`evicted-unfinished`、`evicted-timeout`
     （`actor`＝動作執行者的 `session:@window_id`，tmux 外為 `-`。六欄不變量
     在寫入點保證：全欄空白摺成 `_`、空值補 `-`——pane/runtime 可能取自
     worker 可寫的 registry，欄位安全不靠上游。`actor` 取自呼叫端環境的
     `TMUX_PANE`，屬 best-effort 溯源而非認證——呼叫者可偽造，見「已知限制」）
   - `tasks/<task-id>/`：每個任務一個目錄
     - `metadata.json`：`version`（本輪為 1）、`task_id`、`from`、`to`、
       `created_at`、`updated_at`、`working_directory`、`status`（時間一律
       ISO 8601 UTC）
     - `request.md`：委派內容原文（byte-for-byte 保真）
     - `response.md`：回覆原文（reply 後才存在）
     - `status`：裸狀態字（`queued` / `delivered` / `running` / `completed` /
       `failed` / `cancelled`）
     - `events.log`：append-only 事件流，每行 `<ISO8601Z> <event> [detail]`；
       事件詞彙表固定為 `created`、`notified`、`notify-deferred`、
       `notify-failed`、`delivered`、`re-receive`、`started`、`replied`、
       `failed`、`cancelled`、`read`（`notify-deferred`＝忙碌 worker 的
       Stop hook 會接手取件；`notify-failed`＝送鍵本身沒送成，兩者分開記）
   - `locks/`：狀態轉換用的 mkdir 鎖
3. **通知：runtime 原生 hook 為主通道，tmux send-keys 為輔（通知原生化
   Phase 1/2）**：send / reply / cancel 寫完檔案後要通知對方有新任務，走
   `notify_or_defer` 這道共用閘門。已注入 hooks 的 claude worker（見下）
   若 `state/<name>.json` 顯示 `busy` 且未超過 `AGENT_BRIDGE_STATE_TTL`
   （預設 1800 秒），完全不送鍵、只記一筆 `notify-deferred`：訊息已在
   mailbox，對方自己的 Stop hook 會在 turn 結束時查到並自行 `receive`。
   codex worker **尚未接線這條 hook 通道**（Phase 3 待補），與 claude
   worker 的 state 檔缺失／解析失敗／過期一樣，一律落回下面的 send-keys
   路徑——state 通道整體是「建議非權威」，任何讀不準都以退回 legacy 送鍵
   收場，不會讓通知卡死。

   **send-keys 路徑**（idle worker 的喚醒、無 hook worker 的 fallback）：
   只向對方 pane 送一行 `agent-bridge receive <task-id>`（或
   `read <task-id>`）加 Enter。訊息內容永遠走檔案，絕不進 send-keys。
   文字與 Enter 拆成兩次 send-keys、中間隔 0.3 秒（可用
   `AGENT_BRIDGE_NOTIFY_DELAY` 調整）：agent REPL 這類 TUI 會把同批抵達
   的文字+Enter 當成貼上而吞掉 Enter，導致指令留在輸入框不送出。送鍵前
   還會先 `capture-pane` 掃一眼對方 pane 有沒有停在權限確認對話框——有就
   不送、降級成 notify-failed，避免那個 Enter 替一個正等人類決策的 worker
   按下批准（見「已知限制」）。

狀態機：

```
queued --receive--> delivered --start--> running
delivered | running --reply--> completed   （終態）
delivered | running --fail---> failed      （終態）
queued | delivered | running --cancel--> cancelled（終態）
```

- 非法轉換（如未 receive 直接 reply、對終態再 reply/fail/cancel/start）一律：
  stderr 報錯、非零退出、狀態檔不變。
- receive 對 delivered / running 的任務冪等：重印內容、不改狀態、
  記一筆 `re-receive`。
- `start` 可選：worker 用它標記「已開工」，讓 sender 的 `status` 分得出
  「已送達但可能沒人理」與「正在做」。不 `start` 直接從 delivered `reply` 也合法。
- `fail` 與 `reply` 同構：訊息（失敗原因）寫入 `response.md`，`read` 可讀
  （completed 與 failed 都可 `read`；cancelled 不可）。
- `cancel` 由 sender 使用；已 cancelled 後 worker 的 reply / fail 會以非法
  轉換被拒，worker pane 會收到一行 `agent-bridge status <task-id>` 通知
  （執行後看到 `cancelled`）。

## 安裝

硬依賴：`bash`、`jq`、`tmux`（缺 jq 會報錯並以非零碼退出）。

```bash
git clone <repo-url> ~/projects/agent-bridge
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # 應解析到 symlink
```

Claude Code 的委派協定 skill：把**整個 repo** symlink 成 skill 目錄
（SKILL.md 以相對路徑引用 `share/` 的 briefs，必須跟它們同目錄才解析得到；
用 dotfiles 管理器管理這條 symlink 亦可，效果等價）：

```bash
ln -s ~/projects/agent-bridge ~/.claude/skills/agent-bridge
```

## 指令

```bash
agent-bridge register <agent> <tmux-target>   # 註冊 agent（target 會正規化成 %pane_id）
agent-bridge unregister <agent>               # 移除註冊（未註冊者報錯）
agent-bridge list                             # 每行 name<TAB>pane_id<TAB>ready 欄（人工註冊 -；spawned 為 starting/ready）
agent-bridge send <agent> --from <sender> (--message <text> | --message-file <path>)
agent-bridge receive <task-id>                # 標頭走 stderr、request 原文走 stdout
agent-bridge start <task-id>                  # （worker，可選）delivered → running
agent-bridge reply <task-id> (--message <text> | --message-file <path>)
agent-bridge fail <task-id> (--message <text> | --message-file <path>)  # 訊息＝失敗原因
agent-bridge cancel <task-id>                 # （sender）queued/delivered/running → cancelled
agent-bridge status <task-id>                 # stdout 只印裸狀態字一行
agent-bridge read <task-id>                   # completed/failed 可讀；標頭走 stderr、原文走 stdout
agent-bridge await <task-id> [--timeout <secs>]  # 阻塞至終態，印裸狀態字；逾時 exit 124（其他錯誤非 124）
agent-bridge spawn <name> --runtime <codex|claude> [--model <model>] [--window]
                                              # spawn worker pane；stdout 只印 pane-id
agent-bridge relay <name> --runtime <codex|claude> [--model <model>] --handoff <path> \
                          [--window] [--no-select] [--self-exit <my-name>]
                                              # 把主導權交給接手者 session（注入接手者守則＋交接檔）；stdout 只印 pane-id
agent-bridge despawn <name>                   # 回收 spawn 出身的 worker（人工註冊拒殺）
agent-bridge ready <name>                     # （worker）回報就緒；僅限 spawned agent
agent-bridge disposable <name>                # （worker，僅限 spawned）宣告本輪脈絡已無殘值，可即時回收
agent-bridge idle                             # 回收決策視圖：name<TAB>ready<TAB>disposable<TAB>idle_secs
agent-bridge evict <name> [--timeout <secs>] [--from <sender>]
                                              # 派收尾任務 → 等筆記落地 → despawn；stdout 只印收尾 task-id
agent-bridge gc [--older-than <days>] [--apply] [--include-notes]
                                              # 清舊終態 task（預設 14 天）；預設只試算，--apply 才刪
```

輸出契約（機器可解析）：

- `send` 成功時 stdout **只印 task-id 一行**，支援 `id=$(agent-bridge send ...)`；
  其餘訊息走 stderr。
- `--message-file -` 讀 stdin，是 agent 傳多行內容的主要路徑。
- agent 名（`register` 的 `<agent>` 與 `send` 的 `--from`）限 `[A-Za-z0-9_-]+`。
- `read` 於尚未有回覆的任務：stderr 報「尚未回覆」、非零退出
  （查詢進度請用 `status`）；於 cancelled 的任務報「已取消」、非零退出。
- `await` 是**唯讀**輪詢（不取鎖、不寫 events），只讀 sandbox 內也能用；
  到達終態（completed / failed / cancelled）即印裸狀態字並 exit 0，
  `--timeout 0`（預設）表示不逾時；秒數上限 9 位數（十進位解讀，
  前導零合法）。輪詢間隔預設 1 秒，可用 `AGENT_BRIDGE_POLL_INTERVAL` 覆蓋。
- 所有錯誤路徑：訊息走 stderr、非零 exit code。

## 兩個 pane 完整走一遍

假設 pane 左（`%1`）跑 claude 擔任 planner、pane 右（`%2`）跑另一個 claude
擔任 worker：

```bash
# 兩邊各自（或由任一邊）註冊；target 可用 %pane_id、session:window.pane 等任意 tmux 寫法
agent-bridge register planner %1
agent-bridge register worker  %2
agent-bridge list
# planner	%1	-
# worker	%2	-

# planner 委派任務（多行內容走 stdin）
id=$(agent-bridge send worker --from planner --message-file - <<'EOF'
請幫我跑 tests/run-tests.sh 並回報失敗案例。
限制：不要改任何檔案。
EOF
)

# worker 的 pane 會自動收到一行 `agent-bridge receive <task-id>` 並執行，
# request 原文出現在 worker 的輸入流。worker 完成後回覆：
agent-bridge reply "$id" --message-file - <<'EOF'
測試全過（294 PASS / 0 FAIL），未修改任何檔案。
EOF

# planner 的 pane 會自動收到 `agent-bridge read <task-id>`；也可手動查：
agent-bridge status "$id"   # completed
agent-bridge read "$id"     # response 原文

# 不想依賴 send-keys 通知（例如 worker 的 sandbox 發不出通知）時，
# planner 可改在背景等終態，回來的就是裸狀態字：
agent-bridge await "$id" --timeout 600   # completed / failed / cancelled
```

通知失敗（對方 pane 不在了、tmux 不可用）時：檔案與狀態照常完成、exit 0，
stderr 會印含 task-id 的警告，人工在對方 session 補跑 `receive` / `read` 即可。

## spawn／despawn：worker 生命週期

`spawn` 讓 orchestrator 直接開一個 worker pane 並註冊，不必人工 split＋register：

```bash
pane=$(agent-bridge spawn worker-1 --runtime codex)   # 預設進 per-owner worker window；--window 開專屬 window
agent-bridge list          # worker-1  %N  starting → ready
agent-bridge despawn worker-1                          # 任務收尾：kill pane＋除名＋審計
```

- **落點（per-owner worker window）**：在 tmux 內呼叫時，worker 進「這個
  orchestrator 專屬的 worker window」（緊鄰其 window 之後、名為
  `ab:<orchestrator window 名>`、tiled 均分、pane 邊框顯示 `名稱 (runtime/model)`）；
  同一 owner 再 spawn 會沿用同一窗。落點錨定在呼叫者（registry 記
  `owner`＝`session:@window_id`），不跟著你正在看的視窗跑——owner 的粒度是
  **orchestrator 所在的 tmux window**：不同 window 的 orchestrator 各自成窗，
  同一 window 裡的多個 orchestrator（罕見形態）會共用 owner 與 worker
  window。審計 `agents.log` 尾欄記行為者。沿用 registry 的 `worker_window`
  前會驗證該窗的 tmux 視窗選項 `@ab_owner`（建窗時寫入）等於呼叫者本次
  live 解析的 owner——印記存在 tmux 裡、「只能寫 registry」的攻擊者碰不到，
  同 session 冒用他 owner 的 worker window 也對不上；驗不過就另建新窗。`--window` 開完全獨立的 window（不與其他 worker 共用）。tmux 外
  呼叫（腳本、CI）無 owner 可錨定，退回「目前視窗往下 split」的舊行為。
  pane 標題可能被 runtime 自己的終端 title（OSC）蓋掉（實測 codex REPL 會），
  window 名不受影響。

- **runtime 表**：
  - `codex` → `codex --profile agent-worker`（profile 內容：approval never＋
    workspace-write，設定樣板與理由見 `docs/codex-worker-approval-proposal.md`；
    需自行加進 `~/.codex/config.toml`）
  - `claude` → `claude --permission-mode auto`。三個選擇都是刻意的：**auto**
    讓 worker 零人工介入執行探針命令（實測確認），而 auto 雖已是本機預設仍明寫，
    runtime 表不該依賴使用者哪天改掉 `defaultMode`；**不用 `bypassPermissions`**，
    理由只取[官方文件](https://code.claude.com/docs/en/permission-modes)明載的部分：
    該模式被定位為「Isolated containers and VMs only」，而 worker pane 就跑在本機、
    與主 session 共用檔案系統與憑證，不是隔離環境；`auto` 則是「Everything, with
    background safety checks」，保留那層背景檢查，且 protected paths 的寫入在除
    bypass 外的所有模式都不會被自動核准。
    ⚠️ 別把理由寫成「bypass 會繞過 hooks／讓 ask 消失」——**都不對**：停用 hooks 的
    是另一個旗標 `--bare`，而 deny 與 explicit ask 規則官方明載「apply in every mode,
    including `bypassPermissions`」。這段因果在本檔前後寫錯過兩次，都是獨立複核對照
    官方文件抓出來的；claude worker 會以 `--settings share/claude-worker-hooks.json`
    （路徑可用 `AGENT_BRIDGE_CLAUDE_HOOKS` 覆蓋）注入 worker 專屬 hooks，把
    Stop/UserPromptSubmit/Notification 接到 `agent-bridge hook …`（通知原生化
    Phase 2），但仍**不加 `--setting-sources`**：hooks 分層是合併不是覆蓋，
    使用者全域安全設定原樣繼承，「worker 與主 session 同一套安全規則」的
    承諾不變，只是外加一份隨 repo 走、版本會隨 repo 更新的 worker 專屬 hooks。
    要停用就把 `AGENT_BRIDGE_CLAUDE_HOOKS` 指向一份 `{"hooks":{}}` 之類的合法
    空 JSON 即可——state 檔不再產生，`notify_or_defer` 讀不到新鮮 state 天然
    退回既有 legacy 送鍵，不需要另開一個功能旗標。代價是 worker brief 與
    全域守則並存、可能被全域規則改寫命令
    形式（實測見過 `echo` 重導向被換成 Write 工具），但 `agent-bridge` 是外部
    CLI、沒有等價工具可替換，故這條路徑不受影響。另外**不可用 `-p/--print`**：
    那是 headless，跑完即退出，pane 不會留下來收探針
  - 新增 runtime 前必須實測該 CLI 的位置參數確實會被當第一則 user message 執行
    且執行完 session 常駐；只吃 stdin 或需要別的旗標的 CLI 要另外長出注入方式
- **`--model` 指定 worker 的模型**（兩個 runtime 都吃 `--model` 長旗標，實測
  2026-07-23）：不給＝繼承該 CLI 的使用者預設——主 session 把預設模型換到高階
  層級時，worker 會跟著變貴，**規劃在主 session、執行下放的分工要靠這個旗標
  落地**（策略見 `share/orchestrator-brief.md`）。值會被拼進 pane 啟動命令
  字串，故驗證與 brief 路徑同級：字元集 `[A-Za-z0-9._-]{1,64}` 擋 sh/tmux
  分隔符，**首字元強制英數**擋旗標走私（否則 `--model --bare` 等於往 worker
  啟動旗標塞任意開關）。不合法一律在建 pane 之前拒絕。模型名存進 registry 的
  `model` 欄，事後查得到「這個 worker 當時跑什麼」。⚠️ claude runtime 對輕量級
  模型會把 `--permission-mode auto` **靜默降回 manual**，worker 卡死在第一個
  權限框（2026-07-23 實測 Haiku 4.5）——模型下限判準見
  `share/orchestrator-brief.md` 的 `--model` 段。
- **proxy 環境穿透**：pane 的環境繼承自 tmux server，不是執行 spawn 的程序——
  只存在 orchestrator 環境的 proxy 變數（例如以 `env https_proxy=… claude`
  別名啟動的受限網路環境）到不了 worker，runtime 會直連而死（受限網路實測：
  MITM 憑證擋下 SSL）。故 spawn 把呼叫者環境裡**有設定的**標準 proxy
  變數（`http_proxy`／`https_proxy`／`all_proxy`／`no_proxy` 及大寫版）以
  `printf %q` 跳脫後拼進啟動指令，未設定的不注入。值來自 orchestrator 自己的
  環境、與執行 spawn 的人同一信任域，跳脫是防意外拆詞而非防注入；worker 環境
  因此也帶著這些變數，授權的第三層 spawn 會繼續往下傳。
- **指名穿透**（`AGENT_BRIDGE_PASS_ENV`，逗號分隔的變數名）：同一條穿透路徑的
  白名單版，讓呼叫端指定 proxy 以外還要帶哪些變數過去。典型用途是 headless 姿態
  旗標——例如 cron wrapper 設的 `CLAUDE_UNATTENDED=1`，若不跟著過去，接手者 pane
  會靜默退回有人值守的寬鬆姿態，而靜默降級比明確失敗難察覺得多。規則與 proxy
  相同：只帶**有設定的**變數（未設的不塞空值）、`printf %q` 跳脫；變數名逐個驗
  格式，不合法就 fail-closed。
  例：`AGENT_BRIDGE_PASS_ENV=CLAUDE_UNATTENDED agent-bridge relay …`
  **不要拿來傳秘密**：值會出現在 pane 的啟動指令裡（`pane_start_command`），任何
  能操作這台 tmux 的人都看得到。白名單本身也只能由可信的 orchestrator 設定——
  若讓不可信的請求決定要帶哪些變數，`PATH`／`BASH_ENV`／`LD_PRELOAD`／
  `NODE_OPTIONS` 這類變數足以改變 runtime 的行為。
- **啟動即注入 worker 守則**：spawn 出來的是一個零脈絡的全新 session——它不知道
  自己是 worker，也不知道 pane 裡收到的 `agent-bridge receive <id>` 是要執行的
  命令而不是有人在跟它說話。實測（codex 0.145）沒有守則時，它會把就緒探針當成
  對話反覆回覆文字、永遠不 ready。因此啟動指令會把 `share/worker-brief.md`
  全文當成 runtime 的 initial prompt 位置參數（`cmd [OPTIONS] [PROMPT]`，agent
  CLI 的共通形狀）帶進去，末尾再接上這個 worker 的名字與首要動作。**該檔是
  worker 契約的唯一正本**，人工註冊的 worker 讀同一份（`SKILL.md` 指向它），
  兩邊不會漂移；路徑可用 `AGENT_BRIDGE_WORKER_BRIEF` 覆蓋。
  brief 全文刻意不進 **tmux 啟動命令的字面值**，改讓 pane 內的 sh 展開
  `$(cat -- …)`，啟動指令因此維持短、`pane_start_command` 仍可讀。（展開後
  它當然還是會成為 runtime 的 argv——不進的是命令字面值，不是不進程序。）
- **就緒自報到**：spawn 註冊時 `ready: false`，隨後以間隔重送探針
  `agent-bridge ready <name>` 進新 pane——worker 執行它，把 registry 翻成
  `ready: true`。啟動期被 REPL 吃掉的按鍵由重送覆蓋。等待上限
  `AGENT_BRIDGE_READY_TIMEOUT`（預設 30 秒，`0`＝不等待），重送間隔
  `AGENT_BRIDGE_READY_PROBE_INTERVAL`（預設 2 秒）。**逾時不回滾、僅警告**，
  pane 留用供人工診斷。實測 codex 0.145 約 7 秒回報就緒。
- 對 `ready: false` 的 spawned agent `send` 合法：stderr 警告但不拒送，訊息在
  mailbox 不會丟，只是通知可能延後。

### relay：把主導權交給下一棒

```bash
agent-bridge relay <name> --runtime <codex|claude> [--model <model>] --handoff <path> \
  [--window] [--no-select] [--self-exit <my-name>]
```

`spawn` 開的是**等派工的 worker**；`relay` 開的是**接手者**——它一開始就拿到一份
交接檔，讀完自己往下做，不等 `receive`。除了注入 `share/successor-brief.md`
（而非 worker 守則）與切焦點之外，整條 pane 生命週期（cap／tag／回滾／夭折偵測／
registry／審計）與 `spawn` 完全共用，不複製第二份安全不變量。

- **`--self-exit <my-name>` 不是自殺，是請接手者回收前一棒。** 這不是繞路：
  既有 `despawn` 的順序是「kill pane → 確認已死 → 清 registry → 寫審計」，
  A 若殺自己的 pane，執行中的 process 會被 SIGHUP 帶走，永遠走不到後兩步——
  registry 殘留、審計線斷掉。交給活著的 B 執行，順序不變量原封不動。
- **接力鏈第一棒的人工閘門是免費的**：第一棒是人工開的 session，B 去 despawn 它
  會被既有的「非 spawn 出身，despawn 拒絕」擋下。接手者守則明說看到這個訊息是
  正常的、不要繞過，也不要改用 tmux 直接殺對方的 pane。
- **`--no-select` 是 orchestrator 驅動時的常態**：主導者若是 agent 而非人，
  切焦點沒有意義。互動接力時則預設切過去。
- 交接檔路徑來自命令列，暴露面比內部常數大一級，因此走與 brief 相同的三道
  防線（單引號 fail-closed、`-f && -r`、`cat --`），且全部在建 pane 之前。
- **接力鏈有深度上限**（`AGENT_BRIDGE_MAX_RELAY_DEPTH`，預設 10，`0` ＝不設限）。
  接手者守則鼓勵「context 吃緊就再交棒」，沒有上界的話那就是無界遞迴——無人值守
  時一路接下去，燒掉的額度沒有天花板。深度由 `AGENT_BRIDGE_RELAY_DEPTH` 在鏈上
  逐棒下傳（人工起的第一棒沒有這個變數＝深度 0，其後每 relay 一次 +1），達上限時
  在建 pane 之前擋下，要人介入確認才續。兩個變數都只接受 1–9 位十進位數字（前導
  零合法），**空字串一樣被拒**——`0` 才是「不設限」的寫法，空值若被當成預設就等於
  靜默重置鏈深度。設 `0` 也仍受 9 位數的格式上限約束。**已知限制**：pane 內可以
  自行改寫這個變數繞過（與 registry 同屬 worker 可寫面），這道 cap 擋的是失控
  迴圈，不是蓄意繞過。

安全設計（全部機器可驗）：

- **cap**：`.spawned == true` 的 agent 數達 `AGENT_BRIDGE_MAX_SPAWN`（預設 4）
  時 spawn 被拒；cap 檢查＋建 pane＋註冊包在 registry 專用 mkdir 鎖內，並行
  spawn 無法繞過上限。人工 register 的 agent 不計入。
- **出身標記**：只有 registry 帶 `spawned: true` 的 agent 能被 `despawn`；
  人工註冊的 pane 一律拒殺（要移除用 `unregister`，不碰 pane）。反向也擋：
  `register` 不覆寫 spawned agent、`unregister` 不除名 spawned agent——否則
  出身標記會被洗掉，pane 變成沒人能回收的孤兒。`register` / `unregister` /
  `spawn` / `despawn` / `ready` 共用同一把 registry 鎖，出身檢查一律在鎖內
  完成，杜絕「檢查通過後 registry 被換掉、卻殺到別人 pane」的競態。
- **殺之前先驗 pane 出身**：`pane_id` 不是身分證明——tmux 的 pane id 來自
  server 內的計數器，換一個 server 或 server 重啟後會重新發到同一個 `%N`。
  因此 spawn 時會在啟動指令埋一個一次性 tag（`ab-spawn-<agent 名>-<pid>-<48
  位隨機>`）並存進 registry 的 `spawn_tag`，`despawn` 只有在目標 pane 的啟動
  指令確實帶著該 tag 時才動手。對不上時**不動那個 pane**，只清掉過時註冊並在
  stderr 警告，審計記為 `despawn-stale`。tag 本身也驗格式且綁 agent 名字：
  否則 registry 裡填個 `spawn_tag: "bash"` 就成了萬用鑰匙（任何以 bash 起手的
  pane 都會被前綴命中），抄另一個 worker 的 tag 也能借刀殺人。
- **驗證與 kill 在同一次 tmux 連線內完成**：拆成兩次呼叫的話，中間 server 若
  死掉重啟、新 server 把同一個 `%N` 發給人工 pane，第二次呼叫就殺錯人。
  實作用 `tmux if-shell -F` 讓「再驗一次 tag」與 `kill-pane` 原子地一起發生，
  換了 server 就判 false 不動手；隨後再確認 pane 真的消失，沒消失就報錯並
  保留 registry。**spawn 的回滾走同一套**（回滾同樣是拿著一個 pane id 去殺，
  同樣可能落到已被替換的 server）。
- **registry 的值進 tmux 前先驗格式**：`pane_id` 必須是 `%<數字>`。它會被展開
  進 tmux 命令字串，而 tmux 命令裡 `;` 是分隔符——一個寫成 `%1 ; kill-server`
  的 pane_id 足以殺掉整個 tmux server。
- **判不出來就不動手（fail-closed）**：出身判斷把「JSON 損壞／讀不到」與
  「不是 spawned」分開處理，前者一律拒絕操作並要求人工確認——把兩者壓成同一個
  「非 spawned」，一份壞掉的 registry 就會被 `register` 覆寫、被 `unregister`
  除名（pane 從此沒人回收），也不計入 cap。cap 計數則相反地保守：無法解析的
  registry 照樣佔一個名額。
- **無法確認就不清註冊**：`despawn` 若連 tmux 查詢都失敗（server 不可達、
  sandbox 擋 socket），或 `kill-pane` 失敗，一律非零退出且**保留 registry**。
  把這兩種失敗當成「pane 已不存在」會製造沒人回收得掉的孤兒 pane。只有查詢
  成功且確認 pane 不存在時，才會單獨清除註冊。
- **原子回滾**：spawn 中途任一步失敗（含 runtime 啟動即死的夭折偵測）→
  kill 已建 pane、刪 registry 檔、非零退出。pane id 還沒回到手上就失敗的
  情況也涵蓋：啟動指令帶一次性 tag，回滾靠它把孤兒 pane 掃出來（比對錨在
  指令開頭，不會誤殺參數裡碰巧含同一串的別人 pane）。
  清理本身若失敗（pane 殺不掉、或 `agents/` 目錄權限被外部改動使 registry
  刪不掉），**會在 stderr 明確警告而非靜默**。殘留的 registry 是個 ghost
  worker：佔一個 spawn 名額、擋住同名 spawn、在 `list` 裡照樣列出，`send`
  也還會替它建 mailbox task。排除障礙後用 `despawn` 清掉。
- **brief 不是可讀的普通檔案就不開 pane**：守則檔缺失、不可讀、或不是普通檔案
  （目錄、FIFO、裝置）時 spawn 直接非零退出，不建 pane、不寫 registry、不留
  審計。沒有守則的 worker 收到探針只會當成對話回覆，開出來也是壞的；而檢查
  放在建 pane 之前，才不會留下一個佔著 cap 的半殘 worker。
  只驗 `-r` 是不夠的——它對目錄一樣成立，而 pane 內 `cat` 讀目錄失敗時命令
  替換仍回空字串，runtime 照樣被 exec 起來（此缺陷由獨立複核以 `/tmp` 反例
  抓出，已補回歸測例）。
  **殘留缺口（已知、刻意不修）**：這道檢查與 pane 內實際讀檔之間有 TOCTOU
  空隙，後果有兩種——(a) brief 被刪或換成別的內容：worker 以空的／被替換的
  守則啟動，而不是拒絕啟動；(b) 被換成 FIFO 或其他讀不完的來源：pane 的 sh
  會卡在讀取、runtime 根本起不來，而 0.3 秒的夭折偵測只看到 sh 還活著，於是
  判定 spawn 成功——留下一個佔著 cap、永遠停在 `starting` 的 pane（此後果由
  獨立複核以順序化反例補出，(b) 比 (a) 更糟）。
  要關掉它得把啟動指令改成先讀檔再 `exec` 的複合形式，那會動到 `spawn_tag`
  前綴這條已被反覆錘過的安全不變量，代價高於收益；且替換 brief 本來就在
  同 uid 互信域內（見下）。清理方式是照常 `despawn` 那個 `starting` 的 agent。
- **brief 路徑不得含單引號**：路徑會以單引號字面值送進 pane 的 sh，引號一旦被
  閉合就能往後接命令。含單引號一律拒絕（湊跳脫不如不收）。這條有專門的注入
  測例把關——只斷言「非零退出」是不夠的，畸形路徑多半會讓 sh 語法錯誤而被
  夭折偵測擋下，看起來像 fail-closed 卻什麼也沒鎖住。
- **審計**：spawn/despawn 各追加一行到 `agents.log`（append-only）。
- despawn 對已消失的 pane（如 tmux server 重啟過）仍會清 registry，不卡死。

## disposable／idle／evict：pane 的去留由上下文殘值決定

worker 做完一件事不代表它該死。它腦裡可能還留著沒寫進 response 的東西——查過
但沒寫的 file:line、走過的死路、當時的假設。那些只要還可能被追問，這個 pane
就有價值。**預設保留**，判定者是 worker，最終回收權在 orchestrator。

```bash
agent-bridge idle                    # 看池況，挑一個
agent-bridge evict w1                # 派收尾任務 → 等筆記落地 → despawn
agent-bridge read <收尾 task-id>     # 讀它留下的筆記
agent-bridge spawn w5 --runtime claude
```

- **`disposable` 是建議不是保護**：registry 位在 worker 可寫的目錄，worker 大可
  自己改；但 `despawn` 認的是 `spawn_tag` 不是這個欄位，所以賴不掉。宣告後又被
  派新任務時自動轉 `expired`（`disposable_at` 存在的唯一理由）。
- **`evict` 逾時仍然 despawn**：否則一個不回話的 worker 會把 cap 永久卡死。代價
  是筆記沒落地，所以審計要分得出來——`evicted`（已落地）／`evicted-unfinished`
  （收尾任務 failed/cancelled）／`evicted-timeout`（等到逾時）。全記成同一種會讓
  審計線說謊：「筆記沒落地」的原因不同，追查方向也不同。
- **三步不包在一把鎖裡**：`LOCK_DIR` 是單值，同時持有兩把鎖時 `release_lock`
  只放得掉一把。分段的兩個中斷點失效方向都是「多留一個佔 cap」，而不是「刪掉
  還沒落地的脈絡」——後者是分段時唯一不可接受的失效。
- **`spawn` 不會自動驅逐**：不加 `--evict-if-full`，殺與不殺永遠是一個獨立、
  可審計的決定。
- **`idle_secs` 取「最後任務」與「pane 誕生」兩者較晚者**：agent 名稱可以重用，
  而 tasks/ 是長期累積的，只認名字會讓剛 spawn 的 worker 繼承前一個同名 pane 的
  活動時間，在報表上看起來閒置好幾小時（真鏈驗收實地踩到：spawn 39 秒顯示 21686）。
- 策略層完整守則見 `share/orchestrator-brief.md`（主導者）與
  `share/worker-brief.md`（worker，spawn 時自動注入）。

### gc：清舊 task，但保留線不能破

`tasks/` 原本只增不減。這不只是磁碟問題——`idle` 的回收決策直接掃這個目錄
（`last_task_at`），資料越髒決策越不可信，而且那是 O(task 數) 的掃描。
「同名前一個 pane 的任務讓新 worker 看起來閒置六小時」那個 bug 的根因就在這裡。

```bash
agent-bridge gc                    # 試算：列出可刪的，不動任何東西
agent-bridge gc --apply            # 真的刪
agent-bridge gc --older-than 30 --apply
```

三道保留線，失效方向一律是「留著」而不是「刪掉」：

1. **未完成的不刪**（queued／delivered／running）——那是還在跑的工作。這條有
   兩層：掃描時擋一次，取鎖後真刪之前再驗一次狀態（等鎖那段時間裡它可能被
   `receive`／`reply` 動過）。
2. **evict 的收尾筆記不刪**（metadata 帶 `pinned: true`），除非明確加
   `--include-notes`。那些筆記是這一層刻意留下來的脈絡；會被 GC 靜靜清掉的話，
   「上下文不會憑空消失」就只是延後兌現。
3. **判不出年紀的不刪**：`created_at` 缺失或無法解析就留著。年齡看 metadata 的
   `created_at` 而非目錄 mtime——mtime 會被備份、rsync、檔案系統操作改掉。

外加三條：預設是**試算**，`--apply` 才會刪；只碰 `send` 生成的目錄名形狀
（UTC 時間戳 ＋ 4 hex，不是寬鬆的公開 task-id 格式——這是唯一會 `rm` 的路徑）；
以及**「宣告已失效」的證據不刪**。

最後那條不直覺但要緊：`idle` 判斷一個 `disposable` 宣告有沒有被後續任務推翻，
靠的就是掃 `tasks/` 找晚於 `disposable_at` 的任務。那個任務一旦被清掉，證據跟著
消失，宣告會從 `expired` **復活成 `yes`**，orchestrator 據此直接回收一個其實已有
新脈絡的 worker。所以只要 registry 裡還掛著某個宣告，晚於它的任務就一律保留。

### 回收的審計記號

`agents.log` 分得出這次回收有沒有丟掉東西：

| 記號 | 意思 |
|---|---|
| `despawned` | 乾淨回收：該 worker 宣告過 `disposable` 且宣告仍有效 |
| `despawned-unsaved` | 回收了一個仍被視為有殘值的 worker（沒宣告過，或宣告已被後續任務推翻）——繞過了收尾流程 |
| `despawn-stale` | 註冊清掉了，但那個 pane 已不屬於這個 agent，**未被回收** |
| `evicted` | 走完 evict，收尾筆記已落地 |
| `evicted-unfinished` | 收尾任務以 failed／cancelled 收場 |
| `evicted-timeout` | 等到逾時，pane 仍回收，筆記沒落地 |

`despawn` 是公開指令，機制上**不阻止**你直接回收一個有殘值的 worker——擋下來只會
逼出一個 `--force`，而習慣性加旗標等於沒有防線。改成讓它在審計線上看得出來。
走 `evict` 的回收不會記 `unsaved`（收尾流程已經跑過）。

## 測試

```bash
tests/run-tests.sh
```

純 bash、零額外依賴。整合測試使用獨立 socket
`tmux -L agent-bridge-test -f /dev/null`，不碰使用者真實 tmux server。

## 開發慣例

- **chezmoi 側變更走 topic branch + worktree**（2026-07-22 起）：本專案在
  chezmoi repo 的配套變更（codex config 模板、skill symlink、hook 煙測）
  一律在 chezmoi repo 開 topic branch 並用獨立 worktree 作業，審過再併回
  master——不直接在 master 與使用者日常 dotfile 變更混流。

## 已知限制（補充：鎖的殘留）

鎖是 `mkdir` 目錄鎖，靠 `trap release_lock EXIT` 清除。程序被 `SIGKILL` 或主機
斷電時 trap 不會執行，`locks/<id>.lock` 會殘留，之後同類命令重試 25 次後失敗。
目前沒有 owner PID 或 stale-lock 自動回收——加那套機制本身會引入「誤刪別人正在
持有的鎖」這個更糟的失效方向，所以維持手動處理：確認沒有 agent-bridge 正在跑，
然後刪掉 `locks/` 底下殘留的目錄。

## 已知限制

- **agent 忙碌時通知會延後處理**：已接線 hook 的 claude worker 若 state 檔
  顯示新鮮的 `busy`，`notify_or_defer` 現在完全不送鍵，訊息留在 mailbox，
  改由對方自己的 Stop hook 在 turn 結束時查到並自行 `receive`（通知原生化
  Phase 1/2）。殘餘限制仍要誠實列出：(1) **codex worker 尚未接線**這條 hook
  通道（Phase 3 待補），仍是舊行為——send-keys 打進去的指令要等對方 REPL
  輪到輸入時才會執行；(2) claude worker 的 hook 若中途掛掉（stdin 非
  JSON、缺 `jq`、state 目錄寫不進去等），state 會停在舊的 `busy` 不動，
  要等 `AGENT_BRIDGE_STATE_TTL`（預設 1800 秒）過期後才會退回 send-keys
  通知，這段窗口內只能靠對方自己輪到輸入或人工介入；(3) state 通道語意
  是「建議非權威」（同 `disposable`），不是保護機制——上面兩種情形都不會
  遺失訊息，只是即時性沒有保證。
- **通知原生化本身引入的脆弱面（Phase 1/2，2026-07-28，刻意不美化）**：
  - **`notification_type` 欄位有已知可靠性缺口**：官方 Notification hook
    payload 有時完全缺這個欄位（[issue #12048](https://github.com/anthropics/claude-code/issues/12048)，
    關聯 [#11964](https://github.com/anthropics/claude-code/issues/11964)）。
    本實作 fail-safe 為「缺欄位時不等於 `idle_prompt`、落入不動 state 的
    分支」，而不是誤判成 idle——代價是 state 可能停在舊值，但 TTL 到期後
    仍會退回 legacy 送鍵，不會永久卡住。
  - **state 檔位在 worker 可寫的資料目錄**：`state/<name>.json` 與 registry
    同屬同一個互信域，同 uid 下跑的任何 worker 都寫得到別人的 state 檔。
    這條通道語意本來就是「建議非權威」，是建議不是保護。
  - **hook 靠 `AGENT_BRIDGE_SPAWN_TAG` 析出「我是誰」**：`hook_agent_name`
    讀不到這個環境變數就直接 no-op。人工 `register` 的 worker 環境裡沒有
    這個變數，天然不參與狀態通道——不是被排除，是沒有身分可寫，一律走
    既有 send-keys 路徑。
  - **Stop hook 的續跑依賴 Claude Code 的 `decision: block` 語意**：本實作
    靠這個協定讓 worker 在 turn 結束時自動 `receive` 下一個排隊任務。這個
    語意若日後改變，失效方向是 worker 不再自動取件、退回既有 legacy 行為
    （訊息仍在 mailbox，靠 send-keys 通知或人工介入取件），不會有資料遺失。
- **tmux server 重啟後 pane_id 全部失效**，需要重新 `register`；spawn 出身的
  agent 用 `despawn` 清掉殘留 registry 再重新 spawn——重啟後新 pane 可能拿到
  同一個 `%N`，`despawn` 靠 `spawn_tag` 認得出來，不會誤殺（見上）。
- **tmux 行為的驗證版本是 3.7b**：回滾掃孤兒 pane 依賴 `pane_start_command`
  的存法（含空白的指令會被加上雙引號）。測試以不變量斷言鎖住這個行為，換版本
  若改變存法會直接測出來，而不是默默漏殺。未逐版驗證，故不宣告最低版本號。
- **ready 是自報到，不是健康檢查**：`ready: true` 只代表 worker REPL 曾執行過
  探針，不保證它現在還活著；探針重送只覆蓋「啟動期按鍵被吃」這一種遺失。
  就緒逾時後 pane 仍留用，`list` 會一直顯示 `starting`。
- **cancel 是狀態宣告，不是搶佔**：cancel 只翻狀態並通知，正在執行的 worker
  不會被中斷；它的 reply / fail 會在事後以非法轉換被拒。
- **訊息內容對 receiver 是不可信輸入**：request 來自另一個 agent，構成跨
  agent 的 prompt injection 面。receiver 應把內容當資料而非指令對待
  （見 `SKILL.md`）。
- **通知的 Enter 不會替 worker 按掉權限對話框**：worker 若正停在 Claude Code 的
  權限確認對話框（等人決定要不要放行某個命令），send-keys 送的 Enter 會被對話框
  當成「確認預設選項（Yes）」——等於一則無關的外部通知替 worker 批准了它正等人
  決策的命令（2026-07-23 實測誤觸）。防護是送鍵前後兩次 `capture-pane` 掃對話框
  特徵（送文字前、送 Enter 前各一次，任一次 capture 失敗都 fail-closed 降級），
  掃到就走 notify-failed（訊息仍在 mailbox，交既有降級路徑）。特徵取
  `Do you want to ` 前綴＋底部 `Esc to cancel`，涵蓋 Bash 的
  `Do you want to proceed?`、檔案 Edit/Write 的 `Do you want to make this edit
  to …`、WebFetch 的 `Do you want to allow Claude to fetch …` 等 worker 執行命令
  時實際會撞到的權限框。**侷限（刻意揭露）**：(1) 這是對可見文字的字串匹配，
  Claude Code 改文案或本地化會讓特徵失效——方向是 fail-open（退回原本會誤觸的
  行為），因為漏判（替 worker 誤按批准）比偽陽性（通知延後、訊息可復原）更糟，
  偵測刻意偏攔。(2) 特徵涵蓋兩組（2026-07-23 真 UI 實測）：上述權限框，以及
  plan mode 的退出確認框——後者標題不含 `Do you want to `、footer 不含
  `Esc to cancel`，且 Enter 預設是「Yes, and use auto mode」，誤觸不只批准
  plan 還把 worker 切進 auto mode，比權限框更糟，故以標題兩片段 AND 另列特徵；
  `Do you want to use this API key?` 框實測落在權限框特徵內（footer 同為
  `Esc to cancel`，Enter 預設是安全方向的 `No (recommended)`）。workspace
  trust 框在已 onboarding 的本機實測不出現、拿不到第一手特徵，**不涵蓋**；
  其他未知句式的新框同樣漏判，方向一律 fail-open（回到會誤觸的行為）。(3) 第二次掃描與 send-keys 之間仍有無法在 tmux
  層消除的微小 race。codex worker 走 `approval_policy = never`、不彈這種對話框，
  天然不受影響。
- **通知目標的 registry 讀取與 send-keys 之間仍有極小 race**：send 在發通知前
  會重讀 registry 取最新 pane（把「建目錄＋三次寫檔」期間同名 agent 被
  unregister＋register 換 pane 的窗口縮到次毫秒級），但「讀 registry」與
  「send-keys」在 tmux 層無法原子化，理論上通知仍可能打進已易主的舊 pane。
  只影響通知這一發按鍵；訊息本體在 mailbox，不受影響。
- **同一個資料目錄＝同一個互信域，出身檢查不是對抗惡意 agent 的邊界**：
  所有 agent 以同一個 uid 跑、共用 `AGENT_BRIDGE_DATA`，registry 檔因此對每個
  worker 都可寫。上面的 `spawn_tag` 驗證擋得住**意外**殺錯（pane id 重用、
  server 重啟、過時註冊）與**跨 worker 借刀**（抄別人的 tag、填萬用 tag），
  但擋不住一個已被完全控制、且**能連上 tmux socket** 的 worker——它根本不必
  繞過 `despawn`，直接 `tmux kill-pane` 就是了。真正的邊界是 tmux socket 的
  存取權本身（codex `workspace-write` 預設就擋掉了 socket 連線，見下）。
  把 despawn 的檢查當成最後一道防線是誤解它的角色。
  **worker brief 也在這個互信域內**：`share/worker-brief.md` 位於 worker 可寫的
  workspace，被控制的 worker 可以改寫它，藉此左右**下一個** spawn 出來的 worker
  的啟動 prompt。這不構成新的信任邊界破口（同 uid 本來就互信），但它把「守則
  檔的完整性」變成一個實際的相依項——把 brief 放在唯讀位置並以
  `AGENT_BRIDGE_WORKER_BRIEF` 指過去，是想收緊時的作法（連同**父目錄**也要
  不可替換，否則整個檔可被換掉）。最壞後果不只是「守則被刪、worker 沒守則」：
  換成 symlink 可把另一份同 uid 可讀的檔案帶進 prompt，內容被改寫則等於直接
  改寫下一個 worker 的行為契約。
- 通知協定假設 `agent-bridge` 在對方 pane 的 PATH 上（安裝 symlink 後即成立）。
- **agent runtime 的 sandbox 必須允許寫資料目錄**：receive/reply 需要寫
  `AGENT_BRIDGE_DATA`（預設 `~/.local/share/agent-bridge/`）。例如 codex CLI 的
  `workspace-write` sandbox 預設擋 workspace 外寫入，需在 `config.toml` 的
  `[sandbox_workspace_write] writable_roots` 加入該路徑，否則 receive 會因
  無法建立鎖而失敗（status 唯讀不受影響）。
- **sandbox 擋 socket 連線時，該 agent 發不出通知**：發通知要連 tmux server
  的 unix socket（`/tmp/tmux-<uid>/default`）。codex `workspace-write` 預設
  `network_access = false`，以 seccomp 擋 `connect()`（含 unix socket，
  `writable_roots` 救不了），實測錯誤為
  `error connecting to /tmp/tmux-1000/default (Operation not permitted)`。
  該 agent 收任務、reply 都正常，只有通知走優雅降級（notify-failed 警告＋
  手動補救指令）。**建議解法是 sender 端用 `agent-bridge await`**（唯讀輪詢，
  不需要對方發得出通知，sandbox 維持關閉）；設 `network_access = true` 雖可
  讓通知全自動，但代價是 sandbox 內指令可對外連網，非必要不建議。

# agent-bridge

跨 tmux pane 的 agent 任務委派橋（core MVP）。讓分跑在不同 tmux pane 的多個
claude / codex CLI session 互相委派任務與回覆，把工作拆細、讓每個 agent 的
context 保持短而乾淨。

## 架構

三個組成，全部本機、無常駐程序：

1. **薄 bash CLI**（`bin/agent-bridge`）：十五個子指令，唯一的進入點。
2. **檔案系統 mailbox**（預設 `~/.local/share/agent-bridge/`，可用環境變數
   `AGENT_BRIDGE_DATA` 覆蓋）：
   - `agents/<name>.json`：agent 註冊表（`{name, pane_id, registered_at}`；
     spawn 出身的另有 `spawned: true`、`runtime`、`spawned_at`、`ready`、
     `spawn_tag`）
   - `agents.log`：spawn/despawn 審計流，每行
     `<ISO8601Z> spawned|despawned|despawn-stale <name> <pane> <runtime>`
   - `tasks/<task-id>/`：每個任務一個目錄
     - `metadata.json`：`version`（本輪為 1）、`task_id`、`from`、`to`、
       `created_at`、`updated_at`、`working_directory`、`status`（時間一律
       ISO 8601 UTC）
     - `request.md`：委派內容原文（byte-for-byte 保真）
     - `response.md`：回覆原文（reply 後才存在）
     - `status`：裸狀態字（`queued` / `delivered` / `running` / `completed` /
       `failed` / `cancelled`）
     - `events.log`：append-only 事件流，每行 `<ISO8601Z> <event> [detail]`；
       事件詞彙表固定為 `created`、`notified`、`notify-failed`、`delivered`、
       `re-receive`、`started`、`replied`、`failed`、`cancelled`、`read`
   - `locks/`：狀態轉換用的 mkdir 鎖
3. **tmux send-keys 短通知**：send / reply 寫完檔案後，只向對方 pane 送一行
   `agent-bridge receive <task-id>`（或 `read <task-id>`）加 Enter。訊息內容
   永遠走檔案，絕不進 send-keys。文字與 Enter 拆成兩次 send-keys、中間隔
   0.3 秒（可用 `AGENT_BRIDGE_NOTIFY_DELAY` 調整）：agent REPL 這類 TUI 會把
   同批抵達的文字+Enter 當成貼上而吞掉 Enter，導致指令留在輸入框不送出。

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

Claude Code 的委派協定 skill：`~/.claude/skills/agent-bridge/SKILL.md` 是指向
repo 內 `SKILL.md` 的 symlink（本機由 chezmoi source
`dot_claude/skills/agent-bridge/symlink_SKILL.md.tmpl` 管理；非 chezmoi 環境
直接 `ln -s` 等效）。

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
agent-bridge await <task-id> [--timeout <secs>]  # 阻塞至終態，印裸狀態字；逾時非零退出
agent-bridge spawn <name> --runtime codex [--window]  # spawn worker pane；stdout 只印 pane-id
agent-bridge despawn <name>                   # 回收 spawn 出身的 worker（人工註冊拒殺）
agent-bridge ready <name>                     # （worker）回報就緒；僅限 spawned agent
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
pane=$(agent-bridge spawn worker-1 --runtime codex)   # 預設 split-window；--window 開新 window
agent-bridge list          # worker-1  %N  starting → ready
agent-bridge despawn worker-1                          # 任務收尾：kill pane＋除名＋審計
```

- **runtime 表（v1）**：`codex` → `codex --profile agent-worker`（該 profile 由
  chezmoi 管理，approval never＋workspace-write，見
  `docs/codex-worker-approval-proposal.md`）。claude runtime 待獨立 spec。
- **就緒自報到**：spawn 註冊時 `ready: false`，隨後以間隔重送探針
  `agent-bridge ready <name>` 進新 pane——REPL 真正就緒時才會執行它，把
  registry 翻成 `ready: true`。啟動期被 REPL 吃掉的按鍵由重送覆蓋。等待上限
  `AGENT_BRIDGE_READY_TIMEOUT`（預設 30 秒，`0`＝不等待），重送間隔
  `AGENT_BRIDGE_READY_PROBE_INTERVAL`（預設 2 秒）。**逾時不回滾、僅警告**，
  pane 留用供人工診斷。
- 對 `ready: false` 的 spawned agent `send` 合法：stderr 警告但不拒送，訊息在
  mailbox 不會丟，只是通知可能延後。

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
- **審計**：spawn/despawn 各追加一行到 `agents.log`（append-only）。
- despawn 對已消失的 pane（如 tmux server 重啟過）仍會清 registry，不卡死。

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

## 已知限制

- **agent 忙碌時通知會延後處理**：send-keys 打進去的指令要等對方 REPL 輪到
  輸入時才會執行；狀態在 mailbox 裡不會遺失，但即時性沒有保證。
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
- **同一個資料目錄＝同一個互信域，出身檢查不是對抗惡意 agent 的邊界**：
  所有 agent 以同一個 uid 跑、共用 `AGENT_BRIDGE_DATA`，registry 檔因此對每個
  worker 都可寫。上面的 `spawn_tag` 驗證擋得住**意外**殺錯（pane id 重用、
  server 重啟、過時註冊）與**跨 worker 借刀**（抄別人的 tag、填萬用 tag），
  但擋不住一個已被完全控制、且**能連上 tmux socket** 的 worker——它根本不必
  繞過 `despawn`，直接 `tmux kill-pane` 就是了。真正的邊界是 tmux socket 的
  存取權本身（codex `workspace-write` 預設就擋掉了 socket 連線，見下）。
  把 despawn 的檢查當成最後一道防線是誤解它的角色。
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

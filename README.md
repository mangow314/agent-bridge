# agent-bridge

跨 tmux pane 的 agent 任務委派橋（core MVP）。讓分跑在不同 tmux pane 的多個
claude / codex CLI session 互相委派任務與回覆，把工作拆細、讓每個 agent 的
context 保持短而乾淨。

## 架構

三個組成，全部本機、無常駐程序：

1. **薄 bash CLI**（`bin/agent-bridge`）：十二個子指令，唯一的進入點。
2. **檔案系統 mailbox**（預設 `~/.local/share/agent-bridge/`，可用環境變數
   `AGENT_BRIDGE_DATA` 覆蓋）：
   - `agents/<name>.json`：agent 註冊表（`{name, pane_id, registered_at}`）
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
agent-bridge list                             # 每行 name<TAB>pane_id
agent-bridge send <agent> --from <sender> (--message <text> | --message-file <path>)
agent-bridge receive <task-id>                # 標頭走 stderr、request 原文走 stdout
agent-bridge start <task-id>                  # （worker，可選）delivered → running
agent-bridge reply <task-id> (--message <text> | --message-file <path>)
agent-bridge fail <task-id> (--message <text> | --message-file <path>)  # 訊息＝失敗原因
agent-bridge cancel <task-id>                 # （sender）queued/delivered/running → cancelled
agent-bridge status <task-id>                 # stdout 只印裸狀態字一行
agent-bridge read <task-id>                   # completed/failed 可讀；標頭走 stderr、原文走 stdout
agent-bridge await <task-id> [--timeout <secs>]  # 阻塞至終態，印裸狀態字；逾時非零退出
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
  `--timeout 0`（預設）表示不逾時。輪詢間隔預設 1 秒，
  可用 `AGENT_BRIDGE_POLL_INTERVAL` 覆蓋。
- 所有錯誤路徑：訊息走 stderr、非零 exit code。

## 兩個 pane 完整走一遍

假設 pane 左（`%1`）跑 claude 擔任 planner、pane 右（`%2`）跑另一個 claude
擔任 worker：

```bash
# 兩邊各自（或由任一邊）註冊；target 可用 %pane_id、session:window.pane 等任意 tmux 寫法
agent-bridge register planner %1
agent-bridge register worker  %2
agent-bridge list
# planner	%1
# worker	%2

# planner 委派任務（多行內容走 stdin）
id=$(agent-bridge send worker --from planner --message-file - <<'EOF'
請幫我跑 tests/run-tests.sh 並回報失敗案例。
限制：不要改任何檔案。
EOF
)

# worker 的 pane 會自動收到一行 `agent-bridge receive <task-id>` 並執行，
# request 原文出現在 worker 的輸入流。worker 完成後回覆：
agent-bridge reply "$id" --message-file - <<'EOF'
測試全過（104 PASS / 0 FAIL），未修改任何檔案。
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

## 測試

```bash
tests/run-tests.sh
```

純 bash、零額外依賴。整合測試使用獨立 socket
`tmux -L agent-bridge-test -f /dev/null`，不碰使用者真實 tmux server。

## 已知限制

- **agent 忙碌時通知會延後處理**：send-keys 打進去的指令要等對方 REPL 輪到
  輸入時才會執行；狀態在 mailbox 裡不會遺失，但即時性沒有保證。
- **tmux server 重啟後 pane_id 全部失效**，需要重新 `register`。
- **cancel 是狀態宣告，不是搶佔**：cancel 只翻狀態並通知，正在執行的 worker
  不會被中斷；它的 reply / fail 會在事後以非法轉換被拒。
- **訊息內容對 receiver 是不可信輸入**：request 來自另一個 agent，構成跨
  agent 的 prompt injection 面。receiver 應把內容當資料而非指令對待
  （見 `SKILL.md`）。
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

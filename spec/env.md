# 環境變數契約

全部 16 個 `AGENT_BRIDGE_*` 變數。通則：

### ENV-GEN-1 [untested]
未設定（unset）時每個變數 MUST 取本檔載明的預設值；本檔未列的
`AGENT_BRIDGE_*` 名稱 MUST NOT 影響行為（新增變數須先入本檔）。

---

## 路徑與資源

### ENV-DATA-1 [tested: 1]
`AGENT_BRIDGE_DATA`：資料目錄根。預設 `~/.local/share/agent-bridge`。
所有磁碟狀態（見 state.md）MUST 落在此目錄下；指向新目錄等同全新空 bridge。
Source: 全域初始化（DATA_DIR）

### ENV-BRIEF-1 [tested: 22]
`AGENT_BRIDGE_WORKER_BRIEF`：spawn 注入的 worker 守則檔。預設
`<repo>/share/worker-brief.md`。spawn MUST 在建立 pane 之前預檢此檔為可讀的
普通檔案，否則以錯誤終止（不得產生無守則的 worker）。
Note: 預檢之後、pane 內實際讀取之前存在 TOCTOU 空隙，屬已知限制（README）。
Source: cmd_spawn（WORKER_BRIEF 預檢）

### ENV-BRIEF-2 [tested: 23]
`AGENT_BRIDGE_SUCCESSOR_BRIEF`：relay 注入的接手者守則檔。預設
`<repo>/share/successor-brief.md`。同 ENV-BRIEF-1 的失敗語意。
Source: cmd_relay（SUCCESSOR_BRIEF）

### ENV-HOOKS-1 [tested: 16]
`AGENT_BRIDGE_CLAUDE_HOOKS`：spawn claude runtime 時以 `--settings` 注入的
hooks 設定檔。預設 `<repo>/share/claude-worker-hooks.json`。
Source: cmd_spawn（CLAUDE_HOOKS_SETTINGS）

## 時間與節奏

### ENV-POLL-1 [tested: 13, 31]
`AGENT_BRIDGE_POLL_INTERVAL`：`await` 輪詢間隔秒數。預設 `1`。
Source: cmd_await

### ENV-NOTIFY-1 [tested: 8a]
`AGENT_BRIDGE_NOTIFY_DELAY`：legacy 送鍵通知中 command 與 Enter 之間的
延遲秒數。預設 `0.3`。
Source: notify_pane

### ENV-TMUX-1 [tested: 39]
`AGENT_BRIDGE_TMUX_TIMEOUT`：單次 tmux 子行程的逾時秒數，涵蓋**所有** tmux
呼叫（通知路徑、spawn 生命週期、TUI read model——tui-design §4 bounded-read
硬條款：任何一條無界查詢都足以凍結 UI）。預設 `5`，
`0` 等同不設限。逾時 MUST 殺掉子行程並視同該次呼叫失敗（走 notify-failed
降級）。壞值 MUST 退預設而非終止：這是防止整個指令被鎖死的安全網（見
hooks.md HOOK-NOTIFY-3），拼錯一個變數名不該把安全網拆掉。
合法但極大的值 MUST 夾到內部上限而非 panic——溢位的期限計算會在任務**已建立
之後**的通知階段炸掉，比不逾時更難救。
Source: config::tmux_timeout / tmux::run_bounded

### ENV-TTL-1 [tested: 33, 34]
`AGENT_BRIDGE_STATE_TTL`：state 檔（見 state.md STATE-CHAN-*）新鮮度秒數。
預設 `1800`。state 的 `ts` 距今超過 TTL 秒（或解析失敗）時，通知端 MUST 把
該 worker 狀態視為「未知」並退回 legacy 送鍵路徑。`0` 等同通知端永遠視為
過期（原生通知通道關閉）。
Source: notify_or_defer

### ENV-TTL-2 [tested: 34]
通知端（send/reply/cancel 等 CLI 路徑）：值非 0–9 位純數字時 MUST 以錯誤
終止（die），不得靜默採用預設。
Source: notify_or_defer

### ENV-TTL-3 [tested: 34.5+]
hook 端（`agent-bridge hook`）：值為壞值**或 0** 時 MUST 退回預設 1800 續作，
MUST NOT 因此失敗——TTL=0 只關閉通知端原生通道，owner gate（hooks.md）仍須
運作以防冒名。不可放寬。
Source: hook_owner_gate

## spawn / worker 池

### ENV-SPAWN-1 [tested: 16]
`AGENT_BRIDGE_MAX_SPAWN`：同時存活的 spawn worker 上限。預設 `4`。
達上限時 spawn MUST 拒絕並以錯誤終止。
Source: cmd_spawn

### ENV-READY-1 [tested: 16, 20b]
`AGENT_BRIDGE_READY_TIMEOUT`：spawn/relay 等待 worker 回報 ready 的秒數。
預設 `30`；`0` ＝不等待、立即返回。
Source: cmd_spawn / cmd_relay

### ENV-READY-2 [tested: 16, 20b]
`AGENT_BRIDGE_READY_PROBE_INTERVAL`：ready 等待期間探針重送間隔秒數。預設 `2`。
Source: cmd_spawn / cmd_relay

### ENV-TAG-1 [tested: 16, 18b]
`AGENT_BRIDGE_SPAWN_TAG`：worker 身分標籤，由 spawn 在 worker 行程環境設定，
文法 MUST 為 `ab-spawn-<name>-<pid>-<12hex>`（`<name>` 符合 `[A-Za-z0-9_-]+`）。
hook 端由它析出「我是誰」；despawn/evict 以它比對出身與世代。使用者手動
設定此變數不構成身分授權（詳 hooks.md 的出身防護條款）。
Source: cmd_spawn / hook_agent_name / cmd_despawn

## TUI

### ENV-UI-1 [tested: 40]
`AGENT_BRIDGE_UI_POPUP`：`ui` 的啟動器協定旗標，由 tmux binding
（`display-popup -E 'agent-bridge ui'`）設定，值 `1` 生效、其餘值等同未設定。
設定時 `Enter` focus 成功後 MUST 直接正常退出（行程結束＝popup 關閉，人落在
目標 pane）；未設定時 focus 後繼續執行。程式 MUST NOT 自行偵測是否身處
popup（tui-design.md §2：模式感知屬啟動器，不進核心）。預設未設定。
Source: ab_tui::run（event_loop）

## relay 鏈

### ENV-PASS-1 [tested: 16]
`AGENT_BRIDGE_PASS_ENV`：逗號分隔的變數名清單，spawn/relay 時穿透給 worker。
任一名稱不符合合法變數名文法時 MUST 以錯誤終止。預設空（不穿透）。
Source: cmd_spawn（pass_list）

### ENV-DEPTH-1 [tested: 23]
`AGENT_BRIDGE_RELAY_DEPTH`：本 session 在接力鏈上的棒次，由鏈上逐棒下傳。
未設定 MUST 視為 `0`；**空字串或非數字 MUST 以錯誤終止**（空值靜默重置鏈
深度會癱瘓 cap，不可放寬）。
Source: cmd_relay

### ENV-DEPTH-2 [tested: 23]
`AGENT_BRIDGE_MAX_RELAY_DEPTH`：接力鏈棒次上限。未設定 MUST 視為 `10`；
空字串或非數字 MUST 以錯誤終止；`0` ＝解除上限。達上限時 relay MUST 拒絕
並提示需人工介入。
Source: cmd_relay

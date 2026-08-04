# spawn 實作計畫（第三輪）

狀態（立案當時）：**計畫定稿待審**（2026-07-22）。依據已過審的
`agent-spawn-proposal.md`；設計題拍板結果沿用提案傾向：就緒採自報到、
pane 預設留用、v1 只支援 codex、安全面全採（cap／出身標記／審計）、
失敗原子回滾。

**後續結果：已實作**（其後 runtime 擴充到 claude／agy）。本文保留計畫當時的
形狀，落地行為以 `spec/` 與 `tests/run-tests.sh` 為準。

前置件：codex agent-worker profile —— **已落地並驗收**（該 profile 由本機
dotfiles 管理，不在本 repo；驗收 task 20260722T035038Z-66f2，0 次人工核准）。

## 範圍與非目標

- 新指令：`spawn <name> --runtime codex [--window]`、`despawn <name>`。
- registry 擴充：spawn 出身標記與審計紀錄。
- 非目標：排程／worker 池策略（orchestrator 層）、claude runtime 支援
  （待獨立 spec，見「遺留」）、despawn 之外的 pane 健康監控。

## 現況錨點（已核實）

- registry：`$AGENTS_DIR/<name>.json`，欄位 `{name, pane_id,
  registered_at}`，jq 產生＋`atomic_write` 寫入（已退役 bash 正本的
  `cmd_register`／`atomic_write`，見 git history）。
- 測試 harness：獨立 socket `tmux -L agent-bridge-test`、PATH shim 機制、
  `assert`/`assert_fails` helper（`tests/run-tests.sh`）。
- 通知機制：send-keys 短指令進對方 REPL 輸入流（README「已知限制」：
  忙碌時延後、啟動期按鍵可能被吃——phase 2 要正面處理的就是後者）。

## Phase 1：spawn 核心＋原子回滾＋審計

- `cmd_spawn`：
  1. 名稱過 `NAME_RE` 且未被註冊（已註冊 → die，不覆蓋人工 agent）。
  2. cap 檢查：`.spawned == true` 的 registry 計數 ≥
     `AGENT_BRIDGE_MAX_SPAWN`（預設 4）→ die。cap 檢查＋註冊以
     registry 專用 mkdir 鎖包住，杜絕並行 spawn 繞過 cap。
  3. `tmux split-window -P -F '#{pane_id}'`（`--window` 改
     `new-window`）啟動 runtime；v1 runtime 表只有
     `codex → codex --profile agent-worker`。
  4. 註冊 JSON 加欄位：`spawned: true`、`runtime`、`spawned_at`。
  5. 任一步失敗 → 回滾：kill 已建 pane、刪 registry 檔，exit 非零。
- 審計：`$DATA_DIR/agents.log` 追加
  `<ts> spawned|despawned <name> <pane> <runtime>`。
- 測試：PATH shim 假 `codex`；成功路徑（registry 欄位齊、audit 行在）、
  cap 超限、失敗注入（runtime 指令不存在）零殘留。

## Phase 2：就緒自報到

- spawn 註冊時 `ready: false`；spawn 完成後 send-keys 探針
  `agent-bridge ready <name>` 進新 pane——REPL 真正就緒時會執行它，
  `cmd_ready` 把 registry 翻 `ready: true`（僅限 spawned agent）。
- 探針防遺失：spawn 後由 bridge 以間隔重送探針直到 ready 或逾時
  （`AGENT_BRIDGE_READY_TIMEOUT` 預設 30s；逾時不回滾、僅警告——pane
  留著供人工診斷）。
- `list` 增列 ready 欄；`send` 對 `ready: false` 的 spawned agent 印
  stderr 警告但不拒送（訊息在 mailbox 不會丟，僅通知可能延後）。
- 測試：慢啟動假 REPL shim（sleep 2 後才讀 stdin 執行）驗證 ready 翻轉
  與探針重送；啟動期按鍵被吃的情境由「重送直到 ready」機制覆蓋。

## Phase 3：despawn＋出身防護

- `cmd_despawn`：registry 必須 `spawned == true`，否則 die（人工
  register 的 pane 一律拒殺）；`tmux kill-pane` → 刪 registry → 審計。
- pane 已不存在（tmux 重啟過）：despawn 仍清 registry，不因 kill-pane
  失敗而卡住。
- 測試：despawn 人工 agent 被拒；despawn spawned agent 後 pane 消失、
  registry 刪除、audit 行存在；pane 已死的 despawn 仍成功清理。

## Phase 4：文件與整體驗證

- README：指令表加 spawn/despawn/ready、安全節（cap／出身／審計）、
  已知限制補「探針重送與 ready 語意」。
- SKILL.md：orchestrator 守則——spawn 前 list 查 cap 餘量、despawn 只殺
  自己 spawn 的 worker、任務完成後留用 vs 回收的判準。
- 全套驗證與真 codex 一輪實測。

## Phases × Gates

| Phase | Gate（機器可判） |
|---|---|
| 1 spawn 核心 | 新增測例全過：spawn 註冊欄位齊（jq 斷言 spawned/runtime）、cap 超限 exit≠0、失敗注入後 registry 無檔且 pane 數不變、audit 行 grep 得到；`bash -n`＋`shellcheck` 0 警告 |
| 2 就緒自報到 | 隔離 socket 下慢啟動 shim：ready 在 timeout 內翻 true（輪詢斷言）；探針重送生效（shim 首次吃掉輸入仍成功）；send 對未 ready agent 的 stderr 警告 grep 得到 |
| 3 despawn | despawn 人工 agent exit≠0 且 registry 檔仍在；despawn spawned agent 後 `tmux list-panes` 無該 pane id、registry 檔不存在、audit 行存在；死 pane despawn exit 0 |
| 4 收尾 | `bash tests/run-tests.sh` 全綠（0 FAIL）；shellcheck 全檔 0；README/SKILL grep 不變量（spawn、despawn、ready、agents.log 各至少一處）；真 codex spawn→派工→reply→despawn 一輪 0 人工介入（**human judgment**：REPL 實際就緒行為無法完全自動斷言，由使用者旁觀認證） |

## 遺留（不阻塞本計畫）

- claude runtime 的 worker 啟動 spec（permission mode／headless 與否）：
  獨立文件另審，接進 runtime 表即可。
- orchestrator 層（任務分派策略、池管理）：用 spawn/send/await 組合，
  另案。

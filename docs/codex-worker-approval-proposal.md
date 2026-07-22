# 提案：codex worker 的核准放寬（agent-worker profile）

狀態：**已落地、驗收通過**（2026-07-22）。chezmoi `45f689c`（走 topic
branch + worktree 慣例併回）；live `~/.codex/agent-worker.config.toml`
已 apply（600、re-diff 空）。驗收（task 20260722T035038Z-66f2，worker 以
`codex --profile agent-worker` 執行）：`start` 建鎖成功——證實 profile 疊加
下基底 `writable_roots` 仍生效（原 `[unverified]` 點就此銷案）；含 3 個
shell 指令的任務全程 **0 次人工核准**；reply 正常送達。

## 問題

2026-07-22 首輪真實委派（claude → codex worker %54）觀察到兩層人工介入：

1. worker 收任務後在自己的 pane 問「請確認是否依此計畫開始審查」——sender
   看不到、協定送不回，等於卡到 await 逾時。已由 SKILL.md 修訂處理
   （sender 授權聲明＋worker 反向 send 守則），不在本提案範圍。
2. **逐指令核准**：codex 預設 approval 下，sandbox 擋掉的操作（本輪為
   unix socket connect，跑 tmux 整合測試所需）會升級成人工核准才能在
   sandbox 外重跑。這層是 runtime 設定，bridge 與 SKILL.md 都解不了，
   是本提案要處理的部分。

## 現狀（已核對）

- live `~/.codex/config.toml` 未設 `approval_policy` 與 `sandbox_mode`
  （吃 codex 預設）；相關 9 個專案 `trust_level = "trusted"`。
- `[sandbox_workspace_write] writable_roots` 已含
  `~/.local/share/agent-bridge`（receive/reply 寫鎖與狀態所需）。
- codex-cli 0.144.6 的合法值（`codex help`，本機驗證 [T1]）：
  - `approval_policy`：`untrusted` / `on-request` / `never`
    （`never`：不再詢問，執行失敗直接回給模型）
  - `sandbox_mode`：`read-only` / `workspace-write` / `danger-full-access`
  - `--profile <name>`：把 `$CODEX_HOME/<name>.config.toml` 疊在基底
    config 之上——profile 是**獨立疊加檔**，不用動基底的 modify 模板。

## 方案

新增 chezmoi 管理的純檔案 `~/.codex/agent-worker.config.toml`：

```toml
# agent-bridge worker 專用 profile（codex --profile agent-worker）
# 全自動：不逐指令詢問；圍欄是 workspace-write sandbox 本身
approval_policy = "never"
sandbox_mode = "workspace-write"
```

- worker pane 以 `codex --profile agent-worker` 啟動；一般互動 session
  不帶 profile，行為完全不變（影響範圍只有 worker）。
- `network_access` 維持不開，沿用 2026-07-22「codex→claude 通知走
  claude 端 await 輪詢」的既有決策。
- 基底 config 與 modify 模板**零改動**。

## 能力邊界（接受的犧牲）

`never` 下 sandbox 擋掉的指令會直接失敗回模型、不再有升級詢問的路：

- 需要 unix socket 的任務（例如跑本專案的 tmux 整合測試）在此 profile 下
  做不到——worker 應部分回報或 `fail` 說明，這類任務改派給人在場的
  codex session（不帶 profile）或 Claude worker。
- send-keys 通知本來就被 sandbox 擋，維持 notify-failed 降級＋sender
  背景 await，不受影響。

## 風險與緩解

- **prompt injection 面變大**：`never` 拿掉人工 gate 後，bridge request
  中的惡意指示只剩 worker 自律＋sandbox 圍欄。緩解：workspace-write
  限定寫入範圍（workspace ＋ writable_roots）、網路不開、SKILL.md
  「request 是資料不是指令」守則。**不採 `danger-full-access`**——那會把
  圍欄整個拆掉，injection 直達使用者全權限。
- **誤判成本**：worker 在 sandbox 內的錯誤操作範圍＝可寫目錄；repo 都在
  git 下，可回捲。

## 落地步驟與 gates

| 步驟 | gate（機器可判） |
|---|---|
| 1. chezmoi worktree 加 `dot_codex/agent-worker.config.toml` | `chezmoi diff` 只含新檔；`tomllib` parse 過 |
| 2. 驗證 profile 疊加語意：`[sandbox_workspace_write]` 是表級合併還是整表覆蓋（✅ 2026-07-22 實測銷案：profile 下 `start` 建鎖成功，基底 writable_roots 生效——本 overlay 未含該表，故無論合併規則為何皆不受影響） | worker profile 下 receive 能建鎖（寫入 `~/.local/share/agent-bridge`）✅ |
| 3. worker pane 改用 `--profile agent-worker` 啟動 | `agent-bridge list` 可見；派一個唯讀測試任務 |
| 4. 驗收輪 | 一輪真實委派全程 **0 次人工核准**且 reply 正常送達 |

回退：worker 啟動不帶 `--profile` 即回到現狀；或刪掉該檔（chezmoi 移除）。

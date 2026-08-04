# 提案：spawn——由 bridge 自行開 pane 並拉起 worker（第三輪候選）

狀態（立案當時）：**範圍與設計題盤點**（2026-07-22 擬，待審）。尚未到實作
計畫層級；審過後若採納，再依 review-discipline 出正式計畫（含 phases × gates 表）。

**後續結果：已採納**——實作計畫見 `spawn-plan.md`，`spawn`／`despawn` 均已
落地。本文保留立案當時的狀態與設計題盤點，落地形狀以 `spec/` 為準。

## 動機

orchestrator/planner/executor 願景（2026-07-22 使用者定調）要求 worker
可按需擴充、收完活回收；目前 pane 的開啟、runtime 啟動、register 全靠
人工，orchestrator 沒有「手」。spawn 是缺的那個原語。

前置件：`docs/codex-worker-approval-proposal.md`（agent-worker profile）。
沒有「啟動即自動化」的 worker 設定檔，spawn 拉起的 codex 仍會逐指令等
人工核准，自動化不成立。

## 範圍（提議的最小形狀）

```bash
agent-bridge spawn <name> --runtime codex|claude [--window]
    # 1. tmux split-window（或 --window 開新 window）-P -F '#{pane_id}' 取得 pane
    # 2. 啟動對應 runtime（codex 帶 --profile agent-worker；claude 待定，見設計題 3）
    # 3. register <name> <pane_id>
agent-bridge despawn <name>
    # unregister ＋ kill pane（僅限 spawn 出來的 pane，見設計題 4）
```

非目標：spawn 不做排程、不做任務分派策略、不管理 worker 池——那些是
orchestrator 層（用 spawn/send/await 組合出來），不進 bridge 本體。

## 設計題（審查時要拍板的）

1. **就緒時序**：runtime 啟動要數秒，期間 send-keys 打進去的通知可能被
   TUI 啟動流程吃掉。選項：(a) spawn 後固定延遲才可派工；(b) worker 就緒
   後自報到（在啟動指令後追加一個 ready 標記寫入）；(c) sender 派工前
   輪詢探測。傾向 (b)：確定性最高、不靠猜延遲。
2. **生命週期**：任務完成後 pane 回收還是留用？留用省啟動成本且符合
   worker 守則「reply 後 /clear 接下一個」；回收保證乾淨 context。
   傾向：預設留用、despawn 顯式回收，讓 orchestrator 決定。
3. **claude worker 的啟動設定**：codex 有 profile 疊加檔；claude 端的
   對應物（permission mode、允許哪些工具、是否 headless `-p` 模式）是
   平行於 codex 提案的另一題，需要單獨一輪討論——spawn 第一版可以只支援
   codex runtime，claude 後補。
4. **安全面：能 spawn agent 的 agent**：這是新的授權面——被 injection 的
   worker 若能無上限 spawn，等於自我複製。至少要：spawn 上限（registry
   計數）、despawn 只准殺 spawn 出來的 pane（registry 標記出身，不准殺
   人工 pane）、spawn/despawn 事件留審計紀錄。是否要求「只有 sender 角色
   可 spawn」待議。
5. **失敗語意**：runtime 啟動失敗（指令不存在、profile 檔缺失）時
   spawn 要原子回滾（殺 pane＋不留 registry 殘骸）。

## 驗收方向（實作計畫時細化成 gates）

- spawn → 派工 → reply → despawn 全程零人工介入（配合 profile 提案）。
- despawn 對人工 register 的 pane 必須拒絕（registry 出身標記生效）。
- spawn 失敗路徑不留殘骸：registry 無條目、無殭屍 pane。
- 測試沿用獨立 socket（`tmux -L agent-bridge-test`），不碰真實 server。

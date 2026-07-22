# orchestrator 層實作計畫

狀態：**五個 phase 全部完成**（Phase 4 於 `2c41e6e`，Phase 5 真鏈驗收 2026-07-22 通過）

## Phase 5 真鏈驗收記錄（2026-07-22，4 個真 claude worker）

跑法：spawn w1 → 派一個「只准回 5 行、細節留在 context」的調查任務 → 再 spawn
w2/w3/w4 撞滿 cap → 第 5 個 spawn 被拒 → `idle` 看池況 → `evict w1` → `read`
收尾筆記 → spawn w5 成功。

Gate 結果（全過）：
- 撞 cap 拒絕 → evict 後 spawn 成功；`evict` 全程 28 秒
- 收尾筆記 `read` 讀得回，內容含 file:line、「已驗證 vs 只是推測」的分級、
  死路與未解疑問——**human judgment 判定：有實質價值**，不是空話
- 執行前後 `locks/` 皆為空
- `agents.log` 審計成對：`spawned w1` … `despawned w1` ＋ `evicted w1`

兩個實地發現：

1. **`idle_secs` 名稱重用 bug（已修）**：`last_task_at` 只認 agent 名不認 pane
   實例，tasks/ 又是長期累積的，於是剛 spawn 39 秒的 w1 顯示 `idle_secs=21686`
   （繼承同名前一個 worker 今早的任務）。後果是 LRU 優先驅逐一個還沒做過事的新
   pane，決策視圖說謊。修法：`base` 取 `last_task_at` 與 `spawned_at` 較晚者。
   已補測例 §25（破壞-復原：撤掉修補 → 精準紅 1 條）。
2. **LRU 的反直覺處（寫進 brief）**：剛做完一輪大任務、之後沒再被派工的 worker
   `idle_secs` 反而最大——LRU 挑中的往往正是殘值最高的那個。這不是排序壞了，
   正是 evict 要先讓筆記落地的理由。

Phase 3 落地時與本文的差異（以實作為準）：
基線：`df400c5`（relay 已 shipped，真鏈驗證已通過）；Phase 1/2 於 `b770121` shipped

Phase 3 落地時與本文的差異（以實作為準）：

- 審計記號分**三種**而非兩種：`evicted`（筆記已落地）／`evicted-timeout`（逾時）／
  `evicted-unfinished`（收尾任務以 failed/cancelled 收場）。`await` 對後者是正常
  返回而非逾時，全記成 `evicted-timeout` 會讓審計線說謊。
- 介面多一個 `--from <sender>`（預設 `orchestrator`），讓「誰發起驅逐」進得了
  任務 metadata；`--timeout` 預設 300 秒。
- 收尾任務文案**硬編在 `cmd_evict`**，不抽到 `share/`：它是機制的一部分，抽成
  檔案只會多一條「檔案不見了」的失敗路徑，而那條路徑一旦失敗等於筆記機制
  悄悄消失。

## 核心決定（本輪議定）

1. **pane 生命週期由「上下文殘值」決定，不是做完就殺。**
   預設**保留**；只有 worker 明確宣告「我這輪的脈絡沒有殘值」才可即時回收。
   失效方向因此是安全的——worker 忘記宣告＝留著，不會誤殺。
2. **判定者是 worker，orchestrator 可否決。** 只有 worker 知道 `response.md`
   以外還留了什麼在腦裡；但 cap 壓力下 orchestrator 有最終回收權。
3. **撞 cap 時 LRU 驅逐，且強制落地筆記**——先派一輪收尾任務讓它把還記得的
   關鍵事實寫成 response，寫完才 despawn。上下文不會憑空消失。
4. **機制進 CLI，策略寫成 brief。** 沿用本 repo 既有分工（安全不變量在
   `bin/agent-bridge`，守則在 `share/*.md`）。

## 關鍵設計約束（先查清楚才敢動）

- **單一全域鎖**：`LOCK_DIR` 是單值（`bin/agent-bridge:112`），`release_lock`
  只釋放一把 → **無法同時持有 task 鎖與 registry 鎖**。故保留標記**不掛在
  `reply` 上**（那需要兩把鎖），改走獨立命令 `disposable`。
- **`spawn` 保持純粹，不自動驅逐**。不加 `--evict-if-full`：那會讓一條經過
  九輪複核的路徑長出「自動殺 pane」的能力。撞 cap 時由 orchestrator 明確發起
  `evict`，殺與不殺永遠是一個獨立、可審計的決定。
- **`retain` 不是保護，只是建議**。registry 位在 worker 可寫的資料目錄，
  worker 可以自己改。但 `despawn` 認的是 `spawn_tag` 不是 `disposable`，
  所以 worker 賴不掉。這點要寫進註解，免得後人誤以為它是權限。
- **`idle` 唯讀、零侵入**：不動 `send`（核心路徑，加寫入會擴大失敗面），
  改用掃 `tasks/` 目錄名（本就是 UTC 時間戳）算閒置。代價是 O(task 數)，
  長期需要 task GC——列為後續 backlog，不在本輪。

## 介面（新增三個原語 ＋ 兩份 brief）

```
agent-bridge disposable <name>      （worker）宣告本輪脈絡無殘值，可即時回收
agent-bridge idle                   列出 worker 池狀態：name / ready / disposable / idle_secs
agent-bridge evict <name> [--timeout <secs>]
                                    驅逐：派收尾任務 → 等筆記落地 → despawn；印收尾 task-id
```

- `share/orchestrator-brief.md`：主導者策略（何時複用、何時 spawn、撞 cap 流程、
  什麼任務值得走到第三層）
- `share/worker-brief.md`：**改寫**（不只增補）——見下方「worker-brief 的既有條款衝突」

`evict` 逾時仍然 despawn（否則 cap 永久卡死，LRU 就失效），但審計要留下
`evicted-timeout` 記號，讓「筆記沒落地」這件事看得見。

## worker 是完整 session，不是 subagent（本輪釐清）

`spawn` 起的是 `claude --permission-mode auto`（`bin/agent-bridge:756`），沒有任何
收窄工具的旗標，且**刻意不加** `--settings/--setting-sources`（註解 748-751）。
所以每個 worker 是完整的 Claude Code session：完整工具集（含 Agent tool）、
自己的 context window、使用者全域 CLAUDE.md 與 `rules/*.md` 全部生效。

層級因此是三層，各有獨立 context：

| 層 | 身分 | 能否再往下委派 |
|---|---|---|
| 主 session | orchestrator | 能 |
| agent-bridge worker | 完整 `claude` session | **能**——它就是一個主 session |
| Agent tool subagent | 內建 subagent | 不能 |

這是 agent-bridge 相對內建 subagent 的關鍵價值：**可再分支的一層**。

### 已實測驗證（2026-07-22，task `20260722T113952Z-c90a`，等級 Verified）

worker `nesttest`（claude runtime）實跑一輪確認：

- **Agent tool 可用**，成功 dispatch 一個 `Explore` subagent 並取回結果
  （38.7k tokens / 22s）。可見的 agent type 與主 session 一致（含使用者自訂的
  Explore / executor / verifier / security-executor 等）。
- **全域設定確實載入**：worker 能引述 CLAUDE.md 的輸出語言規則與
  context-discipline 的 >3 檔／>2K token 委派門檻，連 project memory 都在 context 裡。
- **worker 自發執行了 exploration verification gate**：它對 Explore 的回報做了
  獨立 grep 複核才寫進 reply。繼承的紀律是真的在跑，不只是文字。

**關鍵發現 ——「worker 預設不會主動 fan-out」**：worker 的系統 prompt 含
「Do not call the AgentTool unless the user requested it」。本次是任務明確要求
才呼叫。故 **orchestrator 若要用到第三層，必須在 request 裡明確授權**，否則
worker 會一律自己做完。這條要寫進 orchestrator-brief。

**一則需校正的 worker 觀察**：worker 回報「這個 session 沒有 `Grep`/`Glob`
內建工具，跟一般 Claude Code session 不同」。前半正確、後半錯誤——**主 session
同樣沒有 Grep/Glob**，兩邊工具集一致，不是 worker 特有的收窄。對計畫反而是好
消息：派工時不需要為 worker 另設工具假設。（附帶影響：使用者全域
`rules/tool-preferences.md` 要求「用 Glob 找檔、Grep 搜內容」，在當前環境兩者
都不存在，實際只能走 Bash + `rg`/`grep`——這是該規則自身的過時項，與本計畫
無關，但值得另外處理。）

### worker-brief 的既有條款衝突（必須改，不是增補）

`share/worker-brief.md:52` 現寫「完成 `reply` 後先清空自己的 context
（`/clear` 或等價操作）再接下一個任務」。**這條與「保留上下文供追問」直接矛盾**
——照做的話 reply 完脈絡就沒了，保留 pane 只剩空殼，`disposable` 宣告與 LRU
驅逐全部失去意義。Phase 4 必須拔掉或改寫這條。

### 巢狀委派判準與主 session 不同

使用者全域的 context-discipline（>3 檔或 >2K tokens 就 dispatch）是為了保護
**必須長期存活的主 context**。worker 繼承這套規則，但處境不同，且新設計讓它更
微妙：

- 若這個 worker 之後**不會**被追問 → context 可拋，委派門檻該放寬
  （在可拋的 context 裡再開一層，成本乘上去、收益小）
- 若它**會**被追問 → 委派反而有害：原始輸出被委派走了，worker 自己也只拿到
  結論，追問細節時它答不出來

worker 做事當下不知道自己會不會被追問，所以判準要換一個維度：
**「這批原始輸出，正是之後可能被追問的東西嗎？」是→自己讀；否→照常委派。**
這條寫進 worker-brief，明確標示它與主 session 那套規則不是同一條。

## Phases × Gates

| # | Phase | 內容 | Gate（機器可判定） |
|---|---|---|---|
| 1 | `disposable` 標記 | registry 加 `disposable` 欄位；非 spawned agent 拒絕；registry 損壞 fail-closed | 新測例全綠；`shellcheck` + `bash -n` 0 警告；破壞-復原：拆掉 `is_spawned` 檢查 → 至少 2 條紅 |
| 2 | `idle` 查詢 | 唯讀掃 registry + tasks，輸出四欄 TSV | 輸出欄位順序被測例鎖住；`tasks/` 空／目錄名損壞／registry 損壞三種情況不崩；驗證執行期間 `locks/` 始終為空 |
| 3 | `evict` | send 收尾任務 → await → despawn 三步 | 三步各有測例；逾時仍 despawn 且 `agents.log` 出現 `evicted-timeout`；破壞-復原：拆掉「等 response 落地」→ 紅 |
| 4 | brief | orchestrator-brief 新增；**worker-brief 改寫**（拔掉 `/clear` 條款、加巢狀委派判準） | 兩檔存在且注入路徑讀得到（沿用既有 `-f && -r` 測例形狀）；`grep` 斷言 worker-brief 已不含「reply 後清空 context」語句；巢狀委派判準有對應段落 |
| 5 | 真鏈驗收 | 開滿 cap 4 → evict LRU → 成功 spawn 第 5 個 | cap 撞上後 spawn 成功；收尾筆記能用 `read <task-id>` 讀回；`locks/` 無殘留；審計線成對 |

Phase 5 是**人工判斷 + 機器證據混合**：cap 與 locks 是機器可判定的，
「筆記內容是否真的有用」需要人看一眼——這條標為 human judgment。

## 測例陷阱（這個 repo 已踩過四次）

寫否定式／注入式斷言前，**先確認前面沒有別的防線在替它擋**（§22c、§23e 的教訓）。
`evict` 的測例特別容易空綠：收尾任務若根本沒送出，「response 未落地」這條斷言
會因為前一道檢查先失敗而看起來是綠的。

**第六次實際踩到（Phase 3，M1 抓出來的）**：§26 的成功路徑用背景 replier 模擬
worker 回覆，而斷言原本寫在 `wait` **之後**——那等於先等 replier 做完才看狀態，
任務當然是 completed，即使 `evict` 根本沒等過。M1（把「等落地」換成直接宣稱
completed）當時整組空綠。修法是把狀態取樣移到 `wait` 之前。
教訓推廣：**凡是斷言「某件事在某個時間點之前已經發生」，取樣就必須在那個
時間點，不能在測試尾端補看**。

## 已知風險

- **保留 ≠ 上下文品質保留**：worker 閒置期間可能已被 auto-compact，
  「還記得」是遞減的。收尾筆記機制正是為此存在，但無法完全補償。
  orchestrator-brief 要寫明：追問越晚，可信度越低。
- **`evict` 是複合命令**（send + await + despawn），失敗模式比現有任何一個
  子命令都多。逾時、worker 已死、收尾任務被 cancel 都要有明確行為。
- **成本是乘的**：orchestrator → worker → subagent 三層疊起來，按使用者
  cost discipline 的估算（subagent ~4×）相當可觀。orchestrator-brief 要寫明
  什麼任務值得走到第三層，別讓「能分支」變成「預設分支」。
- **worker 的 subagent 對 orchestrator 不可見**：只看得到 `response.md`。
  這符合 context 隔離的初衷，但代表 orchestrator 無法分辨 worker 是自己讀的
  還是委派拿結論的——而這正好影響追問的可信度。追問前無從得知答案品質。

## 不做（本輪明確排除）

- task 目錄 GC（`idle` 的掃描成本問題）→ backlog
- 自動分派策略編碼進 shell（策略留在 brief）
- `spawn --evict-if-full`（見上方設計約束）

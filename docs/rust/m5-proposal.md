# M5 提案：兩個殘餘窗的關閉手段

狀態：**可行性已驗證，實作未開工**，等使用者裁決路線。
前段：`docs/owner-gate-boundary-assessment.md`（2026-07-29 的兩窗評估，
結論是 bash 期維持現狀、真正的關閉手段留給 Rust 期）。

計畫（`~/.claude/plans/cheeky-waddling-meadow.md`）把 M5 寫成
「daemon 單一寫者關認領窗 → 行程級身分（pid＋starttime）關 TTL 降級窗」。
本提案的核心發現是：**關 TTL 降級窗不需要 daemon**，行程級身分自己就夠。

## 1. 量測（2026-07-31，本機實測）

方法：臨時 hooks settings 把三個事件接到一支探針腳本，腳本記錄自身祖先鏈後
`exec` 交還真的 `agent-bridge hook`（stdin payload、argv、exit code 全部
不碰）。spawn 一個 claude worker（`m5probe`），先取本尊事件，再派一個任務
叫它在自己的 pane 裡跑一個巢狀 `claude -p`，取巢狀事件。量完 despawn。

### 本尊的 hook

```
hook_pid=1272513  ppid=1271983  bash
        1271983   ppid=9411     claude      <- worker 本尊
           9411   ppid=1        tmux: server
```

### 巢狀 session 的 hook

```
hook_pid=1276491  ppid=1276073  bash
        1276073   ppid=1276041  claude      <- 巢狀 session
        1276041   ppid=1271983  bash
        1271983   ppid=9411     claude      <- worker 本尊
           9411   ppid=1        tmux: server
```

### 三個決定性事實

1. **hook 行程的直接 PPID 就是啟動它的那個 runtime 行程**。本尊事件的 PPID
   是 worker 本尊（1271983），巢狀事件的 PPID 是巢狀 session（1276073）。
   兩者精確可分——差別在**直接父行程**，不需要走祖先鏈（祖先鏈方案在
   `spec/hooks.md` HOOK-OWNER-4 Note 已被否決，理由正是巢狀的祖先鏈必然
   包含本尊，區分不出來；直接 PPID 沒有這個問題）。
2. **`pane_pid` 就是 worker 本尊的 pid**：tmux 直接 exec runtime，中間沒有
   shell 層（實測 `tmux display -p '#{pane_pid}'` = 1271983 = hook 的 PPID）。
   spawn 端因此拿得到本尊 pid，不必去猜哪個子行程才是 runtime。
3. **巢狀 session 繼承同一個 `AGENT_BRIDGE_SPAWN_TAG`**（探針記到的 tag
   兩邊完全相同）。這重新確認了 spec 既有的判斷：tag 比對擋不住巢狀冒名。
   PPID 可以。

## 2. 三條路線與代價

| 路線 | 窗 1（/clear 後 TTL 降級） | 窗 2（無鎖先到先得認領） | 代價 |
|---|---|---|---|
| **A. 行程身分，無 daemon** | **關**：`/clear` 換 session_id 但行程沒變，PPID 相符即放行，不必等 TTL | 維持現狀 | 小。spawn 多記兩個欄位，hook 多一次比對 |
| B. 常駐 daemon 單一寫者 | 關 | **關** | 大。README 的 no-daemon 賣點要改；hook 改走 IPC；daemon 崩潰的降級路徑一旦退回現行寫法，兩個窗就都回來了 |
| C. A + daemon 選配 | 關（預設路徑） | 關（opt-in 時） | 兩套所有權判別並存，spec 條款與測試分組各寫一份 |

窗 2 的性質值得重述（引評估備忘）：它的前置條件自我矛盾——認領發生在 worker
第一個 hook 事件（實測在派工之前），而巢狀 runtime 只能在 worker 執行任務時
出現，**競態窗存在的那一刻對手還不存在**。加鎖的代價（hook 可能 die、可能
sleep 重試）直接違反 hook 的 exit 0 鐵律與延遲要求。

## 3. 若走 A：設計草案

### 資料面

`spawn` 成功時，registry 多記兩欄：

- `worker_pid`：`tmux display -p '#{pane_pid}'`
- `worker_starttime`：`/proc/<pid>/stat` 第 22 欄（防 pid 重用；lstart 的
  秒級解析度不夠）

兩欄取不到時留空 —— 空值＝退回現行的時間窗判別（見下面的失效方向）。

### 判別面

`hook_owner_gate` 的順序改成：

1. registry 有 `worker_pid`＋`worker_starttime`，且本行程的 PPID 與之相符
   → **放行**（不看 session_id、不看 ts）。這條吃掉 `/clear` 自癒。
2. 相符欄位存在但 PPID 不符 → **擋**（這是巢狀冒名的正解，不再受 TTL 上限
   約束，也就是說冒名者不會因為等久了就取得資格）。
3. 欄位缺、`/proc` 讀不到、runtime 不是 claude → **退回現行的
   session_id + TTL 判別**，行為與今天完全相同。

### 失效方向

第 3 條是整個設計的安全閥。PPID 這條路徑的前提是「runtime 直接 fork hook」，
那是實測結果、不是契約——claude 未來版本改用中介行程的話，判別會失準。
所以判別只在**明確相符**時放行，任何不確定一律落回今天的行為。這讓 M5 的
最壞情況等於現狀，而不是比現狀更糟。

### Gate（可機器判定）

1. 新增分組：本尊 PPID 相符放行、巢狀 PPID 不符被擋、欄位缺退回時間窗、
   starttime 不符（模擬 pid 重用）被擋 —— 四條 hermetic 斷言，用假的
   `/proc` 視圖或注入點餵資料，不需要真的起巢狀 session。
2. 既有 756 不退。
3. `check-contract` 4/4；新條款編號只增（`HOOK-OWNER-5`、`STATE-CHAN-4`
   或類似），`traceability.md` 同步。
4. 真實 canary 一次：重跑本文件第 1 節的量測流程，確認本尊放行、巢狀被擋。

## 4. 未量測 / 殘餘風險

- **codex runtime 沒量**。codex 的 hook 走 profile overlay，行程樹未必相同。
  A 路線對 codex 的效果目前是未知，保守假設是落回第 3 條（退回時間窗），
  也就是 codex worker 維持現狀。要收就得補一次同樣的量測。
- **`pane_pid` 等於 runtime 本尊，依賴 tmux 直接 exec**。本機實測成立，但
  這是 tmux 的行為不是承諾；若哪天中間多一層 shell，spawn 記到的會是 shell
  的 pid，判別會全面落回第 3 條（安全，但窗又回來了）。值得在 gate 裡加一條
  「spawn 記錄的 pid 確實是 runtime 行程」的斷言。
- **本提案沒有碰窗 2**。要關它仍然只有 daemon 一條路，而那是獨立的一件事，
  不該綁在 M5 一起做。

## 5. 建議

走 A，把窗 2 留在現狀並保留評估備忘的論證。理由是代價比（改兩個欄位＋一次
比對 vs 引入常駐行程並改寫產品定位）與失效方向（最壞退回現狀）都明顯有利，
而 daemon 真正買到的只有窗 2 —— 一個實務上對手不存在的競態。

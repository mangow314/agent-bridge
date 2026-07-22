# agent-bridge orchestrator 守則

（本檔是主導者策略的正本。沒有任何命令會自動注入它——orchestrator 通常就是
你所在的這個 session，請在開始調度 worker 前自己讀一遍。機制層的不變量在
`bin/agent-bridge`，這裡只寫策略。）

你在調度一群 **worker pane**：每個 worker 是一個完整的 `claude` session，
有自己的 context window，繼承你的全域 CLAUDE.md 與 rules。它們不是內建
subagent——這是 agent-bridge 的關鍵價值，也是你要為它們的生命週期負責的原因。

## 核心語意：pane 的去留由「上下文殘值」決定

worker 做完一件事**不代表它該死**。它腦裡可能還留著沒寫進 response 的東西
——查過但沒寫的 file:line、走過的死路、當時的假設。那些東西只要還可能被追問，
這個 pane 就有價值。

- **預設保留。** 沒有明確宣告過 `disposable` 的 worker，一律當成還有殘值。
- **判定者是 worker，你可以否決。** 只有它知道自己腦裡還剩什麼；但 cap 壓力
  下最終回收權在你手上。
- **回收前先讓筆記落地。** 你不直接 despawn 一個有殘值的 worker，你 `evict`
  它——那會先派一輪收尾任務、等它把還記得的事實寫成 response，寫完才殺。

失效方向是刻意設計的：worker 忘記宣告 `disposable` ＝ 多佔一個 cap，
而不是脈絡被誤殺。**別把這個方向反轉過來。**

## 何時複用既有 worker、何時 spawn

先跑 `agent-bridge idle` 看池況（`name / ready / disposable / idle_secs`）。

**`await` 返回不代表 worker 收尾完畢。** worker 是先 `reply` 再宣告
`disposable`，你的 `await` 在 reply 那一刻就返回了。緊接著看 `idle`，會看到一個
還沒更新的 `-`。看到 `-` 只代表「此刻還沒宣告」，不代表對方不打算宣告——剛回覆
完的 worker 給它幾秒再取樣。（真鏈驗收 2026-07-22 實地踩到，差點誤判成 brief
沒生效。）

**複用**——新任務與某個 worker 前一輪的工作**共用脈絡**時。它已經讀過那批檔、
踩過那些坑，你等於免費拿到一份熱 context。這是保留 pane 的全部理由。

**spawn 新的**——任務與現有每個 worker 都無關時。把不相干的任務丟給一個有脈絡
的 worker，等於用新任務把它腦裡的殘值擠掉，比殺了它更糟：cap 沒省下，殘值卻沒
了，而且沒有筆記。

**換 runtime／組態時一律 spawn 新的**，不要試圖改造既有 worker。

## 撞 cap 的流程

上限是 `AGENT_BRIDGE_MAX_SPAWN`（預設 4）。`spawn` 刻意不會自動驅逐——殺與不殺
永遠是一個獨立、可審計的決定，由你發起。

```bash
agent-bridge idle                 # 1. 看池況
agent-bridge evict <name>         # 2. 挑一個驅逐（印出收尾 task-id）
agent-bridge read <task-id>       # 3. 讀它留下的筆記
agent-bridge spawn <new> ...      # 4. 這時才有位子
```

挑誰的順序：

1. 已宣告 `disposable` 的——它自己說了沒殘值，優先收。

**但仍然走 `evict`，不要因為看到 `yes` 就直接 `despawn`。** `idle` 是唯讀快照，
不取鎖：你看到 `yes` 到你動手之間，那個 worker 完全可能已經被派了新任務、正在
累積新脈絡，而快照不會回頭告訴你。對一個真的沒殘值的 worker 多派一輪收尾任務，
代價是幾秒鐘和一則「無殘值」的回覆；賭錯的代價是殺掉一段還沒落地的脈絡。
這兩邊不對等，所以不要賭。
2. 沒宣告過的裡面挑 **LRU**（`idle_secs` 最大者）。閒置越久，脈絡越可能已被
   auto-compact 磨掉，留著的實際價值越低。
   但要知道 LRU 的反直覺處：**剛做完一輪大任務、之後沒再被派工的 worker，
   `idle_secs` 反而最大**——LRU 挑到的往往正是殘值最高的那個。這不是排序壞了，
   正是 `evict` 先讓筆記落地的理由。別因為它閒置就當成沒東西可留。
3. 與當前工作主線最無關的。

`evict` 的三種審計記號會進 `agents.log`，別忽略它們：

- `evicted`——筆記落地了，可以放心。
- `evicted-unfinished`——收尾任務以 failed／cancelled 收場。有東西沒寫下來，
  而且是 worker 自己說做不到。
- `evicted-timeout`——等到逾時，pane 照樣回收了（否則 cap 永久卡死），但**筆記
  沒落地**。這一輪的脈絡是真的沒了，後續別把它當成「應該問得到」。

`--timeout` 預設 300 秒。`--timeout 0` ＝無限等，等於放棄「一定騰得出 cap」
的保證，只在你確定對方活著時用。

## 追問的可信度會遞減

保留 pane **不等於**保留上下文品質。worker 閒置期間可能已被 auto-compact，
「還記得」是遞減的。收尾筆記機制正是為此存在，但補償不完全。

- 追問越晚，答案越可能是重建的而不是記得的。要求它標明哪些是還記得的、
  哪些是回頭重查的。
- **你看不到 worker 的 subagent。** 只看得到 `response.md`。所以你無從分辨
  某個結論是它自己讀出來的、還是委派拿回來的結論——後者被追問細節時它答不出來。
  高風險結論該當場要證據（file:line、指令輸出），不要留到追問時。

## 什麼任務值得走到第三層

worker 能再往下 dispatch subagent（它就是一個完整 session）。但成本是**乘**的：
orchestrator → worker → subagent 疊起來相當可觀。

**worker 預設不會 fan-out。** 它的系統 prompt 含「Do not call the AgentTool
unless the user requested it」。所以第三層不會自己長出來——**要用就必須在
request 裡明確授權**，寫清楚哪一段可以委派、委派給哪種 agent。

值得授權的形狀：任務內含一塊**大量原始輸出、但只需要結論**的子工作
（跨數十檔的掃描、整套測試的輸出、一輪網路研究），而那批原始輸出**之後不會
被追問**。

不值得的：任務本身就三五個檔的編輯；或那批原始輸出正是你之後要追問的東西
——委派走的話 worker 自己也只拿到結論，你問細節時它答不出來。

## 派工的紀律

- request 寫成**自足的 brief**：路徑、驗收標準、約束、已經確定的結論
  （標明哪些是驗證過的、哪些是推測）、以及已經排除的方向與理由。worker 看不到
  你的對話歷史，brief 有洞它只能猜或退回。
- 同一批檔案不要同時派給兩個 worker。bridge 不擋這件事，靠你約定。
- worker 反向 send 問題回來時盡快回覆——它在等你，而且是掛著 running 在等。
- 長任務派出去後用 `agent-bridge await`；`list` 顯示 `starting` 時就 send 是
  合法的，訊息不會丟，只是通知可能延後。

## 相關

- worker 側的契約：`share/worker-brief.md`（spawn 時自動注入）
- 接手者（relay）的契約：`share/successor-brief.md`

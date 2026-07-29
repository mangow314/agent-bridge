# owner gate 殘餘邊界評估備忘

- 日期：2026-07-29
- 環境：main @ 5aa2488 之 working tree（bin/agent-bridge 未改動）；判讀
  基礎為當日 `bash tests/run-tests.sh` 單次 754 PASS/0 FAIL（分組 34.5+
  含 gate 全分支 hermetic 斷言）＋同日真實 runtime canary
  （docs/nested-runtime-canary.md，claude sonnet/haiku）。
- 範圍：cross-vendor 複核（2026-07-29，nn-review）判**非阻斷**的兩個殘餘窗，
  評估「維持現狀／縮窗／留待 Rust 期」三擇一。
- 輸入：spec/hooks.md（HOOK-OWNER-1…4）、docs/nested-runtime-canary.md
  （P5 實測）、bin/agent-bridge `hook_owner_gate` 註解的原始論證。
- 復現：本備忘是判讀不是實驗；重建判讀基礎＝重跑
  `bash tests/run-tests.sh`（單次）＋依 nested-runtime-canary.md 的復現
  指令重跑 A/B 兩段。

## 決定性觀察（本文件立論所依）

1. 被 gate 擋下的異主寫入副作用為零：canary A2 前後 state 檔位元組級相同
   （`diff` 空輸出）。
2. 過期接管真實可走：canary A3 stale ts 後 owner 由 `09b5434c-…` 接管。
3. 認領時點早於任何巢狀機會：canary B 段 worker 在**派工前**（ready 回報
   輪）state 即誕生且 owner 已是本尊 `10220aed-…`；巢狀 runtime 只能在
   worker 執行任務時出現。
4. hermetic 面：分組 34.5+ 覆蓋無檔認領／異主新鮮拒絕／壞值 TTL 韌性
   （`tests/run-tests.sh:3309` 起）。

## 窗 1：/clear 後的 TTL 降級窗

**現象**：parent /clear（或重啟 session）後 session_id 換新，state 檔 owner
仍是舊 id 且 ts 新鮮 → parent 自己的 hook 被 gate 擋，state 停更、stop 不
block，最長 TTL（預設 1800 秒）後才由接管路徑自癒（HOOK-OWNER-2 的過期
接管分支）。

**結論：維持現狀。**

理由：
1. 降級期間的行為就是設計的失效方向：state 停更 → 通知端把它當未知 → 退回
   legacy 送鍵（HOOK-NOTIFY-1），任務不遺失、不餓死；canary A2 實證被擋
   期間 state 位元組級不動、副作用為零（P5 A 段）。
2. 縮窗的唯一旋鈕是調低 TTL，但同一個 TTL 同時是冒名防護窗——調低等於讓
   巢狀 session 更早取得接管資格（HOOK-OWNER-3 的兩個極端論證）。「parent
   新 session」與「巢狀 session」在 hook 端不可區分（HOOK-OWNER-4 Note，
   PPID 方案已否決），沒有只縮自癒、不縮防護的參數空間。
3. canary A3 實證過期接管路徑真實可走（P5：stale ts 後 owner 由
   `09b5434c-…` 接管），自癒不是理論。

Rust 期備註：若 ab-core 走 daemon／單一寫者，可持有 worker 行程握把
（spawn 時記 pid＋啟動時間），身分判別不再依賴時間窗，此窗自然消失。

## 窗 2：無鎖先到先得的認領窗

**現象**：state 檔首次認領不取鎖（STATE-CHAN-1 單一寫者假設）；理論上兩個
不同 session_id 在「無 state 檔」瞬間同時通過 gate（HOOK-OWNER-2 的無檔
放行分支）、雙雙寫入，後寫者勝——冒名者可能成為 owner。

**結論：維持現狀。**

理由：
1. 前置條件自我矛盾：認領發生在 worker 的第一個 hook 事件（實測 spawn
   完成、ready 回報那一輪 state 即誕生——P5 B 段 worker 於派工前 owner 已
   是本尊 `10220aed-…`；notify-native canary 同款觀察）。巢狀 runtime 只能
   由 worker 執行任務時啟動，而執行任務必然晚於 worker 自己的第一個事件
   ——競態窗存在的時刻，對手還不存在。
2. 寫入本身原子（STATE-GEN-2 的 rename 語意），競態的最壞結果是整檔覆寫、
   不會撕裂；且錯誤認領仍受 TTL 上限約束，過期後正主可接管（HOOK-OWNER-2）。
3. 加鎖的代價違反 hook 鐵律的風險剖面：acquire_lock 會 die（HOOK-ID-2 禁止
   非零）、會 sleep 重試（Stop hook 延遲直接拖慢 worker 每一個 turn），為
   一個實務上無對手的窗引入常態成本，方向不對。

Rust 期備註：同窗 1——單一寫者行程化後免費消失，不值得在 bash 層先付費。

## 總結

兩窗均維持現狀，皆已有文件化的失效方向與實測自癒證據；真正的關閉手段
（行程級身分、單一寫者）屬 Rust 遷移的架構紅利，不做 bash 層補丁。

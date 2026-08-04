# lifecycle 安全設計：完整論證

> 這裡收的是 spawn／despawn／evict／gc 安全不變量的完整論證——反例、取捨、
> TOCTOU 分析、被獨立複核推翻又補回來的過程。結論式摘要在
> [README.zh-TW.md](../README.zh-TW.md) 的生命週期各章；本檔回答「為什麼是
> 這樣設計」。行為正典在 [spec/cli.md](../spec/cli.md) 與 `tests/run-tests.sh`，
> 與本檔衝突時以正典為準。（2026-08-04 自 README.zh-TW.md 搬入，僅重排與
分節，實質語意未變。）

## spawn cap 與並行

`.spawned == true` 的 agent 數達 `AGENT_BRIDGE_MAX_SPAWN`（預設 4）時 spawn
被拒；cap 檢查＋建 pane＋註冊包在 registry 專用 mkdir 鎖內，並行 spawn 無法
繞過上限。人工 register 的 agent 不計入。

## 出身標記：誰能被 despawn

只有 registry 帶 `spawned: true` 的 agent 能被 `despawn`；人工註冊的 pane
一律拒殺（要移除用 `unregister`，不碰 pane）。反向也擋：`register` 不覆寫
spawned agent、`unregister` 不除名 spawned agent——否則出身標記會被洗掉，
pane 變成沒人能回收的孤兒。`register` / `unregister` / `spawn` / `despawn` /
`ready` 共用同一把 registry 鎖，出身檢查一律在鎖內完成，杜絕「檢查通過後
registry 被換掉、卻殺到別人 pane」的競態。

## spawn_tag：pane id 不是身分證明

tmux 的 pane id 來自 server 內的計數器，換一個 server 或 server 重啟後會
重新發到同一個 `%N`。因此 spawn 時會在啟動指令埋一個一次性 tag
（`ab-spawn-<agent 名>-<pid>-<48 位隨機>`）並存進 registry 的 `spawn_tag`，
`despawn` 只有在目標 pane 的啟動指令確實帶著該 tag 時才動手。對不上時
**不動那個 pane**，只清掉過時註冊並在 stderr 警告，審計記為 `despawn-stale`。
tag 本身也驗格式且綁 agent 名字：否則 registry 裡填個 `spawn_tag: "bash"`
就成了萬用鑰匙（任何以 bash 起手的 pane 都會被前綴命中），抄另一個 worker
的 tag 也能借刀殺人。

## 驗證與 kill 的原子性

拆成兩次 tmux 呼叫的話，中間 server 若死掉重啟、新 server 把同一個 `%N`
發給人工 pane，第二次呼叫就殺錯人。實作用 `tmux if-shell -F` 讓「再驗一次
tag」與 `kill-pane` 原子地一起發生，換了 server 就判 false 不動手；隨後再
確認 pane 真的消失，沒消失就報錯並保留 registry。**spawn 的回滾走同一套**
（回滾同樣是拿著一個 pane id 去殺，同樣可能落到已被替換的 server）。

## registry 的值進 tmux 前先驗格式

`pane_id` 必須是 `%<數字>`。它會被展開進 tmux 命令字串，而 tmux 命令裡
`;` 是分隔符——一個寫成 `%1 ; kill-server` 的 pane_id 足以殺掉整個 tmux
server。

## 判不出來就不動手（fail-closed）

出身判斷把「JSON 損壞／讀不到」與「不是 spawned」分開處理，前者一律拒絕
操作並要求人工確認——把兩者壓成同一個「非 spawned」，一份壞掉的 registry
就會被 `register` 覆寫、被 `unregister` 除名（pane 從此沒人回收），也不計入
cap。cap 計數則相反地保守：無法解析的 registry 照樣佔一個名額。

## 無法確認就不清註冊

`despawn` 若連 tmux 查詢都失敗（server 不可達、sandbox 擋 socket），或
`kill-pane` 失敗，一律非零退出且**保留 registry**。把這兩種失敗當成「pane
已不存在」會製造沒人回收得掉的孤兒 pane。只有查詢成功且確認 pane 不存在
時，才會單獨清除註冊。

## 原子回滾與孤兒掃除

spawn 中途任一步失敗（含 runtime 啟動即死的夭折偵測）→ kill 已建 pane、
刪 registry 檔、非零退出。pane id 還沒回到手上就失敗的情況也涵蓋：啟動指令
帶一次性 tag，回滾靠它把孤兒 pane 掃出來（比對錨在指令開頭，不會誤殺參數裡
碰巧含同一串的別人 pane）。清理本身若失敗（pane 殺不掉、或 `agents/` 目錄
權限被外部改動使 registry 刪不掉），**會在 stderr 明確警告而非靜默**。殘留
的 registry 是個 ghost worker：佔一個 spawn 名額、擋住同名 spawn、在 `list`
裡照樣列出，`send` 也還會替它建 mailbox task。排除障礙後用 `despawn` 清掉。

## brief 檔檢查與 TOCTOU 殘留缺口

守則檔缺失、不可讀、或不是普通檔案（目錄、FIFO、裝置）時 spawn 直接非零
退出，不建 pane、不寫 registry、不留審計。沒有守則的 worker 收到探針只會
當成對話回覆，開出來也是壞的；而檢查放在建 pane 之前，才不會留下一個佔著
cap 的半殘 worker。

只驗 `-r` 是不夠的——它對目錄一樣成立，而 pane 內 `cat` 讀目錄失敗時命令
替換仍回空字串，runtime 照樣被 exec 起來（此缺陷由獨立複核以 `/tmp` 反例
抓出，已補回歸測例）。

**殘留缺口（已知、刻意不修）**：這道檢查與 pane 內實際讀檔之間有 TOCTOU
空隙，後果有兩種——(a) brief 被刪或換成別的內容：worker 以空的／被替換的
守則啟動，而不是拒絕啟動；(b) 被換成 FIFO 或其他讀不完的來源：pane 的 sh
會卡在讀取、runtime 根本起不來，而 0.3 秒的夭折偵測只看到 sh 還活著，於是
判定 spawn 成功——留下一個佔著 cap、永遠停在 `starting` 的 pane（此後果由
獨立複核以順序化反例補出，(b) 比 (a) 更糟）。要關掉它得把啟動指令改成先
讀檔再 `exec` 的複合形式，那會動到 `spawn_tag` 前綴這條已被反覆錘過的安全
不變量，代價高於收益；且替換 brief 本來就在同 uid 互信域內。清理方式是
照常 `despawn` 那個 `starting` 的 agent。

## brief 路徑的單引號防線

路徑會以單引號字面值送進 pane 的 sh，引號一旦被閉合就能往後接命令。含
單引號一律拒絕（湊跳脫不如不收）。這條有專門的注入測例把關——只斷言
「非零退出」是不夠的，畸形路徑多半會讓 sh 語法錯誤而被夭折偵測擋下，
看起來像 fail-closed 卻什麼也沒鎖住。

## relay --self-exit 為什麼不是自殺

既有 `despawn` 的順序是「kill pane → 確認已死 → 清 registry → 寫審計」，
A 若殺自己的 pane，執行中的 process 會被 SIGHUP 帶走，永遠走不到後兩步——
registry 殘留、審計線斷掉。交給活著的 B 執行，順序不變量原封不動。接力鏈
第一棒的人工閘門是免費的：第一棒是人工開的 session，B 去 despawn 它會被
既有的「非 spawn 出身，despawn 拒絕」擋下；接手者守則明說看到這個訊息是
正常的、不要繞過，也不要改用 tmux 直接殺對方的 pane。交接檔路徑來自命令
列，暴露面比內部常數大一級，因此走與 brief 相同的三道防線（單引號
fail-closed、`-f && -r`、`cat --`），且全部在建 pane 之前。

## evict：三步不包在一把鎖裡

`LOCK_DIR` 是單值，同時持有兩把鎖時 `release_lock` 只放得掉一把。分段的
兩個中斷點失效方向都是「多留一個佔 cap」，而不是「刪掉還沒落地的脈絡」——
後者是分段時唯一不可接受的失效。

## idle_secs 的取值

`idle_secs` 取「最後任務」與「pane 誕生」兩者較晚者：agent 名稱可以重用，
而 tasks/ 是長期累積的，只認名字會讓剛 spawn 的 worker 繼承前一個同名
pane 的活動時間，在報表上看起來閒置好幾小時（真鏈驗收實地踩到：spawn 39
秒顯示 21686）。

## gc 的三道保留線與證據保全

`tasks/` 原本只增不減。這不只是磁碟問題——`idle` 的回收決策直接掃這個目錄
（`last_task_at`），資料越髒決策越不可信，而且那是 O(task 數) 的掃描。
「同名前一個 pane 的任務讓新 worker 看起來閒置六小時」那個 bug 的根因就在
這裡。三道保留線的失效方向一律是「留著」而不是「刪掉」：

1. **未完成的不刪**（queued／delivered／running）。這條有兩層：掃描時擋
   一次，取鎖後真刪之前再驗一次狀態（等鎖那段時間裡它可能被 `receive`／
   `reply` 動過）。
2. **evict 的收尾筆記不刪**（metadata 帶 `pinned: true`），除非明確加
   `--include-notes`。那些筆記是這一層刻意留下來的脈絡；會被 GC 靜靜清掉
   的話，「上下文不會憑空消失」就只是延後兌現。
3. **判不出年紀的不刪**：`created_at` 缺失或無法解析就留著。年齡看
   metadata 的 `created_at` 而非目錄 mtime——mtime 會被備份、rsync、檔案
   系統操作改掉。

外加三條：預設是**試算**，`--apply` 才會刪；只碰 `send` 生成的目錄名形狀
（UTC 時間戳＋4 hex，不是寬鬆的公開 task-id 格式——這是唯一會 `rm` 的
路徑）；以及**「宣告已失效」的證據不刪**。

最後那條不直覺但要緊：`idle` 判斷一個 `disposable` 宣告有沒有被後續任務
推翻，靠的就是掃 `tasks/` 找晚於 `disposable_at` 的任務。那個任務一旦被
清掉，證據跟著消失，宣告會從 `expired` **復活成 `yes`**，orchestrator 據此
直接回收一個其實已有新脈絡的 worker。所以只要 registry 裡還掛著某個宣告，
晚於它的任務就一律保留。

## 審計記號為什麼不擋 despawn

`despawn` 是公開指令，機制上**不阻止**你直接回收一個有殘值的 worker——
擋下來只會逼出一個 `--force`，而習慣性加旗標等於沒有防線。改成讓它在審計
線上看得出來（`despawned-unsaved`）。走 `evict` 的回收不會記 `unsaved`
（收尾流程已經跑過）。`disposable` 同樣是建議不是保護：registry 位在
worker 可寫的目錄，worker 大可自己改；但 `despawn` 認的是 `spawn_tag`
不是這個欄位，所以賴不掉。宣告後又被派新任務時自動轉 `expired`
（`disposable_at` 存在的唯一理由）。

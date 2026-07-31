# CLI 契約（21 個子指令）

## 共通慣例

### CLI-GEN-1 [tested: 2, 3]
exit code：`0`＝成功；凡實作明確偵測的錯誤（用法錯、狀態機拒絕、驗證
失敗）MUST 以 `1` 終止並將 `agent-bridge: <訊息>` 前綴的錯誤寫入 stderr；
`124` 保留給 await 逾時（見 CLI-AWAIT-2），MUST NOT 用於其他語意。
Note: 未被偵測的內部工具失敗（如 list 遇損壞 registry 時的 jq 解析錯）
可能以工具自身的非零碼與裸錯誤訊息傳出——呼叫端只能依賴「非 0＝失敗、
124＝逾時」，不得依賴「失敗必為 1」。

### CLI-GEN-2 [tested: 4]
stdout 保留給資料（task-id、狀態字、訊息內容、清單）；人類可讀的進度／
提示／警告一律走 stderr。呼叫端 MUST 能安全地把 stdout 餵給下一個指令。

### CLI-GEN-3 [tested: 3, 31]
公開參數驗證文法：agent 名 `[A-Za-z0-9_-]+`；task-id `[A-Za-z0-9][A-Za-z0-9._-]*`
（僅格式驗證；gc 實際清理另用更嚴的 send 產出形狀，見 CLI-GC-2）。不合法
輸入 MUST 在任何狀態變更前拒絕。
Source: NAME_RE / TASK_ID_RE / check_task_id

---

## `register`

### CLI-REGISTER-1 [tested: 1]
`register <agent> <tmux-target>`：解析 target 為 pane id 並寫入 registry
（STATE-AGENT-1）。target 解析失敗 MUST 錯誤終止。同名既存 agent：spawn
出身或出身不明 MUST 拒絕覆寫（STATE-AGENT-2/3）；人工註冊允許覆寫。

## `unregister`

### CLI-UNREGISTER-1 [tested: 9]
`unregister <agent>`：移除人工註冊。spawn 出身 MUST 拒絕（要求 despawn）；
出身不明 MUST 拒絕並要求人工處理；未註冊 MUST 錯誤終止。
Source: cmd_unregister

## `list`

### CLI-LIST-1 [tested: 1]
`list`：stdout 每 agent 一行、TAB 分隔三欄 `<name>\t<pane_id>\t<ready>`；
ready 欄：人工註冊 `-`、spawned 未就緒 `starting`、已就緒 `ready`。
無 agent 時輸出為空、exit 0。裸 `list` 的輸出是既有腳本的介面，
MUST NOT 因 `--long` 的加入而改變一個位元組。
Source: cmd_list

### CLI-LIST-2 [tested: 38]
`list --long`（`-l` 為等價別名）：人可讀的介入視圖。首行 MUST 為欄名標頭
`NAME\tPANE\tREADY\tORIGIN\tWHERE\tOWNER\tDISPOSABLE\tIDLE`，其後每 agent
一行、TAB 分隔恰八欄。欄值 MUST NOT 含 TAB／換行／控制字元（`name`、
session 名等來自 registry 與 tmux，是可被塞入怪字元的外部資料；一個 TAB 就
能把一列變成九欄）——輸出前 MUST 逐欄代換。

各欄允許值域（可機器判定）：

- `ORIGIN` ∈ `spawned|manual|?`——**provenance 與 liveness 分開呈現**，人工
  註冊者 despawn 恆被拒（CLI-DESPAWN-*），這一欄是那條規則的可見形。
- `READY` ∈ `ready|starting|-|?`；`DISPOSABLE` ∈ `yes|expired|-|?`；
  `IDLE` ∈ 非負整數｜`-`。語意同 `idle` 指令。
- `WHERE`／`OWNER`：以 tmux 即時反查，顯示人看得懂的 `<session>:<window>`；
  registry 存的 `%id`／`@id` 是判定的根（不可變），名稱與 index 只是當下的
  人類標籤，MUST NOT 反過來當判定依據。失效 MUST 顯式降級為固定字面值、
  MUST NOT 沿用看似仍在的舊值：查不到 `dead`／`owner-dead`、tmux 沒得查
  `?`、registry 的 id 形狀不合法 `invalid`、manual 無 owner 概念 `-`。
  三者是不同的事：「查不到」是東西沒了、「沒得查」是無從得知、「不合法」
  是資料損壞。
- **一個 id 可以對到多個位置**：tmux 的 window 可同時 linked 到多個 session
  （`man tmux`「Windows may be linked to multiple sessions」），該 window 與
  其 panes 因此在 `-a` 列表出現多次。MUST NOT 任選一筆（那是隨列序給答案）：
  能以 registry 記的 session 標籤配出唯一一筆時取之，否則 MUST 全部列出、
  以 `,` 分隔並排序去重，讓歧義顯形。
- **唯讀**：MUST NOT 取鎖、MUST NOT 寫任何檔案、MUST NOT 因為判定 `dead`
  就順手清 registry（回收一律是 despawn／evict 的顯式動作）。因為不取鎖，
  tmux 索引、idle 訊號與 registry 是分次讀取的 best-effort snapshot，
  本指令**不承諾**列內或列間的原子一致。
- registry 損壞 MUST NOT 讓整份報表終止：該列以 `?` 呈現後繼續（它照樣佔
  著 cap，必須看得見）。

設計原則（非規範，不可機器判定，故不寫成 MUST）：本視圖給的是**訊號不是
結論**——`IDLE` 只說多久沒動、`DISPOSABLE` 只是 worker 自己留下的建議，
兩者都不等於「可以安全刪除」。是否回收由人判斷；上面的值域限制是這條原則
可機器守住的部分。
Source: cmd_list

## `send`

### CLI-SEND-1 [tested: 4, 6, 14]
`send <agent> --from <sender> (--message <text> | --message-file <path>)`：
建立任務並通知收件者。`--from` 必填；`--message` 與 `--message-file` 恰取
其一；`--message-file -` 讀 stdin。file／stdin 模式的內容 MUST byte-for-byte
保真寫入 request；text 模式尾端補一個換行。成功時 stdout 恰為 task-id 一行。
Source: cmd_send / write_message

### CLI-SEND-2 [tested: 2]
收件者 MUST 已註冊，否則錯誤終止且不留任何 task 痕跡。訊息來源檔 MUST 在
建立 task 目錄前預檢存在。收件者為未就緒 spawned worker 時 MUST 仍收件
（入 mailbox）並向 stderr 警告通知可能延後。
Source: cmd_send

### CLI-SEND-3 [tested: 2, 8, 31]
task 建立中途任何失敗（來源檔消失、寫入失敗）MUST 回滾清除殘缺目錄後以
錯誤終止（STATE-TASK-5）。成功建立後通知失敗 MUST NOT 影響任務本身
（任務已入 mailbox，不遺失）。
Source: cmd_send / send_rollback / notify_or_defer

## `receive`

### CLI-RECEIVE-1 [tested: 3, 4, 14]
`receive <task-id>`：queued MUST 轉 delivered；delivered/running MUST 冪等
重取（記 re-receive 事件、不改狀態）；終態 MUST 拒絕。stdout 恰為
request 內容；task-id／from／working_directory 標頭走 stderr。
Source: cmd_receive

## `start`

### CLI-START-1 [tested: 10]
`start <task-id>`：僅 delivered 可轉 running；其他狀態 MUST 拒絕
（queued 須先 receive）。
Source: cmd_start

## `reply`

### CLI-REPLY-1 [tested: 5, 7, 14]
`reply <task-id> (--message|--message-file)`：僅 delivered/running 可轉
completed；寫入 response 後通知 sender `read <task-id>`。終態 MUST 拒絕
再 reply（response 不可被覆寫——依 STATE-TASK-4 的寫序保證）。sender 已
不在 registry 時 MUST 靜默略過通知、回覆本身仍成功。
Source: cmd_reply / respond_task

## `fail`

### CLI-FAIL-1 [tested: 11]
`fail <task-id> (--message|--message-file)`：同 CLI-REPLY-1 但終態為
failed；失敗原因即 response 內容，`read` 可讀。
Source: cmd_fail / respond_task

## `cancel`

### CLI-CANCEL-1 [tested: 12]
`cancel <task-id>`：queued/delivered/running MUST 可取消；終態 MUST 拒絕。
取消後通知 worker `status <task-id>`；通知失敗 MUST NOT 影響取消結果。
Source: cmd_cancel

## `status`

### CLI-STATUS-1 [tested: 3, 31]
`status <task-id>`：stdout 恰為裸狀態字一行（權威來源，STATE-TASK-4）。
唯讀、不取鎖、不寫事件（只讀 sandbox 可用）。status 檔缺失／不可讀 MUST
以錯誤終止，MUST NOT 以 exit 0＋空輸出蒙混。
Source: cmd_status

## `read`

### CLI-READ-1 [tested: 7, 31]
`read <task-id>`：僅 completed/failed 可讀；cancelled 與未回覆狀態 MUST
拒絕（訊息區分兩者）。stdout 恰為 response 內容；task-id／from／to 標頭走
stderr；記 read 事件（非唯讀路徑）。讀取全程持 task 鎖（與 gc --apply 的
刪除互斥，不得讀到被抽走一半的目錄）。
鎖／狀態驗證／read 事件／payload 讀取的實作已下沉 `ab_core::task::read_response`
（TUI 的 `r` 消費同一份，CLI-UI-1），**行為不變**：stdout／stderr 逐字輸出、
退出碼與拒絕訊息一如既往；CLI 側只剩標頭呈現。
Source: cmd_read / ab_core::task::read_response

## `await`

### CLI-AWAIT-1 [tested: 13]
`await <task-id> [--timeout <secs>]`：輪詢至終態後 stdout 印終態字、exit 0。
唯讀、不取鎖、不寫事件（只讀 sandbox 可用）。`--timeout` 非負整數（至多
9 位），`0`（預設）＝不逾時。
Source: cmd_await

### CLI-AWAIT-2 [tested: 13, 31]
逾時 MUST exit `124` 且僅逾時得用 124；await 自身的操作性失敗（status 檔
被移走、sleep 失敗）MUST 以 exit 1 立即終止，MUST NOT 被誤分類為逾時或
繼續輪詢。不可放寬（呼叫端以 124 觸發回收決策，誤報會殺活 worker）。
Source: cmd_await

---

## `spawn`

### CLI-SPAWN-1 [tested: 16, 37]
`spawn <name> --runtime <codex|claude|agy> [--model <model>] [--window]`：
建立 worker pane 並註冊。model 文法 `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`、
不支援的 runtime、同名已註冊（任一出身）——MUST 全部在建立 pane 之前拒絕。
成功時 stdout 恰為 pane id 一行。

Note（`agy` 的降級，量測正本 `docs/agy-probe.md`）：現行 agy（實測 1.1.9）
沒有可供本專案掛載的 hooks 介面，故 agy worker **不會**有
`state/<name>.json`，通知端恆走 legacy 送鍵
（HOOK-NOTIFY-1 的「未知」分支）。這是接受的 degradation 而非缺陷：
send/receive/reply 的任務狀態機不倚賴 state 通道。bash 正本自 M4 凍結，
`agy` MUST 只存在於 Rust 實作。
Source: cmd_spawn

### CLI-SPAWN-2 [tested: 16, 19, 20b]
所有「建 pane 前預檢」（brief 檔、claude hooks settings、ready 參數、model）
失敗 MUST NOT 留下任何 pane 或 registry 痕跡；建 pane 後、註冊完成前的任何
失敗（含啟動即夭折）MUST 回滾（kill pane＋刪 registry）後以錯誤終止。
註冊完成後 spawn 即視為成功：其後的就緒等待逾時、輸出寫入失敗 MUST 只警告、
MUST NOT 使 exit code 非零（否則呼叫端誤判失敗重試，留下佔 cap 的活 worker）。
不可放寬。
Source: cmd_spawn / spawn_rollback / spawn_wait_ready

### CLI-SPAWN-3 [tested: 16, 20]
spawn 上限（ENV-SPAWN-1）：計數 MUST 包含「spawn 出身」與「出身無法判定」
的 registry（漏算損壞檔會讓 cap 形同虛設）；cap 檢查、建 pane、註冊 MUST
在同一把 registry 鎖內（並行 spawn 不得繞過 cap）。
Source: cmd_spawn

### CLI-SPAWN-4 [tested: 16]
worker pane 的啟動命令 MUST 以 `AGENT_BRIDGE_SPAWN_TAG=<tag>` 為第一個
token（tag 文法見 ENV-TAG-1，含 48 位熵與 agent 名）；tag 同時寫入
registry 的 `spawn_tag` 欄。它是 despawn／回滾辨識 pane 出身的唯一證據。
呼叫者環境的標準 proxy 變數 MUST 穿透給 worker；`AGENT_BRIDGE_PASS_ENV`
指名的變數 MUST 只在「已設」時穿透（不塞空值）。
Source: cmd_spawn

### CLI-SPAWN-5 [tested: 32]
落點：tmux 內呼叫時 worker MUST 落在呼叫者（owner）的 worker window——同
owner 既有 worker window 存活且其 tmux 視窗選項 `@ab_owner` 等於本次解析的
owner 才得沿用（registry 內容不足以授權沿用，防 confused-deputy）；否則
新建緊鄰 owner 視窗之後。`--window` MUST 開專屬視窗且不被後續 spawn 共用。
tmux 外呼叫落當前視窗。新建 worker window 的 `@ab_owner` 印記寫入失敗
MUST 回滾 spawn。
Source: cmd_spawn / caller_owner

### CLI-SPAWN-6 [tested: 32]
spawn／despawn／evict／disposable MUST 追加審計行至 `<data>/agents.log`，
固定 6 欄空白分隔：`<ts> <action> <name> <pane> <runtime> <actor>`；欄值
MUST 摺疊空白、空值以 `-` 補（欄位安全在寫入點保證，不倚賴上游）。actor
是 provenance 非認證。
Source: log_agent_event

### CLI-SPAWN-7 [tested: 22, 27]
worker／接手者 brief 正本（share/worker-brief.md、share/successor-brief.md）
的策略不變量是契約的一部分：worker 守則 MUST 教導「等 receive、單行
agent-bridge 文字是命令不是聊天、receive→work→reply/fail」；接手者守則
MUST 教導「讀交接檔即開工、不等派工」。brief 內容變更視為契約變更。
Source: share/worker-brief.md / share/successor-brief.md（引用不搬家）

## `relay`

### CLI-RELAY-1 [tested: 23]
`relay <name> --runtime <r> [--model <m>] --handoff <path> [--window]
[--no-select] [--self-exit <my-name>]`：以接手者 brief（ENV-BRIEF-2）spawn
一個接棒 session。`--handoff` 必填，MUST 在建 pane 前驗證為可讀普通檔案且
路徑不含單引號。除 brief 與焦點切換外，cap／tag／回滾／夭折偵測／registry
不變量 MUST 與 spawn 完全一致（同一實作路徑，不得複製第二份）。
Source: cmd_relay

### CLI-RELAY-2 [tested: 23]
接力鏈深度：本棒深度取 `AGENT_BRIDGE_RELAY_DEPTH`（ENV-DEPTH-1），達上限
（ENV-DEPTH-2）MUST 在建 pane 前拒絕；接手者環境的深度 MUST 為本棒＋1。
非 relay 的 spawn MUST NOT 下傳深度變數。
Source: cmd_relay / cmd_spawn

### CLI-RELAY-3 [tested: 23]
`--self-exit <my-name>` MUST 以「接手者 ready 後執行 `despawn <my-name>`」
寫入接手者 prompt，MUST NOT 由前一棒自殺（自殺會斷 despawn 的
kill→確認→清 registry→審計順序）。預設把 tmux 焦點切至新 pane；
`--no-select` 不切；焦點切換失敗 MUST NOT 影響 relay 成功。
Source: cmd_relay / relay_prompt_arg

## `despawn`

### CLI-DESPAWN-1 [tested: 18]
`despawn <name>`：僅 spawn 出身可回收；人工註冊 MUST 拒絕（指向
unregister）；出身無法判定 MUST 拒絕並要求人工處理。出身檢查、tag 比對、
kill MUST 在同一把 registry 鎖內（防 TOCTOU 換代）。
Source: cmd_despawn

### CLI-DESPAWN-2 [tested: 18b, 19]
kill 前 MUST 逐級驗證：registry `pane_id` 符合 pane 文法（防命令注入）；
`spawn_tag` 符合含本 agent 名的 tag 文法（防萬用鑰匙）；目標 pane 的啟動
指令以該 tag 起首。三者任一不符時 MUST NOT 殺 pane：id 已被他人佔用或
registry 遭竄改 → 清 registry、stderr 警告、exit 0（stale）。kill 與最終
驗證 MUST 在同一次 tmux 呼叫內完成（防 server 換代誤殺）；kill 後 MUST
確認 pane 已消失，否則保留 registry 並以錯誤終止。不可放寬。
Source: cmd_despawn

### CLI-DESPAWN-3 [tested: 18c, 31]
tmux 查詢失敗（server 不可達）與「pane 不存在」MUST 區分：前者保留
registry 並錯誤終止；後者清 registry 成功收場（absent）。審計 MUST 在動手
前預檢可寫（不可寫則拒絕動手）；不可逆動作完成後審計落地失敗 MUST 只警告、
不得以非零收場。回收無有效 disposable 宣告且未經 evict 收尾流程的 worker
時，審計事件 MUST 記為 `despawned-unsaved`（機制不擋、審計留痕）。
Source: cmd_despawn

## `ready`

### CLI-READY-1 [tested: 17]
`ready <name>`：worker 自報就緒，registry `ready` 置 true。僅 spawn 出身
可用；人工註冊／出身不明 MUST 拒絕。冪等（重複 ready 無害）。
Source: cmd_ready

---

## `disposable`

### CLI-DISPOSABLE-1 [tested: 24]
`disposable <name>`：worker 單向宣告「脈絡無殘值」，registry 寫
`disposable: true` 與 `disposable_at` 時間戳。僅 spawn 出身可用。語意是
給 orchestrator 的建議、不是保護；**預設保留**——未宣告的 worker 一律視為
有殘值。宣告後若又被派新任務，宣告失效（由讀取端判定，見 CLI-IDLE-1，
不清除欄位）。審計記 disposable 事件。
Source: cmd_disposable / disposable_effective

## `idle`

### CLI-IDLE-1 [tested: 25]
`idle`（無參數）：worker 池回收決策視圖。唯讀（不取鎖、不寫檔、不建目錄，
只讀 sandbox 可用）。stdout 每 agent 一行 TAB 分隔四欄
`<name>\t<ready>\t<disposable>\t<idle_secs>`。disposable 欄三值即決策：
`yes`＝有效宣告可回收；`expired`＝宣告後又有較晚任務（失效）；`-`＝未宣告
（預設保留）或人工註冊。
Source: cmd_idle

### CLI-IDLE-2 [tested: 25]
idle_secs MUST 取「最後派工時間」與「本 pane 誕生時間」較晚者起算（agent
名可重用，不得繼承同名前代的任務時間而誤判久閒）；時間無法解析 MUST 印
`-` 而非 0（0 會被誤讀成「剛用過」）。損壞 registry MUST 以
`<name>\t?\t?\t-` 一行呈現，MUST NOT 靜默跳過（它仍佔 cap）。
Source: cmd_idle / last_task_at

## `gc`

### CLI-GC-1 [tested: 28]
`gc [--older-than <days>] [--apply] [--include-notes]`：清理夠舊的終態
task 目錄。預設 dry-run（stdout 列 `<id>\t<status>\t<created_at>`、不刪）；
`--apply` 才刪。天數預設 14、0–99999 整數。年齡 MUST 以 metadata
`created_at` 判定（目錄 mtime 不可信）；讀不出年齡 MUST 視為年輕保留。
Source: cmd_gc

### CLI-GC-2 [tested: 28]
刪除門檻文法 MUST 是 send 產出形狀 `^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{4}$`
（嚴於公開的 TASK_ID_RE）；非此形狀或殘缺（缺 metadata/status）的目錄
MUST 不碰。不可放寬（寬鬆文法會把任何人放進 tasks/ 的目錄納入 rm 範圍）。
Source: cmd_gc

### CLI-GC-3 [tested: 28, 29]
三道保留線，失效方向一律「留著」：(1) 非終態一律不動；(2) `pinned` 收尾
筆記一律不動，除非 `--include-notes`；(3) 現存 disposable 宣告的「失效
證據」任務（created_at ≥ 該 agent 的 disposable_at）一律不動——刪掉它會讓
expired 宣告復活成 yes。實刪 MUST 持該 task 鎖並在鎖內重驗終態；取不到鎖
記失敗計數、不中止整輪。結尾 MUST 對 stderr 匯報各保留線計數。
Source: cmd_gc

## `evict`

### CLI-EVICT-1 [tested: 26]
`evict <name> [--timeout <secs>] [--from <sender>]`：三段式驅逐
send（收尾任務，pinned）→ await → despawn。timeout 預設 300。僅 spawn
出身；registry 無 `spawn_tag` MUST 拒絕（無法鎖定世代）。收尾任務文案是
機制的一部分（要求 worker 落地 context 內殘值），MUST 內建於實作、不得
外部檔案化。成功時 stdout 恰為收尾 task-id 一行。
Source: cmd_evict

### CLI-EVICT-2 [tested: 26, 31]
await 結果分流 MUST 為：completed → `evicted`；failed/cancelled →
`evicted-unfinished`；逾時（124）→ `evicted-timeout`，**逾時仍 despawn**
（不回話的 worker 不得永久卡死 cap）；await 其他非零（操作性失敗）MUST
中止並保留 pane（不得把活 worker 當逾時殺）。審計事件名 MUST 如上分流，
不得合併。不可放寬。
Source: cmd_evict

### CLI-EVICT-3 [tested: 26, 29]
despawn 段 MUST 綁定 evict 起始時記下的 spawn_tag 世代：期間同名 agent
換代 MUST 拒絕回收（失效方向＝多佔一個 cap，不誤殺新 worker）。despawn
結果為 stale 時 MUST NOT 記任何 evicted* 審計（該 pane 未被回收）。三段
MUST NOT 包在同一把鎖內；各段之間中斷的失效方向 MUST 不刪未落地脈絡。
Source: cmd_evict / cmd_despawn

## `hook`

### CLI-HOOK-1 [tested: 33]
`hook <stop|prompt-submit|notification>`：agent runtime hook 事件的落地端，
stdin 收事件 JSON。完整協定見 hooks.md；此處只定 CLI 面：未知事件靜默
exit 0；**任何**失敗（非 JSON stdin、依賴缺失、寫入失敗）MUST exit 0
（exit 2 對呼叫端是 block 訊號，hook 故障不得干擾 worker）；僅 stop 分支
需要 block 時得有 stdout。不可放寬。
Source: cmd_hook

## `ui`

### CLI-UI-1 [tested: 40, 41]
`ui`：alternate-screen 全螢幕 TUI dashboard（設計正本 `docs/tui-design.md`，
本條款對應其第一縱切 §8）。MUST 以 alternate screen 進出：退出後（正常
`q` 退出**與** panic／錯誤退出皆然，後者由 panic hook 保證）MUST 還原
terminal（raw mode 關閉、離開 alternate screen），且 tmux 的 pane id／
window id／layout 與 geometry 不變。`Enter` focus 語意：目標 pane 在
current session → select-window＋select-pane；在其他 session → 先
switch-client；linked window 多位置 → 優先 current session，否則取第一個
live location，不彈詢問框。`x` 僅對 task 列合法（worker 列 MUST 提示無
效），單一確認框 MUST 逐字顯示等價 CLI 原文 `agent-bridge cancel
<task-id>`，確認後執行與 `cancel` 相同的轉換與通知（CLI-CANCEL-1）。
read model 為磁碟輪詢（500ms）＋tmux 節流查詢（2s）；每條 tmux 查詢
MUST bounded（ENV-TMUX-1）**且 MUST NOT 在 UI thread 上執行**（背景 worker
＋channel），逾時該欄降級 unknown、動作顯示進行中，MUST NOT 凍結 UI——
`AGENT_BRIDGE_TMUX_TIMEOUT=0`（不設限）時亦然。首頁 selection MUST 落在
current owner（呼叫者所在 `session:@window`；對不上時以 current pane 反查
registry），而非字典序第一筆。task status MUST 顯示權威狀態字（不存在
`blocked`）。
啟動器協定（popup）：程式 MUST NOT 自行偵測 popup；由 binding 設定
`AGENT_BRIDGE_UI_POPUP=1`（ENV-UI-1）宣告，此模式下 `Enter` focus 成功後
MUST 直接正常退出（`display-popup -E` 的行程結束即關 popup，人落在目標
pane）；未設定時 focus 後繼續執行。
版面（設計 §2 P2）：四面板 `OWNERS｜WORKERS／TASKS｜DETAIL`。TASKS 欄是
當前選中 owner 底下所有 worker 的任務平坦列表（**含終態**，id 反序）；
DETAIL 欄 MUST NOT 可聚焦，永遠顯示當前聚焦面板選中項的唯讀細節。`Tab`
MUST 在 `OWNERS → WORKERS → TASKS → OWNERS` 三欄間循環。`x` 的合法目標
擴為 WORKERS 欄的 task 列與 TASKS 欄的**非終態**列；TASKS 欄的終態列
MUST 提示無效且 MUST NOT 開確認框。
唯讀鍵（三者皆 MUST NOT 改變任何 task／registry 狀態，`read` 事件除外）：
`r` 讀選中 task 全文，MUST 走與 `read` 相同的實作（同一把 task 鎖、同一組
拒絕訊息、同樣記 read 事件，CLI-READ-1），且 MUST 在背景 worker 上執行
（取鎖會 block）；成功以全螢幕 overlay 呈現，失敗訊息進 footer。
`i` 顯示選中 worker 的摘要頁（name／pane／runtime／owner／ready／
spawn_tag／registered_at／liveness／in-flight 一覽），資料來源限於 registry
與已載入的 read model／liveness 快照，MUST NOT 為此新增 tmux 查詢；
liveness MUST 維持三態（`unknown` MUST NOT 顯示為 dead）。
`c` 複製**證據**到 tmux buffer（`set-buffer`，可由 `show-buffer` 讀回），
內容限於 `task-id:`／`pane:`／`agent-bridge read <id>`／
`agent-bridge status <id>`（worker 列則為 `pane:`＋`agent-bridge list --long`），
**MUST NOT 含任何 mutation 子指令原文**（cancel／evict／despawn／send／
spawn／relay／unregister／register／kill／gc）；tmux 不可用時 MUST 以訊息
降級，MUST NOT 凍結 UI。
Source: cmd_ui / ab_tui::run

---

## 唯讀指令的 sandbox 契約

### CLI-RO-1 [tested: 31]
`status`／`await`／`idle`／`list` MUST 全程不寫檔、不建目錄；`status` 與
`await` 另 MUST 不依賴 jq。`hook` 自理依賴檢查與目錄建立且失敗全吞
（CLI-HOOK-1）。其餘指令啟動時自動補建資料目錄。
Source: main（require_jq / ensure_dirs 豁免表）

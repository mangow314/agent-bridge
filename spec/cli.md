# CLI 契約（21 個子指令）

## 共通慣例

### CLI-GEN-1 [tested: 2, 3]
exit code：`0`＝成功；凡實作明確偵測的錯誤（用法錯、狀態機拒絕、驗證
失敗）MUST 以 `1` 終止並將 `agent-bridge: <訊息>` 前綴的錯誤寫入 stderr；
`124` 保留給 await 逾時（見 CLI-AWAIT-2）、`125` 保留給 await 的 blocker
提前返回（見 CLI-AWAIT-4），MUST NOT 用於其他語意。
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

### CLI-LIST-2 [tested: 38, 44]
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

### CLI-AWAIT-3 [tested: 46]
blocker 探測 MUST 唯讀：只做 tmux 查詢（mode／capture／exists），MUST NOT
送鍵、寫檔、寫事件——await 的「只讀 sandbox 可用」性質（CLI-AWAIT-1）不因
探測而改變。探測重用送鍵防線同一套 matcher（`screen_has_prompt`），MUST NOT
另養一套。pane 解析不到（task 無 `to`、agent 未註冊、registry 無 pane）MUST
降級為純輪詢並 stderr 提示一次，MUST NOT 因此失敗。
Source: cmd_await / notify::probe_blocker

### CLI-AWAIT-4 [tested: 46]
`--on-blocker warn|return|off`（預設 `warn`）＋`--blocker-grace <secs>`
（預設 60）。Prompt MUST 連續兩次探測確立才行動（單次不動作）；確立喚
stderr 警告一次。`return` 下確立起算持續滿 grace MUST 以 **exit 125**
提前返回；`125` 僅限此語意，MUST NOT 挪用；逾時語意不變（Blocked 不得
吃掉 124）。CopyMode／Gone 各至多警告一次、MUST NOT 提前返回；查詢層失敗
（Unknown）MUST NOT 觸發警告、MUST NOT 歸零去抖與持續計時（查不到不是
「已解除」的證據）。stdout 終態契約不變：僅 exit 0 印終態字。
預設是 warn 而非 return：matcher 有誤判史，Return 誤報會讓協調者把工作中
的 worker 當卡死處置；無人值守派工的 await 應顯式帶 `--on-blocker return`
（orchestrator-brief 派工紀律）。evict 的內部 await MUST NOT 帶探測
（回收決策只認終態與真逾時）。
Source: cmd_await / task::await_task_watched

---

## `spawn`

### CLI-SPAWN-1 [tested: 16, 37]
`spawn <name> --runtime <codex|claude|agy> [--model <model>] [--here|--window]`：
建立 worker pane 並註冊。model 文法 `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`、
不支援的 runtime、同名已註冊（任一出身）、`--here` 與 `--window` 同時給
——MUST 全部在建立 pane 之前拒絕。落點細節見 CLI-SPAWN-5。成功時 stdout
恰為 pane id 一行。

Note（`agy` 的降級，量測正本 `docs/agy-probe.md`）：現行 agy（實測 1.1.9）
沒有可供本專案掛載的 hooks 介面，故 agy worker **不會**有
`state/<name>.json`，通知端恆走 legacy 送鍵
（HOOK-NOTIFY-1 的「未知」分支）。這是接受的 degradation 而非缺陷：
send/receive/reply 的任務狀態機不倚賴 state 通道。退役前的 bash 正本凍結於
M4、不含 agy；正本現已自樹移除。
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
指名的變數 MUST 只在「已設」時穿透（不塞空值），且 **reserved 變數
（`AGENT_BRIDGE_SPAWN_TAG`／`AGENT_BRIDGE_RELAY_DEPTH`）MUST 剔除**——它們
的 assignment 會排在 spawn 自己那一個之後而覆蓋它（詳 ENV-PASS-1）。
Source: cmd_spawn

### CLI-SPAWN-5 [tested: 32]
落點：`spawn` 接受互斥的 `[--here|--window]`；兩者同時給出，或 here layout
壞值（ENV-HERE-1），MUST 在建 pane 前拒絕。`--window` MUST 開專屬視窗且
不被後續 spawn 共用。`--here`（有可解析 tmux 呼叫者時）與 auto 規則判定
「呼叫者是人工 session」（`AGENT_BRIDGE_SPAWN_TAG` 未設定或空字串，三態
語意見 ENV-TAG-1，與 CLI-RELAY-4 盯守提醒共用同一判準）落同一解：切進呼叫者（owner）當前 window，split 後依
`AGENT_BRIDGE_HERE_LAYOUT` 重排（`none`＝只 split 不重排）；此落點 MUST NOT
查或寫 worker-window reuse 的信任根（`@ab_owner`），registry 的
`worker_window` 欄 MUST 留空。auto 規則判定「呼叫者是 spawn 出身」時維持
既有 per-owner worker window 規則：同 owner 既有 worker window 存活且其
tmux 視窗選項 `@ab_owner` 等於本次解析的 owner 才得沿用（registry 內容
不足以授權沿用，防 confused-deputy）；否則新建緊鄰 owner 視窗之後。
tmux 外呼叫（含顯式 `--here` 但無可解析呼叫者）落當前視窗——未指定 target
的 current-window split fallback，不視為錯誤。新建 worker window 的
`@ab_owner` 印記寫入失敗 MUST 回滾 spawn。
Note：`AGENT_BRIDGE_SPAWN_TAG` 在此只是落點 provenance 的訊號、不是身分
授權——誤判的後果止於版面 UX，人可用 `--here`／`--window` 顯式修正，不為
此新增信任根。
Source: cmd_spawn / caller_owner / config::here_layout

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

### CLI-RELAY-1 [tested: 23, 32]
`relay <name> --runtime <r> [--model <m>] --handoff <path> [--here|--window]
[--no-select] [--self-exit <my-name>]`：以接手者 brief（ENV-BRIEF-2）spawn
一個接棒 session。`--handoff` 必填，MUST 在建 pane 前驗證為可讀普通檔案且
路徑不含單引號。除 brief 與焦點切換外，cap／tag／回滾／夭折偵測／registry／
落點解析（CLI-SPAWN-5，`--here` 同適用）不變量 MUST 與 spawn 完全一致
（同一實作路徑，不得複製第二份）。
Source: cmd_relay

### CLI-RELAY-2 [tested: 23]
接力鏈深度：本棒深度取 `AGENT_BRIDGE_RELAY_DEPTH`（ENV-DEPTH-1），達上限
（ENV-DEPTH-2）MUST 在建 pane 前拒絕；接手者環境的深度 MUST 為本棒＋1。
非 relay 的 spawn MUST NOT 下傳深度變數。
Source: cmd_relay / cmd_spawn

### CLI-RELAY-3 [tested: 23, 32]
`--self-exit <my-name>` MUST 以「接手者 ready 後執行 `despawn <my-name>`」
寫入接手者 prompt，MUST NOT 由前一棒自殺（自殺會斷 despawn 的
kill→確認→清 registry→審計順序）。預設把 tmux 焦點切至新 pane；
`--no-select` 不切；焦點切換失敗 MUST NOT 影響 relay 成功。`--no-select`
的契約是「不改 active pane」，不是「畫面完全不變」——here 落點的 layout
重排（CLI-SPAWN-5）可能改變版面。
Source: cmd_relay / relay_prompt_arg

### CLI-RELAY-4 [tested: 23]
呼叫者環境判為人工 session（`AGENT_BRIDGE_SPAWN_TAG` 未設定或空字串——
三態語意見 ENV-TAG-1，與 CLI-SPAWN-5 auto 規則共用同一判準，手動起的
session 多為接力鏈第一棒）時，relay 成功後 MUST 在 stderr 印**恰一行**
盯守提醒；判為 spawn 出身（該變數有非空值，含非 UTF-8）時 MUST NOT 印。
理由：手動 session 的權限框沒有任何機制偵測得到（spawn 出身的 worker 帶
skip-permission 設定不彈框；偵測＝常駐輪詢，已裁定不做——docs/tui-design.md
§1 known gap），此行是該 gap 的唯一緩解。此行 MUST NOT 改變 relay 的
exit code 與 stdout 契約。
Source: cmd_relay

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
Source: cmd_evict / ab_core::evict

### CLI-EVICT-2 [tested: 26, 31]
await 結果分流 MUST 為：completed → `evicted`；failed/cancelled →
`evicted-unfinished`；逾時（124）→ `evicted-timeout`，**逾時仍 despawn**
（不回話的 worker 不得永久卡死 cap）；await 其他非零（操作性失敗）MUST
中止並保留 pane（不得把活 worker 當逾時殺）。審計事件名 MUST 如上分流，
不得合併。不可放寬。
Source: cmd_evict / ab_core::evict

### CLI-EVICT-3 [tested: 26, 29, 43]
despawn 段 MUST 綁定 evict 起始時記下的 spawn_tag 世代：期間同名 agent
換代 MUST 拒絕回收（失效方向＝多佔一個 cap，不誤殺新 worker）。despawn
結果為 stale 時 MUST NOT 記任何 evicted* 審計（該 pane 未被回收）。三段
MUST NOT 包在同一把鎖內；各段之間中斷的失效方向 MUST 不刪未落地脈絡。
Source: cmd_evict / cmd_despawn

### CLI-EVICT-4 [tested: 42, 43]
`evict` 的 additive 參數 `--expect-pane <pane>`／`--expect-generation <tag>`
（compare-and-act，設計正本 `docs/tui-design.md` §5）。兩者各自獨立可用，
MUST 只驗帶到的那一項；**兩者皆不帶時行為與現行完全相同**。
expect 驗證 MUST 在**任何副作用之前**執行，且與收尾任務的建立在**同一把
registry 鎖內**完成——驗證與建立之間若能換代，收尾任務就會派給新世代並
回收它（CLI-EVICT-3 的綁定只保護「送出後 → despawn」那個窗口，兩個 race
window 各有各的防線）。該鎖 MUST 只在帶了 expect 參數時取用：無條件取鎖
會讓不帶參數的呼叫在 registry 鎖被佔用時等滿重試上限才以鎖錯誤失敗，
既有的錯誤優先序因此改變，與上一句的「完全相同」相矛盾。通知 MUST NOT 圈在該鎖內（送鍵帶延遲，會與
spawn／despawn 互撞）；三段整體仍 MUST NOT 共用一把鎖（CLI-EVICT-3）。
不符 MUST 以含 `selection stale` 的訊息非 0 退出，且 MUST NOT 建立 task、
MUST NOT 通知、MUST NOT kill pane、MUST NOT 更動 registry 或審計。
Source: cmd_evict / ab_core::evict（check_expect）

## `hook`

### CLI-HOOK-1 [tested: 33]
`hook <stop|prompt-submit|notification>`：agent runtime hook 事件的落地端，
stdin 收事件 JSON。完整協定見 hooks.md；此處只定 CLI 面：未知事件靜默
exit 0；**任何**失敗（非 JSON stdin、依賴缺失、寫入失敗）MUST exit 0
（exit 2 對呼叫端是 block 訊號，hook 故障不得干擾 worker）；僅 stop 分支
需要 block 時得有 stdout。不可放寬。
Source: cmd_hook

## `ui`

### CLI-UI-1 [tested: 40, 41, 43, 44]
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
狀態顯示為 **ACTIVITY × BLOCKER 雙軸正交**（設計 §4）：BLOCKER 是獨立的一軸，
MUST NOT 取代 task status、MUST NOT 造出權威字以外的 task 狀態。v1 的 BLOCKER
契約只承諾兩項（皆有現成實作來源，設計 §4／§7 終審收窄）：`prompt`（沿用
`notify::screen_has_prompt` 的硬編碼 matcher）與 `occluded`（**結構性查詢**
`pane_in_mode`，非畫面比對）。三態 MUST 保留：查不到＝`unknown`，
**MUST NOT 顯示成「沒有 blocker」**（顯示紀律 §5）。查詢與 liveness 同一輪
節流、同樣 bounded、同樣 MUST NOT 在 UI thread 上執行。
screen-matcher 來源的 `prompt` MUST 去抖：**連續 2 輪命中才升旗**，未達門檻
顯示為「沒有可見 blocker」而 **MUST NOT** 謊報成 `unknown`（該輪畫面確實讀到
了）；降旗 MUST 即時（一輪未命中即撤）。結構性的 `occluded` **MUST NOT** 去抖
——它不是字串比對，沒有單幀誤判面。去抖狀態只存 TUI 記憶體，MUST NOT 寫 FS。
代價：升旗＝首次命中後再一輪，以 2s 節流計，**自框出現算起最壞未滿 4s**。
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
MUST 在 `OWNERS → WORKERS → TASKS → OWNERS` 三欄間循環，`Shift+Tab`
（`BackTab`）MUST 為其反向且互為逆運算（P4 效率量測驅動的 additive 補入，
設計 §3／§9 P4 量測史）。`x` 的合法目標
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
`e` evict 選中 worker（設計 §3／§5 P3）：合法目標**只有 worker 列**（task 列
MUST 提示無效）。出身非 spawn 出身／registry 無法解析／缺 `spawn_tag` MUST
拒絕，且判定與訊息 MUST 沿用 evict 的 core 判定（TUI MUST NOT 另寫一套規則）。
確認框 MUST 逐字顯示等價 CLI 原文（含 `--expect-pane`／`--expect-generation`），
措辭 MUST 是「派收尾任務後回收」，**MUST NOT 出現任何「安全刪除」語彙**，且
MUST NOT 以顏色／排序暗示可刪度、MUST NOT 因 idle／disposable 預選。
確認（`y`）當下 MUST **重讀 registry** 取當下 pane／世代（不得沿用輪詢快照）；
確認期**任何取不到「與框上同一個 identity」的情況**（值不符、registry 消失／
損壞、出身變成人工註冊、`spawn_tag` 消失）MUST 一律以 `selection stale` 中止
且不建 task、不通知、不 kill、不動 registry／審計——原始理由（「未註冊」
「非 spawn 出身」）在確認期會被讀成「這個 worker 本來就不能 evict」，而事實
是「你看到的那一代已經不在」。初次開框（`e`）則 MUST 保留原始理由。
等價 CLI 原文中的動態值（pane／世代／名稱）MUST 經 shell quoting——registry 是
worker 可寫面，含空白會破壞 argv 等價、含 `;`／`$(…)` 會讓「可貼上重跑」變成
注入面。確認框在窄畫面 MUST 換行而非截斷（截掉尾端的世代＝人失去判斷依據）。相符則以當下值帶 `--expect-*` 走 evict 的**同一份 core
編排**（CLI-EVICT-4／CLI-EVICT-3，鎖語意不變）。
執行段 MUST NOT 跑在常駐 worker thread 上（那條同時負責 liveness 輪詢，
evict 的 await 段預設等 300s）：MUST 另起一次性 thread、結果經同一個 channel
回 UI，UI thread MUST NOT 阻塞；同一個 worker 的 evict 在途時 MUST 以 in-flight
閘擋下重複發動。**每一次發動 MUST 有終局**：thread 起不來、或工人自身 panic
（unwind）都 MUST 轉成 terminal error 訊息回 UI，MUST NOT 讓畫面停在「進行中」
且 in-flight 閘不放開。
警告（core 的 `EvictEvent::Warn`、確認期 stale、非乾淨終局）MUST NOT 只寫進會
被下一則訊息覆寫的單行 footer：MUST 以 sticky 的有界歷史呈現，由人顯式清除
（`Esc`），且溢位 MUST 有可見計數，MUST NOT 靜默丟棄。
Source: cmd_ui / ab_tui::run / ab_core::evict

### CLI-UI-2 [tested: 44, 48]
`ui` 的呈現層補強（設計正本 `docs/tui-design.md` §9 的 P5.3–P5.6 四列）。
本條款只管**顯示**，不新增任何協定、不改任何 payload 位元組。

**摘要與時間欄**：WORKERS／TASKS 兩層列各為兩行，第二行是活動摘要。摘要文字
取自 `tasks/<id>/request.md` 的**首行**與 `tasks/<id>/events.log` 的**尾行**，
兩者皆 MUST 有界讀，且界 MUST 成立於**取得路徑**（先 `take`／先 seek 到尾端，
不是整檔讀進來再截）：首行至多 4096 bytes，尾行至多檔尾 8192 bytes。
兩個檔都在 task 目錄下＝worker 可寫面，檔案大小不是本程式保證得了的事實。
尾行讀取從檔案中段切入時，**首段半條行 MUST 丟棄**（半條事件不得當成一整行
呈現）。讀不到＝空字串 fail-closed，MUST NOT 反覆重試。
兩行列的行高 MUST 有單一事實源，捲動位移、捲軌總量與 PgUp／PgDn 的一頁列數
MUST 全部由它換算——行與列兩種單位只要有一處混用，症狀就是靜默跳列。
摘要在畫面上是**顯示層截斷**（過長加省略號、MUST NOT
換行），payload 本身逐字不變——`read` 的 stdout 與 `c` 的 buffer 內容不因此
改變一個位元組（CLI-READ-1／CLI-UI-1 的 `c` 白名單皆不受影響）。elapsed／
`ago` 由呼叫端注入的時刻算出，render MUST NOT 自行取牆鐘。
task status 一律仍是權威字（不存在 `blocked`）；stat header 的計數取自
**整池**，MUST NOT 隨 filter 或可見列數變動。

**triage 排序**：WORKERS 的**組間**順序 MUST 依組內最大 severity 浮頂，
severity 恰三個排序鍵且不得擴充——blocker 升旗（`Prompt`）／pane 死活為
`Dead`／該 worker 最近一輪任務終態為 `failed`；`Blocked > Dead > Failed >
None`。`Unknown` MUST NOT 浮頂（三態不得壓成兩態）。**組內順序不得重排**，
排序 MUST stable（同 severity 維持 canonical 序），且 MUST 只在資料事件邊緣
重排並緊接 selection 重定位。排序 MUST 是**當下事實的函式**：severity 消失後
組序 MUST 回到 canonical 序，MUST NOT 保留上一輪的排名。stale 降級期間，
排序鍵 MUST 與同一幀畫面採信的兩軸同源——畫面顯示 unknown 時 MUST NOT 繼續
依快取中的舊事實浮頂。閒置時長 MUST NOT 進排序鍵，強調 MUST NOT 以顏色
或排序暗示可刪度（設計 §5）。

**blocker 框內容**：升旗中的 worker 其 DETAIL 得顯示 matcher **命中窗**的
原文尾行（有界：至多 6 行）。內容 MUST 來自 blocker 探測那一輪已取得的
`capture-pane` 輸出，**MUST NOT 新增任何 tmux 查詢**（§4 bounded-read）；
MUST 只存於 TUI 記憶體、**MUST NOT 寫 FS**；顯示層 blocker 不再是 `Prompt`
時 MUST 立即清除——去抖未升旗、降旗、以及**整層降級為 unknown 的那一幀**
（MUST NOT 等到下一則探測回報才清）三者皆然。判定與內容 MUST 同源
（`notify::screen_has_prompt` 與 `notify::prompt_snippet` 走同一份 matcher），
MUST NOT 出現「標了 blocked 卻拿不到框」或反之。

**色盤降級**：色盤 MUST 依 `COLORTERM` 決定——恰 `truecolor`／`24bit` 兩個
字面（允許前後空白）升級為 24-bit，其餘（未設、空值、`256color`、認不得的
字面）MUST 降級回 ANSI 16。降級後的畫面 MUST 與升級前的 ANSI 16 版本逐字
相同；**兩種色盤下的字元層 MUST 完全相同**（顏色只加樣式不動字元，既有
`capture-pane` 字元比對因此不受影響）。色盤 MUST 在進 alternate screen 前
定案，MUST NOT 於同一次執行中途更換。
Source: ab_tui::view / ab_tui::model / ab_tui::theme / ab_core::notify::prompt_snippet

---

## `scan`

### CLI-SCAN-1 [tested: 47]
`scan`：掃一輪 page 層事件並推播（設計正本 `docs/tui-design.md` §1 的 page
層）。**page 層只存在於 Rust 實作**（退役前的 bash 正本凍結於 M4、不含
page 層；正本現已自樹移除）。不吃參數；stdout MUST 只印
一個十進位整數＝這一輪**新**推的事件數（已推過的不計），說明走 stderr。
事件恰兩類，不得擴充：`task-failed`（`fail` 的收斂點同步發）與
`worker-died`（pane 不存在但掛著非終態 task）。
Source: cmd_scan / ab_core::page::scan

### CLI-SCAN-2 [tested: 47]
`worker-died` 的判定 MUST **先取得整份 pane 清單**再比對，MUST NOT 逐 pane
查詢後把「查不到」當成「死了」：tmux 不可用時整輪 MUST 零推播（回空），
不得把整池 worker 判成死。task 是否掛在該 agent 身上，判準與 TUI 的歸屬軸
同一條——收件人名相符**且** task `created_at` 嚴格晚於該 agent 的
`registered_at`（同秒＝不可證＝不掛，同名 respawn 不承接上一代的歷史 task）。
此判準與 TUI 的歸屬軸是**同一份實作**（`ab_core::page::task_belongs_to`；
`ab_tui::model::attached` 轉呼叫），MUST NOT 各寫一份。

**已知盲點 MUST 明列**：`list-panes` 永久非零（最後一個 tmux server／session
已退出）與暫時性查詢失敗（逾時、權限、錯 socket）在目前的資料模型下無法
區分，兩者都壓成「整輪不掃」。前者的事實可能正是「整池 pane 都死了」，那些
事件於是**永遠**發不出去，不是「晚一輪」。要涵蓋這一格需要能證明 server 身分
的 schema，屬另立設計裁定，不在本相位。

**每輪的 tmux 成本 MUST 與「有沒有新事件」成正比**：地點 MUST 只在該 agent
還有未推事件時才解。掃描掛在每個成功的非唯讀子指令後面，若每個死列都先解
一次地點，N 筆殘留死列（沒有 task、只剩終態 task、已推過）會讓每次
`fail`／`reply`／`send` 白付最多 2N 次串行 tmux 查詢。這一篩是鎖外的效能篩，
權威的去重覆核仍在 `emit` 的鎖內那一次（CLI-PAGE-1）。
Source: ab_core::page::scan / task_belongs_to

### CLI-SCAN-3 [tested: 47]
每個**非唯讀且已成功完成**的子指令 MUST 在其完成**之後**順手掃一輪（沒有
daemon，事件只能由呼叫者發現）。時點 MUST 是 dispatch 之後而非之前：擺在
前面會讓參數打錯／未知指令也推播、讓 `ready` 這條熱路徑先跑外部通知才寫
ready（把 spawn 的等待推向逾時）、並讓 `fail`／`cancel` 先推一則本次指令
正要解除的 `worker-died`。

排除項：唯讀指令（MUST NOT 寫任何東西，CLI-RO-1，而掃描寫事件流）、非零
退出的指令、`scan` 自身（否則其計數恆為 0）、`ui`（§4 bounded-read：tmux
掛住時 TUI 仍 MUST 畫得出磁碟 read model，dashboard 的啟動不得取決於 page 層）。
Source: main dispatch

### CLI-PAGE-1 [tested: 47]
去重與落盤：每則事件有一個 event key（`failed:<task-id>`／
`died:<agent>:<spawn-tag>:<task-id>`，後者帶 generation key，故同名 respawn
是新事件、行程重啟不是）。

保證的強度是 **durable record ＋ 至多嘗試推播一次**（at-most-once attempt），
**不是 exactly-once**——正常路徑下同一 key 落盤一次、推播一次，但兩個 crash
window MUST 被明列而非隱瞞：①事件流已 append、seen 未記時行程死亡 → 重啟後
同一 key 會再落一筆；②seen 已記、推播尚未發出時行程死亡 → 該則實際推播 0 次
（事件流裡仍在）。真正的 exactly-once 需要具 ack／idempotency key 的 outbox
協定，而收件端是 `notify-send` 或使用者的任意腳本，該保證在本層無從建立。
seen 先於推播寫入是刻意的：否則壞掉的 notifier 會讓同一則每次呼叫重推。

順序 MUST 是**先落盤後推播**：推播管道全滅時事件 MUST 仍在
`state/page-events.jsonl`，且呼叫端 MUST exit 0——推播是副作用，任何子指令的
退出碼與 stdout 契約 MUST NOT 因它改變。

event key MUST 撐得住 seen 檔的一行一 key 格式：含控制字元（含換行）的 key
MUST 整則拒收（不落盤、不推播）。registry 的 `spawn_tag` 與 agent name 是
worker 寫得到的不可信輸入，含換行的 tag 會讓 seen 比對永遠失準、把去重變成
無限的通知源。
Source: ab_core::page::emit / key_is_frameable

### CLI-PAGE-2 [tested: 47]
推播管道分層：①`AGENT_BRIDGE_NOTIFY_CMD` 有設 → 以它為 **argv[0]**、後接
`<title> <body>` 兩個參數（一支可執行檔，不是 shell 字串）；②否則有本機圖形
session（`WAYLAND_DISPLAY`／`DISPLAY`）**且非** SSH（`SSH_CONNECTION` 不在）
→ `notify-send`；③一律再對每個 attached client 送一次 `display-message -c
<client> -l`（`-l` 保證事件文字不被當 tmux format 展開）。SSH 下 MUST NOT
走桌面通知：遠端的 `DISPLAY` 指的是遠端那台的螢幕，彈在沒有人的地方比不
通知更糟。

來源標記由各通道用自己的慣用法承擔，MUST NOT 佔用標題：桌面那一層走
`notify-send -a agent-bridge`（appname 欄），tmux status line 由 pager 自行
接前綴並把內文的換行壓平（status line 是單行）。

自訂命令 MUST 以三個 stdio 全接 null 的方式執行：`Command::status()` 預設
繼承 stdin，而掃描跑在 CLI 路徑上——`send/reply/fail --message-file -` 的
payload 會被自訂 notifier 讀走，指令仍成功但落盤內容已被截短。執行 MUST
有界（內部固定 5 秒，逾時殺掉並視同失敗，**無解除逃生口**）：不退出的自訂
命令會讓一個正在寫盤的原子指令永遠沒有終局。

**SSH 判定是 heuristic，邊界 MUST 明列**：它讀的是「當前 CLI 行程的環境
來源」，不是「此刻正在看畫面的那個 client」。本機建立的長壽 pane 被 SSH
attach 時仍可能走本機 `notify-send`；反向殘留的 `SSH_CONNECTION` 會抑制桌面
層。因 tmux 層一律逐 attached client 送，這是桌面層的誤投／漏投而非完全失聯。
要精準到 viewer 需改成 client-aware 設計，MUST NOT 再堆 `SSH_TTY`／
`SSH_CLIENT` 之類的猜測。
Source: ab_core::page::SystemPager / SubprocessRunner

### CLI-PAGE-3 [tested: 47]
通知文字的內容契約（PG4 human judgment 實測 2026-08-03 定案）。標題 MUST
帶 **agent 名**，且在解得出時 MUST 帶 **地點**（`session:window索引
window名`）；內文首行 MUST 是**決策依據**——`task-failed` 取失敗訊息的第一
個非空行，取不到才退回狀態字；末行 MUST 是 **task id**（`ab read <id>` 的
把手，同一 agent 併發多筆時的辨識軸）。

理由是實測打回來的：受測者答得出「誰、出了什麼事」，卻卡在「要不要切過去
看」——通知既沒說切去哪裡（缺地點），也沒說不管它會怎樣（只有狀態字）。

地點 MUST 先問 pane、pane 問不到再退到 registry `owner` 欄的 window
（`session:@winid`）：`worker-died` 依定義 pane 已死，只問 pane 等於最需要
地點的那一類永遠沒有地點。**tmux 對已死的 target 是 exit 0 ＋ 空展開**
（結果如 `":"`），故解析結果 MUST 驗形（window 索引為數字）才算數，否則
fallback 永遠走不到。兩條都問不到 MUST 只是少一段地點，MUST NOT 讓事件發
不出去。人工註冊的 agent 無 owner 欄（38c），其 pane 一死即無地點可解——
這是既有裁定的後果，不是缺陷。

地點與失敗原因 MUST NOT 進 event key：window 改名、失敗訊息換句話說都不是
新事件。兩者以獨立型別（`PageDetails`）承載，與事件身分分離。

`worker-died` 的內文 MUST 說得出下一步（清理或改派），MUST NOT 只陳述
「沒人會回」；同時 MUST NOT 內嵌可複製的指令——`despawn` 對人工註冊的 agent
會被拒，孤兒 task 的正解也可能是 cancel 或改派（使用者 2026-08-04 裁定）。

不可信文字 MUST 被壓成單行並截斷：失敗原因來自 worker 寫的 response.md、
window 名任人可改，控制字元 MUST 轉空白（否則單行通知被撐開、status line
被洗掉），截斷 MUST 按字元而非 byte。「控制字元」MUST 涵蓋 `is_control()`
以外的 U+2028／U+2029 與 bidi 控制字元（U+202A–U+202E、U+2066–U+2069、
U+200E／U+200F／U+061C）：前者仍會在 GUI 通知裡換行，後者能把地點與 task id
在人眼前重排成別的字。

通知文字 MUST 以 positional 身分抵達 notifier：`notify-send` 的 argv MUST 在
title／body 前加 `--`。agent 名文法允許 `-` 開頭而標題以 agent 名開頭，
GLib 的 option parser 會把 `--help` 這種值重新解析成旗標（實測印 help、
exit 0、一則都沒送），而事件已先記 seen 不會重試。
Source: ab_core::page::{PageDetails, resolve_location, plausible_location,
SystemPager}

---

## 唯讀指令的 sandbox 契約

### CLI-RO-1 [tested: 31, 47]
`status`／`await`／`idle`／`list` MUST 全程不寫檔、不建目錄（含 page 層的
機會式掃描——它會寫事件流，故 MUST NOT 掛在唯讀指令上，見 CLI-SCAN-3）；
`status` 與 `await` 另 MUST 不依賴 jq。`hook` 自理依賴檢查與目錄建立且失敗全吞
（CLI-HOOK-1）。其餘指令啟動時自動補建資料目錄。
Source: main（require_jq / ensure_dirs 豁免表）

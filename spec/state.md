# 磁碟狀態契約

資料目錄（`AGENT_BRIDGE_DATA`，見 env.md）下的四類狀態與其寫入語意。

## 通則

### STATE-GEN-1 [tested: 1]
目錄佈局 MUST 為：`agents/`（registry）、`tasks/`（mailbox）、`state/`
（worker 活動狀態）、`locks/`（鎖）。除唯讀指令（CLI-RO-1 豁免表）外的
指令首次觸碰資料目錄時 MUST 能自行建立缺少的子目錄；唯讀指令 MUST NOT
無中生有（目錄缺失時以空集／找不到收場）；`hook` 只自建 `state/`。
Source: ensure_dirs / main（豁免表）

### STATE-GEN-2 [untested]
所有 JSON／status／訊息檔的寫入 MUST 原子（同檔案系統暫存檔＋rename 語意）：
讀者在任何時點 MUST NOT 觀察到半寫檔案。
Source: atomic_write

### STATE-GEN-3 [tested: 2]
agent 名稱文法 MUST 為 `[A-Za-z0-9_-]+`；不合法名稱在寫入任何狀態前 MUST
以錯誤終止。
Source: NAME_RE（cmd_register / cmd_send 驗證）

## agents/（registry）

### STATE-AGENT-1 [tested: 1, 16]
每個 agent 一份 `agents/<name>.json`。人工註冊（register）欄位：
`{name, pane_id, registered_at}`。spawn 出身另含
`{spawned: true, runtime, model, spawned_at, ready, spawn_tag, owner,
worker_window}`，以及 STATE-AGENT-4 的 `{worker_pid, worker_starttime}`。
Source: cmd_register / cmd_spawn

### STATE-AGENT-2 [tested: 20]
出身判定 MUST 三態：spawn 出身／人工註冊／無法判定（JSON 損壞、非 object、
讀不到）。「無法判定」MUST fail-closed：register 拒絕覆寫、despawn／evict
拒絕動作並要求人工處理，MUST NOT 將損壞 registry 當成人工註冊。不可放寬。
Source: is_spawned

### STATE-AGENT-3 [tested: 1]
register 對既存同名 agent：spawn 出身 MUST 拒絕覆寫（要求先 despawn）；
人工註冊允許覆寫（換 pane）。registry 寫入 MUST 持 registry 鎖，與
spawn/despawn/ready 互斥。
Source: cmd_register

### STATE-AGENT-4 [tested: 35]
spawn 出身另 MUST 記 `{worker_pid, worker_starttime}`：worker runtime 行程的
pid，與該 pid 的行程啟動刻度（Linux `/proc/<pid>/stat` 第 22 欄，用於分辨
pid 重用）。取不到任一項時 MUST 寫入空字串而非讓 spawn 失敗——這兩欄是
HOOK-OWNER-5 的最佳化輸入，缺了只是落回時間窗判別，不影響 spawn 是否成立。
Note: `pane_pid` 之所以就是 runtime 本尊，是因為 tmux 直接 exec 啟動命令、
中間無 shell 層（2026-07-31 實測）。若該前提在某環境不成立，記到的會是中介
行程的 pid，而本尊 hook 的 PPID 會與之不符、反被 HOOK-OWNER-5 擋掉——比不做
更糟。**故 spawn MUST 在寫入前確認該 pid 的 `argv[0]` basename 等於本次
runtime 名**，不符則兩欄留空走落回路徑。只看 `argv[0]` 是刻意的：夾了 shell
時整串 cmdline 找得到 runtime 名（在 `sh -c` 的引數裡），argv[0] 則是 `sh`。
Note: 身分確認 MUST NOT 讀 `/proc/<pid>/environ`（行程完整環境，含憑證，屬
本專案安全邊界的絕對禁區）。spawn tag 存在環境而非 argv，因此確認手段只能
是 argv[0] 這條較弱的線索；不足之處由「不確定就留空、留空就落回」吸收。
Note: 本檔對同互信域的 worker 可寫，這兩欄與 `spawn_tag` 同級——是身分**線索**
不是憑證，防的是巢狀 runtime 的意外冒用而非具寫入權的惡意行為者。
Source: cmd_spawn

## tasks/（mailbox）

### STATE-TASK-1 [tested: 4, 15]
task-id 文法 MUST 為 `<UTC 時間戳 %Y%m%dT%H%M%SZ>-<4hex>`；同秒碰撞 MUST
重試產生新 id（有限次，耗盡則錯誤終止）。id 的字典序即建立時序（秒級）。
Source: cmd_send（task_id 產生）

### STATE-TASK-2 [tested: 4]
每任務一目錄 `tasks/<task-id>/`，完整形狀 MUST 含：`metadata.json`、
`request.md`、`status`；回覆後另有 `response.md`；`events.log` 為
append-only 事件日誌，行格式 `<ISO8601 時間戳> <event> [detail]`。
Source: cmd_send / log_event

### STATE-TASK-3 [tested: 4]
`metadata.json` 欄位：`{version: 1, task_id, from, to, created_at,
updated_at, working_directory, status}`；被 pin 的收尾任務另含
`pinned: true`（gc MUST 跳過，見 cli.md gc 節）。
Source: cmd_send

### STATE-TASK-4 [tested: 5, 7, 31]
狀態機 MUST 為 `queued → delivered → running → completed|failed|cancelled`
（cancel 可自 queued/delivered/running 進入）。**裸字 `status` 檔是操作上的
唯一權威**；`metadata.status` 是展示性副本，允許落後一步。狀態轉換 MUST
先寫 `status` 檔、後寫 metadata——反序會開「終態轉換可重放」窗（對已
completed 的 task 再 reply 覆寫 response）。不可放寬。
Source: update_meta_status

### STATE-TASK-5 [tested: 19, 28, 31]
缺 `metadata.json` 或 `status` 的 task 目錄不屬於任何狀態：send 中途失敗
MUST 回滾清除該目錄；gc MUST 只處理完整形狀的目錄（殘缺目錄不得被 gc
靜默清走，也不得被當成任務）。
Source: send_rollback / cmd_gc

## state/（worker 活動狀態）

### STATE-CHAN-1 [tested: 33]
每個 spawn worker 一份 `state/<name>.json`，欄位：
`{state: "idle"|"busy", ts: <ISO8601>, last_delivered: <task-id|"">,
owner: <session_id>}`。寫入者 MUST 只有該 worker 自己的 hook（單一寫者，
不取 registry 鎖）；通知端只讀不寫。
Source: hook_write_state

### STATE-CHAN-2 [tested: 33, 34.5+]
`ts` 新鮮度語意由 `AGENT_BRIDGE_STATE_TTL` 決定（env.md ENV-TTL-1）：過期、
未來、解析失敗一律視為「未知」。`last_delivered` 是 Stop 迴圈剎車：只有
Stop 分支得更新它；其他事件寫 state 時 MUST 保留原值（沖掉會使防無限迴圈
判斷失準）。`owner` 每次寫入 MUST 取本次事件的 session_id，MUST NOT 從舊檔
沿用（接管當下沿用會抄回舊主）。
Source: hook_write_state / cmd_hook

### STATE-CHAN-3 [tested: 33]
state 通道任何寫入失敗 MUST 靜默吞掉（hook 呼叫端仍 exit 0）：失效方向是
state 停在舊值 → TTL 過期 → 通知端退回 legacy 送鍵，MUST NOT 讓 hook 以
非零收場。不可放寬。
Source: hook_write_state

## locks/

### STATE-LOCK-1 [tested: 8b]
鎖為目錄型（`locks/<id>.lock`），取得即互斥。佔用時 MUST 有限重試後錯誤
終止（不無限等待）；建鎖失敗但鎖目錄不存在（權限／sandbox 問題）MUST 立即
以真實原因終止，MUST NOT 誤報為「鎖佔用中」。
Source: acquire_lock

### STATE-LOCK-2 [tested: 21]
釋放鎖失敗 MUST NOT 靜默：須向 stderr 警告殘留鎖路徑（殘留鎖會擋住後續
同類操作）。
Source: release_lock

# Hook 協定契約

worker runtime 的 hook 事件如何落進 bridge 的 state 通道，以及 Stop block
的派工語意。CLI 面（用法、exit code）見 cli.md CLI-HOOK-1；state 檔格式見
state.md STATE-CHAN-*。

## 事件繫結

### HOOK-BIND-1 [tested: 16]
claude worker 以 `--settings` 注入的 hooks 設定 MUST 恰含三個事件繫結，
命令為**裸命令**（無包裝、無旗標）：`Stop` → `agent-bridge hook stop`、
`UserPromptSubmit` → `agent-bridge hook prompt-submit`、`Notification` →
`agent-bridge hook notification`。設定只含 `hooks` 一個 key（合併進使用者
全域設定，不覆蓋）。不可放寬。
Source: share/claude-worker-hooks.json（正本；引用不搬家）

## 身分與鐵律

### HOOK-ID-1 [tested: 33]
hook 端身分「我是誰」MUST 自 `AGENT_BRIDGE_SPAWN_TAG` 析出（文法
ENV-TAG-1：`ab-spawn-<name>-<pid>-<12hex>`）；無 tag 或格式不符 MUST
靜默 no-op（人工 pane 沒有 state 通道）。
Source: hook_agent_name

### HOOK-ID-2 [tested: 33]
**exit 0 鐵律**：`hook` 的任何失敗路徑（未知事件、無身分、依賴缺失、
stdin 非 JSON、寫入失敗、被 owner gate 擋下）MUST exit 0 且除 stop 的
block 輸出外 MUST 無 stdout。exit 2 對呼叫端是 block 訊號——hook 故障
MUST NOT 卡住 worker。失效方向一律是「state 停舊值 → TTL 過期 → 通知端
退回 legacy 送鍵」的降級鏈。不可放寬。
Source: cmd_hook

### HOOK-ID-3 [tested: 34]
stdin 讀取 MUST 有上限（約 2 秒）：stdin 開著不送資料或 fd 已關 MUST NOT
使 hook 掛住（Stop hook 掛住＝worker 的 turn 永遠結束不了，是鐵律唯一
沒有 TTL 能救的一段）。逾時已讀到的部分交給後續解析的保底分支；環境缺
timeout 工具 MUST 用有上限的替代讀法，MUST NOT 退回無上限讀取。不可放寬。
Source: cmd_hook（stdin 讀取段）

## 所有權閘門（owner gate）

### HOOK-OWNER-1 [tested: 34.5+]
payload 缺 `session_id` 的呼叫 MUST 不寫 state、不發 block（無身分者
不參與 state 通道），靜默 exit 0。閘門 MUST 在 stop 分支查 mailbox／發
block **之前**執行（三事件一體適用）——挪後等於讓巢狀 session 攔走 parent
的任務。不可放寬。
Source: cmd_hook / hook_owner_gate

### HOOK-OWNER-2 [tested: 34.5+]
所有權語意：第一個帶 session_id 寫入 state 的 session 認領所有權（先到
先得；worker 本尊的第一個事件必然最早）。其後 session_id 不符者：state
`ts` 新鮮（0 ≤ now−ts ≤ TTL）MUST 被擋（靜默 exit 0、無 stdout，stop
不發 block）；`ts` 超過 TTL、落在未來、缺欄位或解析失敗 MUST 放行接管
（該 state 已不可信，通知端此刻也已把它當未知——交出所有權零損失；這同時
是 parent /clear 換新 session_id 後的自癒路徑，自癒上限＝TTL）。無 state
檔或無 owner 欄（舊格式）MUST 放行。
Source: hook_owner_gate

### HOOK-OWNER-3 [tested: 34.5+]
gate 使用的 TTL：`AGENT_BRIDGE_STATE_TTL` 壞值**或 0** 一律退預設 1800
續作、MUST NOT die（ENV-TTL-3）。「永遠可接管」等於沒有 gate；「永不
接管」讓 parent /clear 後 block 功能永久死掉——兩個極端都 MUST NOT 出現。
Source: hook_owner_gate

### HOOK-OWNER-4 [tested: 34.5+]
接管成功的寫入 MUST 以本次事件的 session_id 為新 owner，MUST NOT 從舊檔
沿用（STATE-CHAN-2）。
Note: 在**沒有**行程身分可用時（HOOK-OWNER-5 的落回路徑），hook 端「parent
的新 session」與「巢狀 session」在環境與 payload 面不可區分，只能靠時間窗
交還——PPID **祖先鏈**方案在此仍然無效（巢狀的祖先鏈必然包含本尊）。
`receive` 端刻意不加驗：巢狀繼承同 SPAWN_TAG、tag 比對無效；門在 hook 端
不發 block（cross-vendor 複核確認此裁定）。
Source: hook_write_state / cmd_hook

### HOOK-OWNER-5 [tested: 35, 36]
**行程身分優先於時間窗**。registry 同時具備 `worker_pid` 與
`worker_starttime`（STATE-AGENT-4）時，閘門 MUST 先確認「本 hook 是記錄的
worker runtime 自己（含其自有啟動鏈）fork 的」。確認形狀按 registry 的
`runtime` 欄位逐 runtime 白名單，每一形狀都是實測值、不是介面承諾：

- **直接形**（claude 實測；同時是所有 runtime 的基本形）：hook 的直接 PPID
  等於 `worker_pid` 且該 pid 的 starttime 等於 `worker_starttime` → MUST
  放行，且 MUST NOT 再看 `session_id` 或 `ts`。這是 parent `/clear` 換新
  session_id 的即時自癒路徑（行程沒變，身分就沒變），取代 HOOK-OWNER-2 的
  TTL 等待。
- **launcher 形**（`runtime` 為 `codex` 時另 MUST 嘗試；實測見
  `docs/codex-hooks-probe.md` 補測節）：hook 的直接 PPID 不等於
  `worker_pid`，但該直接 PPID 的**父行程**等於 `worker_pid`（starttime 驗法
  同上），**且**該直接 PPID 依 STATE-AGENT-4 的 argv 前兩項規則命中本
  runtime 名——即它是 `worker_pid` 這個 launcher fork 出的同產品原生執行檔
  → MUST 放行。恰好一層、不多不少；中介行程的 cmdline 與 starttime MUST
  依 STATE-AGENT-4 的同快照夾讀規則取得（防走訪期間 pid 重用）。
- 其餘一切（鏈更長、中介不命中本 runtime 形、`runtime` 欄缺或不在形狀表、
  兩欄缺其一、行程已不存在、starttime 不符、或執行環境無此能力）→ MUST
  落回 HOOK-OWNER-2 的 session_id＋TTL 判別，行為與未實作本條時完全相同。

判別 MUST 只在「明確命中白名單形狀」時放行，其餘一律落回；**MUST NOT 把
不符當成「確認為冒名」而擋**。失效方向因此收斂到現狀，最壞等於未實作。
Note: **MUST NOT 一般化成「祖先鏈走得到 `worker_pid` 即確認」**——巢狀
runtime 的祖先鏈必然包含本尊（HOOK-OWNER-4 Note 的否決理由，該否決仍然
有效）。本條與該否決的界線：這裡確認的是 runtime **自有啟動鏈的實測形狀**
（hook 是被 worker 自己的啟動鏈 fork 的），不是「鏈上找不找得到本尊」。
同理 MUST NOT 放寬成「鏈上沒有夾另一個 runtime 形狀的行程即可」或「直接
PPID 豁免檢查」：實測的巢狀鏈（`docs/rust/m5-proposal.md` §1）是
hook → 巢狀 claude → bash → 本尊——巢狀 runtime 正是 hook 的直接 PPID、
其餘中介只有 bash，上述兩種寫法都會把它誤認成本尊。誤認在本條的代價是
**立即奪權**（不經 TTL），是唯一比未實作更糟的失效方向，故形狀表 MUST
逐 runtime 白名單、寧可誤落回。
Note: PPID／鏈形不符不足以推出冒名——合法中介一樣不符：runtime fork 出新的
主行程而舊主仍活著、或使用者經 `AGENT_BRIDGE_CLAUDE_HOOKS`（公開覆蓋面）
指定 hook wrapper。當成冒名的話本尊會被**永久**擋死，連 TTL 自癒都到不了，
比未實作更糟（codex 複核 2026-07-31）。代價是本條不提供「冒名者過 TTL 仍
擋」這種保證：巢狀 runtime 的暴露面維持 HOOK-OWNER-2 的原狀。
Note: 兩個形狀都是 2026-07-31 本機實測（claude 直接 fork：`docs/rust/
m5-proposal.md` §1；codex 為 npm node launcher fork 原生執行檔、原生執行檔
才 fork hook：`docs/codex-hooks-probe.md` 補測節），不是介面承諾。故本條的
價值只在命中時的即時自癒，落回路徑是常態而非例外處置。
Note: 取樣順序 MUST 是先讀自身 PPID、再走中介（如有）、最後驗 `worker_pid`
的 starttime。反過來的話父行程若在兩次讀取之間退出，hook 會被 reparent，
於是前一步證實了記錄中的行程、後一步卻拿到新的 PPID。TOCTOU 的保證是
**取樣到不一致即落回**，不是「所有 interleaving 都必落回」——worker 在走訪
中途退成 zombie 時 starttime 仍讀得到而可能確認成功，但該 hook 確實出自其
啟動鏈（取樣當下鏈為真），語意無害。
registry 對同互信域的 worker 可寫，本條防的是**巢狀 runtime 意外冒用**，
不是防具寫入權的惡意行為者——後者早已在信任模型之外（STATE-AGENT-4 Note）。
Source: hook_owner_gate

## 三事件語意

### HOOK-EVT-1 [tested: 33]
`prompt-submit`：state 標 `busy`，`last_delivered` 保留原值。
Source: cmd_hook

### HOOK-EVT-2 [tested: 33]
`notification`：僅 payload `notification_type == "idle_prompt"` 時標
`idle`（`last_delivered` 保留）；其他型別或缺欄位 MUST 不動 state
（fail-safe 方向：不誤判 idle，最壞 TTL 後退 legacy）。
Source: cmd_hook

### HOOK-EVT-3 [tested: 33, 34]
`stop`：查 mailbox 最舊一筆派給本 agent 且 `queued` 的任務（狀態 MUST 讀
權威裸 status 檔；task-id 字典序即時間序；目錄名不符 task-id 文法 MUST
就地跳過不中止）。無待辦 → 標 `idle`。有待辦 → 預設標 `busy`、
`last_delivered` 記該 task-id，並向 stdout 輸出 block JSON：
`{"decision": "block", "reason": <含 "agent-bridge receive <id>" 指引>}`。
Source: cmd_hook / hook_oldest_queued

### HOOK-EVT-4 [tested: 33, 34.5+]
Stop 迴圈剎車：payload `stop_hook_active == true` 且待辦 task-id 等於
state 的 `last_delivered`（同一任務已被擋過一輪、模型仍選擇停下）→ MUST
放行（標 idle、不再 block），防無限迴圈。待辦是**不同** id 時 MUST 繼續
block——多任務連鎖是合法路徑，worker 得以一路清空 mailbox。不可放寬。
Source: cmd_hook（stop 分支）

## 與通知端的分工

### HOOK-NOTIFY-1 [tested: 33, 34]
通知端（send/reply/cancel 的共用 gate）讀 state 決定送鍵與否：**只有**
明確讀到 `busy` 且新鮮（0 ≤ now−ts ≤ TTL）才不送鍵（defer，記
notify-deferred 事件，交給 Stop hook 的 block 派工）；其餘一切——新鮮
`idle`、過期、**ts 落在未來**、缺檔、解析失敗、TTL=0——MUST 視為「未知」
走 legacy 送鍵。未來 ts 不擋會讓 busy 永遠新鮮、通知永久停擺且 TTL 救
不回，不可放寬。state 通道完全失效時系統 MUST 退化為純 legacy 行為，
任務不遺失、餓不過 TTL。
Source: notify_or_defer

### HOOK-NOTIFY-2 [tested: 8a, 30]
legacy 送鍵前 MUST 偵測目標 pane 一屏可見文字是否停在 runtime 的權限確認
對話框，是則 MUST NOT 送鍵（送鍵會替 worker 按掉權限框），降級為
notify-failed 警告；pane 已死、capture 失敗同走 notify-failed，任務仍在
mailbox 不遺失。偵測是 best-effort 字串特徵比對，特徵字串 MUST 與現裝
runtime 實際文案保持一致（有 canary 測試守著）。
Source: notify_pane / screen_has_prompt

### HOOK-NOTIFY-3 [tested: 39]
legacy 送鍵前 MUST 確認目標 pane 不在 tmux 的 copy-mode，是則 MUST NOT 送鍵，
降級為 notify-failed（任務仍在 mailbox）。查不到 mode 狀態同樣 MUST 視為不可
送鍵（fail-closed，同 HOOK-NOTIFY-2 的 capture 失敗）。**MUST NOT** 為了送鍵
而把 pane 踢出 copy-mode：捲動位置是人正在介入的現場。
通知路徑用到的**每一個** tmux 子行程（pane 存活查詢、mode 查詢、capture、
送鍵）MUST 有逾時（見 env.md ENV-TMUX-1）：檢查與送鍵之間存在 TOCTOU 空窗，
而 copy-mode 中的 send-keys 實測永不返回；只擋送鍵則任一查詢卡住仍會鎖死
整條 `send`，上限形同不存在。逾時 MUST 與「tmux 起不來」同樣 fail-closed。
Source: notify_pane / pane_accepts_keys / tmux::run_bounded

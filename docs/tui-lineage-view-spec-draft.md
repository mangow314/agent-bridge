# TUI lineage 視覺化 spec 條文草案

本檔不是獨立正本；它是 LV9 合併進 `spec/state.md`、`spec/cli.md` 與
`spec/traceability.md` 的逐字草案。查證標記：下列「現有最高」編號已於
2026-08-11 對 `spec/state.md:26-152`、`spec/cli.md:89-586` 與
`spec/traceability.md:1-78` 查證；其餘 Source 名稱與條文都是待實作、待合併的
規範提案，不是現況主張，也沒有以「推測」補足現況。核對結果：
STATE-AGENT 現有最高 6、STATE-TASK 現有最高 5、CLI-SPAWN 最高 7、
CLI-RELAY 最高 4、CLI-DESPAWN 最高 3、CLI-EVICT 最高 4、CLI-SEND 最高 3、
CLI-GC 最高 3、CLI-UI 最高 2，故以下編號無撞號。

每條前的 HTML comment 是派工 routing metadata；可原樣併入，不影響 rendered
spec。Source 名稱與
[tui-lineage-view-plan.md](tui-lineage-view-plan.md) 定義的 production
介面一致。

## DEVIATION

proposal 第 334 行聲稱 canonical 的 example，其 PID 段
`20260810T143000` 含非數字，違反 STATE-AGENT-6。以下所有條文以既有
STATE-AGENT-6 為正本；合法例為
`AGENT_BRIDGE_SPAWN_TAG=ab-spawn-p4w05-143000-5678f9e8c1d2`。這是 example
修正，不改文法。

---

## 合併至 `spec/state.md`

建議插入位置：STATE-AGENT-6 後加入 STATE-AGENT-7/8/9；STATE-TASK-5 後加入
STATE-TASK-6。

<!-- 落地相位：S2／LV3 -->
### STATE-AGENT-7 [tested: 50]
spawn generation 的終局紀錄 MUST 歸檔於
`agents/archive/<bare-generation>.json`。generation identity 的正本仍是
STATE-AGENT-6 canonical key 全串；archive filename MUST 由同一個受測轉換產生：

1. canonical key 先完整通過 STATE-AGENT-6；
2. 剝除固定 `AGENT_BRIDGE_SPAWN_TAG=` 前綴，餘值作 bare filename；
3. 讀端把 bare 值補回固定前綴、再次通過 STATE-AGENT-6 後才得開檔。

任何一步失敗 MUST 視為 archive 不可證，MUST NOT 組路徑、MUST NOT 嘗試用
未驗字串開檔。寫端與讀端 MUST 共用同一轉換函式；不得各自手抄。

archive 是 live registry 同一次 parse 的完整 object，另加：
`archive_version: 1`、`archived_at: <ISO8601 UTC>`、
`end_kind: <kind>`，以及 optional `wrapup_task_id`。合法 `end_kind`
恰為 `despawned`、`despawned-unsaved`、`despawn-stale`、
`evicted`、`evicted-unfinished`、`evicted-timeout`；其他字面或型別
MUST 視為 invalid archive。只有 evict 流程得寫 `wrapup_task_id`，其他流程
MUST 省略 key，不得寫空字串。archive 建立後不可更新；write MUST 使用同目錄
暫存檔＋rename 的 atomic write。

`Paths::ensure_dirs` MUST 建立 `agents/archive/`。唯讀路徑 MUST NOT 因目錄
缺席而建立它；缺目錄、缺檔、損壞檔或 invalid key 一律 fail-soft 成「無 archive」
證據。live registry 與 archive 若同時宣稱同一 canonical generation，read model
MUST 只採 live record（live wins），不得拼接兩份欄位。

Note: archive 是 audit/provenance，不是 auth/CAS 憑證；不得改變既有 live worker
回收判斷。
Source: ab_core::agent_archive / ab_core::paths::Paths::ensure_dirs

<!-- 落地相位：S1／LV2 -->
### STATE-AGENT-8 [tested: 49]
spawn 出身 registry 得 additive 寫入 optional string `spawn_kind`，合法值恰為
`spawn` 或 `relay`。普通 `spawn` MUST 寫 `spawn`；經
`relay` 共用 spawn 路徑建立者 MUST 寫 `relay`。欄位只在 generation
建立時決定一次，之後不得更新，legacy 不 backfill。

缺欄、非字串或其他字面在 display/projection 一律是 `unknown`，MUST NOT
fallback 成 `spawn`。本欄只供 provenance/display，不得進 auth、CAS、cap、
排序、回收或 task routing。
Source: ab_core::spawn::spawn_locked / ab_core::registry::snapshot

<!-- 落地相位：S1／LV2 -->
### STATE-AGENT-9 [tested: 49]
spawn 出身 registry 得 additive 寫入 optional string `role`。合法 role 文法
恰為 `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`；有帶合法 `--role` 才寫 key，
省略時 MUST 整個 key 缺席，MUST NOT 寫空字串。人工 `register` 不寫本欄；
legacy 不 backfill。

role 只在 generation 建立時決定一次、只供 display。任何讀取失敗、非字串、
invalid 字面或缺欄都顯示 `—`；MUST NOT 從 agent 名、runtime、request、
task sender 或其他內容推測。role 不得進 auth、CAS、排序、回收或 routing。
Source: ab_core::spawn::is_valid_role / ab_core::spawn::spawn_locked /
ab_core::registry::snapshot

<!-- 落地相位：S3／LV3 -->
### STATE-TASK-6 [tested: 50]
task `metadata.json` 得 additive 寫入 optional string
`from_generation`／`to_generation`。兩欄若存在，值 MUST 完整通過
STATE-AGENT-6；建立 task 時決定一次，任何後續狀態轉換不得更新，legacy task
不 backfill。

- `from_generation`：caller 的 `AGENT_BRIDGE_SPAWN_TAG` 裸值 canonicalize
  並驗文法後，只有在 create 時觀察到它與 `from` 對應的 spawn registry
  `spawn_tag` 恰一相符才寫；manual、代送、0 筆或多筆相符一律省略。
- `to_generation`：只有 create 時收件 registry 是唯一可讀、spawn 出身且
  `spawn_tag` 合法才寫；缺席、manual、損壞或 invalid 一律省略。CLI-EVICT-6
  的 exact target 是唯一例外入口。

兩欄只記「create 時觀察到的 provenance」，不是 routing/auth/CAS 保證。
讀端遇到缺欄、invalid 值或 identity 對不上 MUST 視為 unknown/unattached，
不得用 name、時間鄰近或 task sender 補猜。
Source: ab_core::send::observe_generation_refs / ab_core::task::create_task

---

## 合併至 `spec/cli.md`

### `send`

<!-- 落地相位：S3／LV3 -->
### CLI-SEND-4 [tested: 50]
`send` 建 task 時 MUST 依 STATE-TASK-6 做一次 fail-soft generation
observation；觀察結果只決定 optional metadata keys，任何缺失、損壞、invalid
或 ambiguous 都 MUST 省略欄位而維持 send 原本的成功／失敗、stdout、通知與
回滾語意。

normal send MUST NOT 為 generation refs 無條件取得 agents-registry 鎖；
讀到並行換代的前一或後一份完整 atomic snapshot 都是 create-time observation，
不得把 display provenance 升格成同步保證。task directory 一旦建立，兩欄不得
重算。
Source: ab_core::send::create_send_task /
ab_core::send::observe_generation_refs

### `spawn`

<!-- 落地相位：S1／LV2 -->
### CLI-SPAWN-8 [tested: 49]
`spawn` 建立的 registry MUST 依 STATE-AGENT-8 寫
`spawn_kind: "spawn"`；寫入 MUST 位於 spawn/relay 共用的
`spawn_locked` registry builder，不得由 CLI 外殼事後補寫。缺新欄的 legacy
registry 仍可讀且顯示 unknown；既有 pane、cap、tag、ready、stdout 與 rollback
語意不變。
Source: cmd_spawn / ab_core::spawn::spawn_locked

<!-- 落地相位：S1／LV2 -->
### CLI-SPAWN-9 [tested: 49]
`spawn` 接受 additive `[--role <slug>]`；slug 文法見 STATE-AGENT-9。
缺參數、空字串或 invalid slug MUST 在任何 pane、registry、audit 副作用前
拒絕。省略 flag 時 registry MUST 省略 role key，且所有既有 stdout/exit
契約不變。
Source: cmd_spawn / parse_spawn_args / ab_core::spawn::is_valid_role

### `relay`

<!-- 落地相位：S1／LV2 -->
### CLI-RELAY-5 [tested: 49]
`relay` 建立的接手 generation MUST 依 STATE-AGENT-8 寫
`spawn_kind: "relay"`。判定只得使用共用 `SpawnRequest.relay` 是否存在；
不得從 handoff path、depth、名稱或 prompt 推測。registry 寫入仍走
CLI-RELAY-1 要求的共用 spawn 實作。
Source: cmd_relay / ab_core::spawn::spawn_locked

<!-- 落地相位：S1／LV2 -->
### CLI-RELAY-6 [tested: 49]
`relay` 接受與 CLI-SPAWN-9 相同的 additive `[--role <slug>]`。
`cmd_relay` MUST 把原值轉送至共用 spawn parser/request；驗證正本只有
`is_valid_role` 一份。invalid role MUST 在建 pane 前拒絕；省略時不得寫 key。
Source: cmd_relay / parse_spawn_args / ab_core::spawn::is_valid_role

### `despawn`

<!-- 落地相位：S2／LV3 -->
### CLI-DESPAWN-4 [tested: 50]
`despawn` 對通過既有出身／世代／pane 防護的 live registry MUST 在刪除前依
STATE-AGENT-7 best-effort 歸檔，時點依結果分流：

- Killed：確認目標 pane 已消失後、刪 live registry 前寫；
- Absent：確認 pane 本已不存在後、刪 live registry 前寫；
- Stale：確認 pane ownership/tag mismatch 後、**不動該 pane**，刪 live
  registry 前寫，且 `end_kind` MUST 強制為 `despawn-stale`，不得沿用
  evict 傳入的 `evicted*`。

非 evict 的 Killed/Absent，`end_kind` MUST 與既有 audit 判定同源：
`despawned` 或 `despawned-unsaved`。archive write 失敗 MUST 在 stderr
給可見 warning，但 MUST NOT 改變原 DespawnResult、exit code、live registry
刪除、pane 處置或 agents.log audit outcome。若 archive 成功而後續 live
registry 刪除失敗，兩份得暫時並存，讀端依 STATE-AGENT-7 live wins。
Source: ab_core::spawn::despawn_locked / ab_core::agent_archive::archive

### `gc`

<!-- 落地相位：S2／LV3 -->
### CLI-GC-4 [tested: 50]
`gc` 接受 additive `--include-agent-archive`。未帶時 MUST 完全沿用
CLI-GC-1/2/3：只處理 task，既有 stdout/stderr golden byte-for-byte 不變；
`Paths::ensure_dirs` 新建 archive 目錄是 STATE-AGENT-7 明列的 additive
filesystem side effect，不改輸出契約。

帶 flag 時，除既有 task candidates 外才掃 `agents/archive/`。候選 MUST
同時滿足：directory entry 是 regular file（不追 symlink）、filename 可依
STATE-AGENT-7 還原並通過 STATE-AGENT-6、JSON object 的
`archive_version` 是 number 1、`archived_at` 可解析且已達
`--older-than`。任一不確定一律視為年輕保留。dry-run 不刪；`--apply`
才刪，刪前 MUST 重讀並重驗同一組條件；單檔失敗只累計 warning，不中止整輪。

帶 flag 時，每個 archive candidate 的 stdout 格式恰為
`agent-archive<TAB><bare-generation><TAB><end_kind><TAB><archived_at>`，
排在既有 task candidate rows 之後、依 bare generation 字典序；stderr 另列
archive 可刪／已刪／保留／失敗計數。此 flag 與 `--include-notes` 彼此獨立，
不得因清 archive 而放寬 pinned task 保留線。
Source: cmd_gc / ab_core::agent_archive::gc

### `evict`

<!-- 落地相位：S2／LV3 -->
### CLI-EVICT-5 [tested: 50]
`evict` 在完成 await 分流後 MUST 把既有 audit event
`evicted`／`evicted-unfinished`／`evicted-timeout` 與收尾
`task_id` 傳入同一次 `despawn`；Killed/Absent archive 的 `end_kind`
與 `wrapup_task_id` MUST 據此寫入。若 despawn 回 Stale，CLI-DESPAWN-4
優先，archive MUST 記 `despawn-stale`，且 CLI-EVICT-3 仍禁止追加任何
`evicted*` audit。

archive failure 只 warning，MUST NOT 改變 evict 原本的 task wait、
pane 回收、stdout task-id、exit code 或 audit outcome。
Source: ab_core::evict::evict / ab_core::spawn::DespawnCtx

<!-- 落地相位：S3／LV3 -->
### CLI-EVICT-6 [tested: 50]
evict 在入口 precheck/CAS 已取得目標 canonical `gen_tag` 後，收尾 task 的
`to_generation` MUST 直接使用該 exact tag，不再做一次可能跨換代的 registry
observation；`from_generation` 仍依 CLI-SEND-4 fail-soft 觀察。
exact tag 驗證與 task 建立 MUST 留在 CLI-EVICT-4 同一把 registry 鎖內，
通知仍在鎖外。

archive 的 `wrapup_task_id` 與 task 的 `to_generation` 形成雙向
provenance link，但兩者都不得進 auth/CAS。UI 讀到 archive link 而 task
不存在時只能顯示 `task missing (possibly removed by gc)`，不得斷言刪除原因。
Source: ab_core::evict::evict / ab_core::send::create_send_task

### `ui`

<!-- 落地相位：LV1；整合相位：LV7 -->
### CLI-UI-3 [tested: 44, 51]
`ui` dashboard 首屏仍以 attention/triage 為主，不得放完整 lineage tree。
WORKERS 每個 generation 的第二行 MUST 在截斷前顯示恰一個可證 parent label：

- 合法自成根（`lineage_root == spawn_tag` 且 `parent_agent` 缺席）顯示
  `root`；
- `parent_agent` 與當輪唯一 live registry generation 相符時顯示
  `parent: <name>`；
- 有 parent key 但當輪無唯一 record 時顯示
  `parent unavailable`，不得用 task sender、owner 或名稱補猜；
- manual/legacy/invalid 無 generation 證據時顯示
  `parent unavailable (legacy/manual)`。

TASKS 的 unattached row MUST 維持兩行，第二行顯示以下五類之一；分類只用
當輪 registry、task `to`、task `created_at`、worker
`registered_at` 與既有嚴格 `created_at > registered_at` attach 判準：

1. 無同名 current registry row：
   `recipient absent from current registry`；
2. 任一必要時刻解析失敗：`timestamp invalid`；
3. 嚴格 attach 命中超過一列：`multiple matches`；
4. 0 命中且 task 與至少一候選 registration 同秒：
   `same-second · unprovable`；
5. 0 命中且 task 早於候選 registration：
   `task predates current registration/generation`。

reason 不得寫 `never registered` 或其他歷史全稱；status 必須保留權威六字，
只有寬度不足時才可用既定 display alias。
Source: ab_tui::model::parent_label /
ab_tui::model::unattached_reason / ab_tui::view

<!-- 落地相位：LV4／LV5；整合相位：LV7 -->
### CLI-UI-4 [tested: 51]
`ui` 的 `t` 鍵 MUST 在 dashboard 與唯讀 lineage view 間切換；返回時以
canonical generation `TreeKey::Generation` 映回同一 `(name, spawn_tag)`，
generation 已消失時才依既定 nearest relocation。首屏不得因本視圖改變 attention
排序。

lineage projection 的 v1 集合恰為 current live generations、由它們的
`parent_agent` 逐跳 direct-read 得到的 archive ancestors、第一個缺席 parent
tombstone、單一 synthetic `… unknown generations` gap，以及 current manual
standalone display rows；MUST NOT 全掃 archive 重建已消失旁支。live/archive
同 generation 時 live wins。edge 只由 canonical parent 關係產生；
`spawn_kind` 對應 `──▸ spawn`／`══▸ relay`／`··?▸ unknown`，
缺/invalid kind MUST 是 unknown，不得猜 spawn。task sender、owner、agent name
與時間鄰近一律不得產生 derivation edge。

width >80 MUST 使用 2D flow；width ≤80 MUST 使用 compact。兩 renderer MUST
共用同一 projection、label/style mapper、DFS selectable order 與 TreeKey；
80↔81 只重算 geometry，不改 node/edge 集合或 selection。manual standalone、
gap、純 missing tombstone不可選；active 與 archived generation 可選。

flow generation box MUST 固定寬、框內恰三行
`name`／`[role|—] · runtime`／
`<authoritative task status or idle> · <liveness/blocker>`，總高恆 5。
layout MUST 是 ordered forest 的 O(V+E) span/interval 演算法：subtree 佔連續
row interval、parent 位於 interval 中點、depth 決定 x、root bands 不重疊。
shared trunk 一律中性，只有進 child 的末段水平線編碼 edge kind；edge 先畫、
box 後畫，線不得穿框。compact generation/manual row 固定兩行；
gap/tombstone 是單行 decoration。

`j/k` 是 DFS 前後，`h/l` 是 parent/first child，兩模式同義。camera MUST
以最小位移保持 selected rect 完整可見；內容超出 viewport 時 title 顯示
`node i/N · canvas WxH · offset x/y` 並畫右/下兩軸 scrollbar。v1 MUST NOT
提供 collapse、free-pan、zoom、Sugiyama/force、cross-edge 或外部 renderer。
selection 使用 `▶`＋selected background；Thick border 只代表 focused panel。

`Enter` 只對 live generation 沿用 CLI-UI-1 focus；archived/manual/gap/missing
footer 不得宣稱有 focus。任何 lineage view 操作除既有 focus 外 MUST NOT 寫
task、registry、archive 或 audit。
Source: ab_tui::lineage / ab_tui::app / ab_tui::view

<!-- 落地相位：LV6；整合相位：LV7 -->
### CLI-UI-5 [tested: 51]
lineage view 選中 generation 時，`y` MUST 開 lifecycle timeline，
`i` MUST 開 generation info；confirm overlay 已開時 `y` 仍只代表 yes。
`Esc` 先關最上層 overlay，再由 lineage 返回 dashboard；`q` 恆為全域 Quit。
不得新增 `d`，`+`/`-` 不得冒充 zoom。

timeline 只得使用 archive/live registry timestamps、可辨識的 child
`spawned_at`、task `events.log` 與權威 status。task lifecycle row 的 raw
event 字面只用 `delivered`、`started`、`replied`、`failed`；
`completed` 是 status projection，MUST 顯示成
`replied → (status: completed)`，不得偽裝 raw event。ready 只有 bool，
顯示 `ready: yes · timestamp unavailable`，不得捏造時間。relay 不是 task，
只能顯示 child `spawned · via relay`；不得顯示 handoff path 或 hop depth。
archive/read 缺失必須顯示 unavailable/missing；超過取得上限只能顯示
`older events omitted by read limit`，不得寫 truncated/rotated。

generation info MUST 分成互不混合的
`INBOUND TASKS (to_generation)`、
`OUTBOUND TASKS (from_generation)`、
`CHILD DERIVATIONS (spawn_kind)`。task sender 不證明 lineage parent；
relay derivation 不得放入 OUTBOUND TASKS。task duration 缺任一端 event 時間
顯示 `unavailable`。evidence 命令只給真 task id：
`agent-bridge read <id>`／`agent-bridge status <id>`。

取得路徑 MUST bounded：generation task 候選只用 read model 已截限的最新 200
筆；每個 `events.log` 最多讀檔尾 8192 bytes、最多保留 64 records，從中段
切入的首個半行丟棄；單一 overlay 合併後最多保留 256 events。任一上限截斷都
必須顯示 omitted。timeline 讀取 MUST 跑在一次性 background worker，UI thread
不得阻塞；結果必須攜帶 TreeKey，晚到時若 selection 已換代即丟棄。

role 缺/invalid 顯示 `—`；archived/end_kind、pane liveness、blocker/task
status 是不同軸，不得把 archived 染成 dead 或造綜合健康度。v1 overlays
唯讀，不得新增 tmux query 或 mutation。
Source: ab_tui::lineage_detail / ab_core::task::event_lines_bounded /
ab_tui::worker::Handle::spawn_oneshot

---

## 合併至 `spec/traceability.md`

插入 group 48 後；新條款都有測試，故檔頭既有 `untested 計數：2` 不變。

| 分組 | 主題 | 條款 |
|---|---|---|
| 49 | lineage S1：spawn_kind＋explicit role | CLI-SPAWN-8, CLI-SPAWN-9, CLI-RELAY-5, CLI-RELAY-6, STATE-AGENT-8, STATE-AGENT-9 |
| 50 | lineage S2/S3：agent archive＋task generation refs | CLI-SEND-4, CLI-DESPAWN-4, CLI-GC-4, CLI-EVICT-5, CLI-EVICT-6, STATE-AGENT-7, STATE-TASK-6 |
| 51 | TUI lineage view／breakpoint／overlay 整合 | CLI-UI-3, CLI-UI-4, CLI-UI-5 |

同步要求：`tests/run-tests.sh` 的 group 49–51 標頭 `# spec:` mirror 必須逐字
對應上表；`GRP_KNOWN` 加入三組，有跨組 fixture 才在 `GRP_NEEDS` 明列。

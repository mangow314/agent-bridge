# agent-bridge TUI lineage 視覺化提案（`t` 視圖＋P0 首屏補洞）

- 狀態：**已定案開工**（LV0 由使用者 2026-08-11 指示通過：鍵位 `t`/`y`/`i`、
  批次 S1→S2→S3、P0 首屏先行照案；實作計畫見
  `docs/tui-lineage-view-plan.md`，spec 條文草案見
  `docs/tui-lineage-view-spec-draft.md`，layout 原型見
  `docs/lineage-prototype/`）
- 性質：`docs/tui-design.md` 的增量提案；定案後由本檔回填正本（§9 新增相位）
- 依據：四輪跨廠討論收斂（claude 協調、codex＋agy pane worker 各自獨立
  作答後交叉對質；記錄見 §8）。使用者兩次方向輸入：
  1. 「以樹狀圖／架構圖／生命週期圖呈現 orchestrator 與 worker 關係與身分
     （planner/executor），一眼辨識」（2026-08-10 立案句）
  2. 「mermaid 流程圖式的方框連線呈現」（同日補充；80 欄以下降級縮排樹）

---

## 1. 一句話與動機

在既有 dashboard 之上新增唯讀的 **lineage 視覺化二級視圖**（`t` 鍵）：
寬終端畫 mermaid 式 2D 方框 forest（派生邊／relay 邊／unknown 邊三種線型、
含已終局的歸檔節點），窄終端降級縮排樹；並先行在首屏補兩項一階因果資訊
（直系 parent 標籤、unattached 理由欄）。

動機三源合流：

- **PD1 實測 1/3 未過**（tui-design.md §9）：「誰派生此 blocker」答不出
  （parent 藏在 DETAIL）、墓碑鏈只見一代、unattached 無理由欄。
- **正本既有伏筆**：「attention 首屏、lineage tree 二級鍵」的版面演進已有
  裁定方向但尚未開批（tui-design.md §9 rubric v2 注意事項）。
- **差異化**（tui-design.md §12）：跨 runtime、父子鏈鑑識是第一方
  （Agent Teams／Agent View）明文不做的三件事之二。

## 2. 目標／非目標增量

### 目標

- P0 首屏（不動 attention 主體）：WORKERS 第二行加 `parent: <name>`
  （區分 `root` 與 `parent unavailable (legacy/manual)`）；unattached task
  顯示可查證的不可證原因（五類，見 §5 mockup 1）。
- `t` 唯讀 lineage 視圖：>80 欄為 2D 方框 forest，≤80 欄為縮排樹；
  兩者顯示同一組 generation、同一派生關係與缺席證據（只允許幾何不同）。
- 節點框固定寬三行：`name`／`[role|—] · runtime`／`<權威 task status 或
  idle> · <liveness/blocker glyph>`。三條語意軸（task status／liveness／
  blocker）獨立呈現，不壓成綜合「健康度」。
- `y`：選中 generation 的垂直 lifecycle timeline overlay。
- `i`：選中 generation 的 info overlay，內分三個獨立區塊：
  INBOUND TASKS（`to_generation`）／OUTBOUND TASKS（`from_generation`）／
  CHILD DERIVATIONS（`spawn_kind`）——**派工邊與派生邊永不混畫**。
- 缺席與不確定性是第一級字面：`† missing`、`… unknown generations`、
  unknown edge `··?▸`，不得只靠 glyph／顏色。

### 非目標（越線即設計錯誤）

- 不在首屏放樹；lineage 仍是 dashboard 二級鑑識能力。
- 不做通用 graph editor、任意 cross-edge、Sugiyama/force 自動排版、
  Kitty graphics、Mermaid subprocess、外部 renderer；依賴上限維持
  ratatui＋crossterm＋ab-core。
- v1 `t`／overlay 全唯讀；mutation（focus 例外，見鍵位）留在 dashboard。
- 不推測 role（不從名字、request 內文回填）；不做 dependency critical
  path、進度百分比、token/cost（三廠共識「不做」清單）。
- 不宣稱完整歷史：v1 forest＝現存 generation＋可由 archive 沿 parent
  直接追到的祖先；不掃全 archive 重建已消失旁支。
- 不補 `ready_at`：timeline 顯示 `ready: yes · timestamp unavailable`。
- 協定語意零改變：全部 schema 變更為 additive optional。既有 stdout／
  exit code／live registry／audit outcome 不變；S2 的 archive 檔、其失敗
  warning 與 `ensure_dirs` 新目錄是**明列的 additive 可觀察副作用**，
  不在「byte-for-byte 不變」的宣稱範圍內（該宣稱只適用於不帶新參數的
  既有輸出面，如 `gc` 不帶新旗標時的 golden）。

## 3. additive schema（S1→S3 批次）

以下條款編號為建議值，落地時與 `spec/` 現況對齊。role／spawn_kind／
task refs 僅 provenance/display，不進 auth/CAS/排序/回收判斷；archive 的
`archived_at`／`archive_version` 供 archive GC 自身流程使用（CLI-GC-4），
不影響任何 live worker 的回收決策。

### S1（同批，先行）：`spawn_kind`＋`--role`

- **`spawn_kind`**（STATE-AGENT-8、CLI-SPAWN-8、CLI-RELAY-5）：spawn-born
  registry optional string `"spawn"|"relay"`。`SpawnRequest` 已帶
  `relay: Option<Relay>`（spawn.rs:142-163），共用寫入點直接決定字面。
  legacy 缺席＝unknown，任何其他字面／型別＝invalid→unknown，
  **不 fallback 成 spawn**；畫面對應第三線型 `··?▸`。
- **`--role`**（STATE-AGENT-9、CLI-SPAWN-9、CLI-RELAY-6）：
  `spawn|relay … [--role <slug>]`，slug `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`，
  建 pane 前驗證；省略時 registry key 缺席、UI 顯 `—`；空字串不落盤。
  relay 重組 spawn args 時必須轉送 role。display-only。

### S2：registry 歸檔＋archive GC

（STATE-AGENT-7、CLI-DESPAWN-4、CLI-EVICT-5、CLI-GC-4）

- 新目錄 `agents/archive/<bare-generation>.json`。檔名規則：寫入前先以
  STATE-AGENT-6 驗 **canonical key 全串**（含 `AGENT_BRIDGE_SPAWN_TAG=`
  前綴），通過後剝除固定前綴得 bare filename；讀端反向套用同一轉換函式
  （bare 組回 canonical、再驗、才開檔），兩端共用一份轉換＋測試。
  archive 保留 live registry 全欄位，尾加：
  `archive_version:1`、`archived_at`、`end_kind ∈ {despawned,
  despawned-unsaved, despawn-stale, evicted, evicted-unfinished,
  evicted-timeout}`、`wrapup_task_id`（僅 evict 有值，否則 key 缺席）。
- 唯一寫入點在 `despawn_locked`（evict 經 `DespawnCtx` 傳入 end_kind＋
  wrapup id），寫入時點按回收結果分支：
  - **Killed／Absent**：pane 回收或確認缺席後、刪 live registry 之前
    atomic-write。
  - **Stale**（ownership mismatch；現行本就**不回收 pane**、只清登記，
    spawn.rs:1121-1127）：確認 mismatch 後、刪 registry 之前寫，且
    end_kind **強制覆寫為 `despawn-stale`**，不得沿用 evict ctx 的
    `evicted*`。
- **失效方向**：歸檔失敗只出可見 warning，原 despawn/evict 的 exit／
  outcome 逐字不變（協定語意零改變的必要代價）；缺 archive 時 UI 誠實退
  tombstone，不假裝有歷史。live 與 archive 同 generation 並存時 live wins。
- `gc` 不帶新旗標時 byte-for-byte 維持現契約（只處理 task）；additive
  `--include-agent-archive` 才納入，年齡取 `archived_at`（解析失敗視為
  年輕保留），只碰 strict filename＋regular file＋`archive_version:1`。
- **補強（agy 終審）**：
  1. `Paths::ensure_dirs` 必須補建 `agents/archive/`，否則首次歸檔
     atomic_write 因目錄不存在 ENOENT。
  2. TUI 讀 archive 前，bare filename 必須先組回 canonical 並通過
     STATE-AGENT-6 校驗才開檔（路徑只由已驗 generation 產生，阻斷
     path traversal；與上方檔名規則同一轉換函式）。

### S3（同批）：task generation refs＋evict wrap-up link

（STATE-TASK-6、CLI-SEND-4、CLI-EVICT-6）

- task `metadata.json` optional `from_generation`／`to_generation`，值僅限
  canonical generation key；**建立時決定一次**，不更新、不 backfill：
  - `from_generation`：caller env tag canonicalize 後與 `from` 對應 registry
    的 `spawn_tag` **恰一相符**才寫；手動／代送／ambiguous 省略。
  - `to_generation`：建立當下唯一可讀有效的收件 registry generation；
    讀不到／invalid 省略。
- 兩欄是「create 時觀察到的 provenance」，**不是 routing/auth 保證**；
  不得為此讓 send 無條件取 registry 鎖（send 現行不持鎖，send.rs:21-53）。
  UI 只在 generation 對得上時掛邊，否則 unattached/unknown。
- evict 已持有目標 `gen_tag`＋`task_id`（evict.rs:252-296）：收尾 task 的
  `to_generation` 直接用該 tag；despawn 完成時 archive 補 `wrapup_task_id`。
- **懸空退化（agy 終審補強）**：引用的 task 讀不到時顯示
  `task missing (possibly removed by gc)`——資料只能證明 task 不存在，
  不能斷言刪除原因（gc／人工刪除／損壞皆可能）；fail-soft 不阻塞渲染。

## 4. 視圖設計

### 4.1 斷點與雙模式

- **width > 80**：2D 方框 flow；**width ≤ 80**：縮排樹 compact。
  79/80/81 三寬各有測試；模式切換只重算幾何，TreeKey 與 selection 不變。
- 兩模式共用一份 `LineageProjection`（node identity、parent/children、
  edge kind、role/runtime/status、gap/tombstone、DFS order、filter）＋
  label composer＋style mapper＋selection relocation；layout 各自一個小
  純函式（`FlowLayout → SceneGeometry`／`CompactLayout → rows`）。
  兩份幾何，一份 lineage 規則。

### 4.2 flow 佈局（手排，O(V+E)，零新依賴）

1. `LineageProjection` 建 ordered forest；排序依 generation key／
  `spawned_at` canonical order（**不吃 attention 排序**，blocker 升降不得
  使整張圖跳位）。
2. bottom-up `span(node)=max(1, Σ span(child))`；每 subtree 佔連續 row
   interval；node center y＝自身 interval 中點。
3. `x = depth × (BOX_W + COL_GAP)`；固定 `BOX_W`、`BOX_H`（框內三行）。
4. 多 root 各佔互斥垂直 band（`ROOT_GAP` 分隔）；canvas bounds＝所有
   rect/segment 聯集。
5. parent 右中點出線 → column gap 中的共同 trunk（**一律中性 `│`**）→
   進 child 左中點前的**末段水平線才編碼 kind**：`──▸` spawn／`══▸`
   relay／`··?▸` unknown。edge 先畫、box 後畫，線不穿框。
6. 退化案例：長 relay 鏈→寬度 O(depth)，靠 camera 不折回（relay depth
   上限可調不可 hardcode）；星狀 fan-out→高度 O(leaves)；gap＝**單一
   synthetic display node** `… unknown generations`（不代表恰一代）；
   cycle/duplicate/malformed 沿用現行 fail-closed，入 standalone/invalid
   band，不「修成一棵樹」。

### 4.3 selection／camera／可測性

- stable key：`TreeKey::Generation(canonical_tag)`；archive/live 同代同
  key。gap、純 missing tombstone 與 **manual standalone（無 generation
  身分，display-only）皆不可選取**；有 archive record 的已終局 generation
  可選（僅開 `y`/`i`，無 focus/evict capability）。標題列 `node i/N`
  只計 generation 節點。
- `j/k`＝DFS 前後；`h/l`＝parent／first child（flow 與 compact 同義；
  **v1 不做 collapse**，避免兩模式 selection 語意分裂）。
- camera：selection-follow 最小位移（selected rect 恆完整在 viewport），
  **v1 無 free-pan、無 zoom**；標題列顯示 `node i/N · canvas WxH ·
  offset x/y`，右／下兩軸 scrollbar——內容超出 viewport 必有可見訊號
  （P5.1「隱藏內容不可零提示」的 2D 版本）。
- 節點選取用 `▶`＋selected background；**Thick 邊框保留給 focused panel**
  （theme.rs 既有語意），不用於節點。
- 單一 `SceneGeometry` 同時產出 node_rects／edge_segments／
  selectable_order／canvas_bounds；render、導航、camera、scrollbar 全部
  只讀這一份（P5.1 行號會計教訓的 2D 對應物）。
- TestBackend：沿用現行 clone-Buffer 斷言，不新增 snapshot crate。純
  layout 不變量（rect 不重疊、parent.x < child.x、band 互斥、edge 不穿
  框、deterministic、cycle 有界）＋buffer golden 四份（120×40 flow／
  81×24 flow 邊界／80×24 compact 邊界／79×24 compact）＋camera gate
  （深鏈／寬星／multi-root 全程 selected 可見）。

### 4.4 鍵位定案（終審裁決）

| 鍵 | 動作 | 備註 |
|---|---|---|
| `t` | Dashboard ⇄ Lineage view | 返回時 selection 映回 `(name, spawn_tag)` |
| `y` | 選中 generation 的 timeline overlay | 與 confirm overlay 的 `y=yes` 情境互斥，無衝突 |
| `i` | 選中 generation 的 info overlay（inbound/outbound/derivations 三區） | 既有 worker info 的自然延伸；**不新增 `d`**（易聯想 despawn） |
| `j/k` | DFS 前後 | 方向鍵 mirror |
| `h/l` | parent／first child | 兩模式同義；不做 collapse |
| `Enter` | live generation 沿用既有 focus pane 語意 | archived/missing 無此 capability，footer 不顯示 |
| `Esc` | 先關最上層 overlay，再由 lineage 回 dashboard | `q` 保留全域 Quit |
| `?` | contextual help | `+`/`-` zoom 明確不做 |

## 5. Prototype mockups（終審修正版）

> 皆為示意；成品字寬／截斷依 `BOX_W` 常數。chrome 英文、task status 權威
> 六字、短 id 僅為版面佔位（evidence 區保留 canonical id）。

### 5.1 首屏 P0 增量（>80 欄；回答：誰要處置？誰派生它？unattached 為何滯留）

```
┌ WORKERS ─ 3/12 ──────────────────────────────────────────────────────────────┐
│ ▸ p4w01           codex   running    main:0 %101                             │
│     running 12m04s · task …a1b2 plan-stage design assessment · root          │
│ ▸ p4w05           claude  running    main:1 %105                             │
│     running 5m12s · task …c3d4 relay execution block · parent: p4w01         │
│ ▸ p4w06           claude  running ⛔  main:2 %106                            │
│     running 8m30s ⛔ · task …e5f6 permission prompt · parent: p4w01          │
├ TASKS ─ scope: unattached 2/20 ──────────────────────────────────────────────┤
│ ⚙ …9999  to p4w99   queued 45m                                               │
│     unattached · recipient absent from current registry                      │
│ ⚙ …7c21  to p4w01   queued 45m                                               │
│     unattached · task predates current registration/generation               │
└──────────────────────────────────────────────────────────────────────────────┘
```

unattached 理由五類（fail-closed 分類函式，全部現成資料可判）：
`recipient absent from current registry`／`task predates this generation`／
`same-second · unprovable`／`timestamp invalid`／`multiple matches`。

### 5.2 `t` flow 模式（>80 欄；回答：拓撲、邊種類、歸檔終局與斷代在哪）

節點框固定：`BOX_W` 統一、框內恰三行（name／role·runtime／status），
總高恆 5；選取以 `▶` 標在框內 name 行。

```
┌ LINEAGE ─ 3 bands · 7 generations (2 archived · 1 missing) ─ node 5/7 · canvas 128x24 · offset x+18 y+0 ─┐
│                                                                                                          │
│ ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│ │ p4w01            │  │ p4w05            │  │ p4w02            │  │ p4w03            │  │▶p4w04            │
│ │ [planner] · codex│─┬▸│ [executor]·claude│══▸│ — · codex        │══▸│ — · claude       │══▸│ [tester] · agy   │
│ │ running 12m      │ │ │ running 5m       │  │ archived·evicted │  │ archived·despawnd│  │ running 2m       │
│ └──────────────────┘ │ └──────────────────┘  └──────────────────┘  └──────────────────┘  └──────────────────┘
│                      │ ┌──────────────────┐                                                               │
│                      │ │ p4w06            │                                                               │
│                      └▸│ [executor]·claude│                                                               │
│                        │ running 8m ⛔    │                                                               │
│                        └──────────────────┘                                                               │
│ ──── band: partial lineage ─────────────────────────────────────────────────────────────────────────────  │
│                                              ┌──────────────────┐                                         │
│  … unknown generations ─ † …b8ef missing ··?▸│ p4w07            │                                         │
│                                              │ — · codex        │                                         │
│                                              │ idle 3m          │                                         │
│                                              └──────────────────┘                                         │
│ ──── band: standalone (display-only) ───────────────────────────────────────────────────────────────────  │
│  ┌──────────────────┐                                                                                     │
│  │ p4w09            │  manual registration · not selectable                                               │
│  │ — · claude       │                                                                                     │
│  │ idle 42m         │                                                                                     │
│  └──────────────────┘                                                                                     │
│                                                                                                          │
│ legend: ──▸ spawn  ══▸ relay  ··?▸ unknown  † missing-reference  ▶ selected  archived=audit end_kind      │
└────────────────────────────────────────────────────────────────────────────────── Esc back · q quit ──────┘
```

（`node 5/7`：可選取的 generation 節點依 DFS 為 p4w01→p4w05→p4w02→
p4w03→p4w04→p4w06→p4w07 共 7；p4w09 無 generation 身分，display-only
不入計數。`archived·despawnd` 是 `BOX_W` 截斷示意，完整字面在 overlay。）

語意鐵則：`† missing` 只給「有引用、無資料」的缺席參照；archived 節點寫
`archived · <end_kind>`（audit 事實，不染 dead 色）；pane dead 是 liveness
軸另行標示；三者不得混為一談。root 不冒充 orchestrator——手動 caller 無
generation 身分，v1 這裡只有 `manual registration`／`external origin` 字面。

### 5.3 compact 模式（≤80 欄；同一份資料的一維降級）

```
┌ LINEAGE (compact) ─ node 5/7 ────────────────────────────────┐
│ p4w01   [planner] codex · running 12m · root                 │
│           task …a1b2 plan-stage design assessment            │
│ ├──▸ p4w05   [executor] claude · running 5m                  │
│ │       task …c3d4 relay execution block                     │
│ │ ╘══▸ p4w02   — codex · archived · evicted                  │
│ │         wrap-up task …8aa1 (replied)                       │
│ │   ╘══▸ p4w03   — claude · archived · despawned             │
│ │           no wrap-up task                                  │
│ │     ╘══▸ ▶p4w04   [tester] agy · running 2m                │
│ │             task …9f00 verify layout invariants            │
│ ├──▸ p4w06   [executor] claude · running 8m ⛔               │
│ │       running 8m30s ⛔ · task …e5f6 permission prompt      │
│ ─ partial lineage ───────────────────────────────────────────│
│ … unknown generations                                        │
│ ╘ † …b8ef missing                                            │
│   ╘··?▸ p4w07   — codex · idle 3m                            │
│           idle 3m · last: replied (12m ago)                  │
│ ─ standalone (display-only) ─────────────────────────────────│
│ p4w09   — claude · idle 42m · manual registration            │
│           idle 42m · last: replied (10m ago)                 │
└─────────────────────────────────────────── Esc back · q quit ┘
```

**節點固定兩行列**（與現行 dashboard 行高會計同源）、無 collapse
（`▾/▸` 不存在）；gap（`… unknown generations`）與 tombstone
（`† …b8ef missing`）是**單行裝飾列、不可選取**，不入 node 計數；
edge 標記與 flow 完全同三種。

### 5.4 `y` timeline overlay（回答：這一代從生到滅經歷了什麼）

```
┌ TIMELINE ─ p4w02 · gen …9d2e (short · full key in `i`) ──────────────────────┐
│  14:50:00  spawned · via relay · parent gen t…-5678 (p4w05)                  │
│            ready: yes · timestamp unavailable                                │
│  14:50:15  task …2222  delivered → started 14:50:16 → replied 15:31:00       │
│            (status: completed)                                               │
│  15:40:00  child spawned via relay · gen t…-77aa (p4w03)                     │
│  ── older events omitted by read limit ──                                    │
│  16:02:11  archived · evicted · wrap-up task …8aa1 (replied)                 │
│            wrap-up notes: $ agent-bridge read …8aa1                          │
└──────────────────────────────────────────────────────────────── Esc close ───┘
```

規則：raw task event 只用 `delivered/started/replied/failed`，`completed`
是狀態投影必須寫成 `replied →（status: completed）`；ready 無時戳；relay
不是 task，只能以子代 `spawned · via relay` 呈現；不寫 handoff 路徑；
超額只寫 `older events omitted by read limit`（沒有 rotation 證據不得寫
truncated/rotated）；`wrapup_task_id` 懸空時顯示 `task missing (possibly
removed by gc)`——只證明不存在，不斷言原因。

### 5.5 `i` info overlay（回答：它收了什麼、發了什麼、派生了誰）

```
┌ INFO ─ p4w05 · [executor] · claude ──────────────────────────────────────────┐
│ generation : AGENT_BRIDGE_SPAWN_TAG=ab-spawn-p4w05-143000-5678f9e8c1d2      │
│ provenance : spawned (spawn) · parent p4w01 · root p4w01 · origin main:1     │
│                                                                              │
│ INBOUND TASKS (to_generation)                                                │
│  • …1111  from p4w01 · completed · 7m45s                                     │
│  • …c3d4  from p4w01 · running · started 5m ago                              │
│ OUTBOUND TASKS (from_generation)                                             │
│  • none recorded                                                             │
│ CHILD DERIVATIONS (spawn_kind)                                               │
│  • p4w02 · relay · 14:50:00 · archived                                       │
│                                                                              │
│ evidence: $ agent-bridge read …c3d4 · $ agent-bridge status …c3d4            │
└──────────────────────────────────────────────────────────────── Esc close ───┘
```

三區各自獨立：task sender ≠ lineage parent（派工邊不證明派生邊）；relay
不入 OUTBOUND（不是 task）；duration 缺任一端事件時間顯示 unavailable；
hop depth 無持久化欄位，不顯示。

## 6. Phases × Gates

測試分級依 `docs/testing-policy.md`（phase 內 `TEST_GROUPS=受影響分組`，
收案必跑全套，PARTIAL 不得作收案證據）。分組 49–51 為建議新編號；各
phase 落地時 gate 必須同步把新分組登記進 `tests/run-tests.sh` 的
`GRP_KNOWN`（現行只到 48；跨組 fixture 再補 `GRP_NEEDS`），否則
`TEST_GROUPS=49/50/51` 直接以 unknown group 失敗。

| Phase | 內容 | Gate（可機器判定除非另註） |
|---|---|---|
| LV0 設計定案 | 本提案 | **human judgment**（rubric 四題，逐題可由本文證據答是/否，全 yes 才開工）：(1) root 不冒充手動 orchestrator？(2) unknown edge／gap／archived-ended 三者在畫面可區分？(3) archive 失敗不改既有 despawn/evict outcome？(4) 80/81 模式切換與 selection 行為已明定？ |
| LV1 P0 因果補洞 | 首屏 parent 標籤＋unattached 五類理由 | `cargo test -p ab-tui p0_causal`：blocker 列字元層含正確直系 parent；五種 reason 各命中且不得寫成真實 owner 推斷（`never registered` 這類超證據斷言為負向斷言） |
| LV2 S1 schema | `spawn_kind`＋`--role`（spawn/relay 共用寫入） | `TEST_GROUPS=49`：帶/不帶 role 的 registry golden；invalid role 建 pane 前失敗；legacy 缺欄可讀；既有 stdout/exit 不變 |
| LV3 S2/S3 provenance | archive＋archive GC＋task generation refs＋evict link | `TEST_GROUPS=50`：root→A→B 歸檔 A 後可由 generation 讀回；end_kind 分流；wrapup id 雙向對得上；**歸檔寫失敗只警告且原 outcome 不變**；`ensure_dirs` 建 archive 目錄；gen_key 未過文法不開檔；不帶新旗標時既有 gc golden byte-identical |
| LV4 projection＋compact | `LineageProjection`＋TreeKey＋≤80 縮排樹 | `cargo test -p ab-tui lineage_projection`：active/archive/gap/tombstone/legacy/duplicate/cycle fixture 的 node/edge/DFS golden；79×24 buffer 逐字合 compact golden |
| LV5 2D flow＋camera | 分層 layout＋三線型＋兩軸 viewport＋80/81 斷點切換 | `cargo test -p ab-tui lineage_flow`：layout 不變量（不重疊/不穿框/deterministic/selected 恆可見）；120×40、81×24 flow golden（80×24/79×24 為 compact golden，屬 LV4）；80↔81 resize stable-key 斷言 |
| LV6 overlays | `y` timeline＋`i` info＋bounded archive/event read | `cargo test -p ab-tui lineage_timeline`：時序排序；缺 ready_at／缺 archive／懸空 wrapup 明示 unavailable/missing；超額顯示 omitted 且取得路徑 record/byte bounded |
| LV7 CLI/TUI 整合 | `t`/`y`/`i` 鍵、返回位置、真 tmux 寬窄 | `TEST_GROUPS=51`：120×40 進 flow、79×24 進 compact；兩者 generation/edge 集合相同；返回 dashboard 落同一 generation；全程零 task/registry mutation |
| LV8 理解驗收 | 真實終端三寬 | **human judgment**（rubric 四題，60 秒 4/4 且指出畫面字面證據）：(1) blocker 的直系 parent 是誰？(2) 哪條邊是 relay、哪條 unknown？(3) 哪一代 missing、哪一代 archived-ended？(4) 選中 worker 的 wrap-up task 如何結束？ |
| LV9 收案 | 規格回填 tui-design.md＋demo＋全回歸 | `cargo clippy --all-targets -- -D warnings && cargo test && tests/run-tests.sh` 全綠、無 `PARTIAL RUN` |

## 7. 風險與顯示紀律自查

1. **假拓撲**：legacy 無 spawn_kind、手動端無 generation、archive 可缺——
   unknown edge／external root／gap 全是第一級字面；不得用名字、owner、
   時間鄰近補線。
2. **畫布爆長**：cap 與 relay depth 皆可調——SceneGeometry＋雙軸溢出
   提示＋selection-follow camera；不以預設 4 當硬界。
3. **兩 renderer 漂移**：projection/label/style/selection 共用；80/81
   （斷點兩側）fixture 斷言 node/edge 集合相同、只許幾何不同。
4. **格點邊誤讀**：混合 trunk 中性 `│`、末段才編碼；不得只靠色彩區分
   （ANSI 16／truecolor 字元層一致）。
5. **選取／捲動說謊**（P5.1 教訓）：幾何單一事實源；TreeKey 鎖
   generation，不以名字接續 respawn。
6. **archive 擴大失敗面**：best-effort＋警告，既有 outcome 不變。
7. **讀取成本**：archive 依 generation filename 直查，不做每 500ms 全掃；
   timeline 讀取 record/byte 有界，超額明示 omitted。

顯示紀律逐條：不暗示可刪度（idle/role/archived/disposable 不進排序、
不預選、無「安全」語彙）；權威六字不變（`archived/ended/relay` 是 agent
provenance 字，不冒充 task status）；雙軸不混（⛔ 與 pane dead 各自顯示）；
role display-only 缺就 `—`；v1 無 mutation，日後若在 `t` 開 evict 仍逐字
`wrap-up task, then reclaim`＋等價 CLI 原文上畫面。

## 8. 跨廠討論記錄（仲裁稽核）

四輪（上限五輪，第四輪收案）；vehicle＝agent-bridge pane worker
（`tui-viz-codex`／`tui-viz-agy`），plan-stage 意見不佔 per-diff review cap。

- **R1 獨立**（codex `20260810T145037Z-f1ca`／agy `…-515c`）：大方向即收斂
  （二級鍵樹、首屏補 parent/unattached、role additive、樹≠timeline）。
- **R2 對質**（`…-7e16`／`…-97ec`）：三分歧全數收斂——agy 撤回名字慣例
  fallback（改 `—`）、撤回全池泳道圖（改選中世代 timeline）、撤回
  「hop depth 現成」（RELAY_DEPTH 僅子代 env，spawn.rs:786-788）；codex
  接受 P3 靜止時間但修正文案（`state unchanged`，用 `updated_at`）。
- **R3 立案**（`…-ff9e`／`…-8d5a`）：codex 出提案本體並修正三前提
  （cap/depth 可調→畫布可裁切；spawn_kind 缺值→第三線型；root 不冒充
  orchestrator）；agy 出五張 mockup。
- **R4 交叉終審**（`…-ae9b`／`…-2294`）：codex 判 mockup 1、3 修後採用、
  2、4、5 重畫（READY 時戳／hop／[manual] 當 role／handoff 路徑四疑點
  全成立），並裁鍵位 `t/y/i`（不增 `d`）、斷點 ≤80、無 zoom／collapse；
  agy 審提案通過＋三補強（ensure_dirs／gen_key 校驗／懸空 task 降級），
  全數收入 §3。mockup 修正由協調者依 R4 指示落於本檔 §5。
  仲裁備註：agy 終審轉述中的「Sugiyama＋重心排序」不符 codex 原案
  （ordered forest 無需交叉最小化），本檔以 codex 原案為準。
- **R5 定稿審查**（codex `20260810T152927Z-4b2c`；review-discipline
  >150 行門檻的 cross-vendor 輪）：8 blocker＋3 should-fix，全數修入本檔
  ——archive 檔名採「先驗 canonical 再剝前綴」單一轉換、byte-for-byte
  宣稱收窄並明列 archive 副作用、`despawn-stale` 寫入時點分支、斷點
  gate 修為 80↔81、mockup 固定框高／`node 5/7`／終端箭頭補齊、
  「死節點」混稱清除、manual standalone 定為 display-only 不可選、
  懸空 task 改 `possibly removed by gc`。file:line 引用與 R1–R4 歸納
  經其覆核無誤。

## 9. 待使用者裁決（LV0）

即 §6 LV0 rubric 四題；另加兩題方向確認：

1. 鍵位定案 `t`/`y`/`i`（不增 `d`）接受？
2. 批次順序 S1（role+spawn_kind）→ S2（archive）→ S3（task refs）＋
   LV1 P0 首屏先行，接受？

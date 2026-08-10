# TUI lineage 視覺化實作計畫

- 狀態：LV0 已由使用者通過；本檔供繼任 session 逐 phase 派工。
- 規範正本：[tui-lineage-view-proposal.md](tui-lineage-view-proposal.md)。
- 實作順序：LV1 → LV2（S1）→ LV3（先 S2、後 S3）→ LV4 → LV5 → LV6 →
  LV7 → LV8 → LV9。
- 驗證紀律：phase 內依 `docs/testing-policy.md:7-12` 跑 clippy、Cargo tests
  與受影響 shell group；只有 LV9 的無 `TEST_GROUPS` 全套可作收案證據。

查證標記：本文所有「既有／現行／已查證」主張均已於 2026-08-11 對目前工作樹
查證，並在首次使用處附 `file:line` 或同一檔案小節內的行號；標成「新增」的名稱
是本計畫定下的未來實作介面，不冒充現況。本文沒有以「推測」作為實作依據的
主張。行號會隨前一 phase 位移，executor 開工時仍須以函式名重新定位。

## 給繼任者的 orchestration 指引

1. **一個 phase 一次 executor dispatch**。brief 直接附該 phase 全節；不得只
   摘要 gate 而漏掉「不做」邊界。LV3 雖含 S2/S3，仍由同一 executor 依 S2→S3
   分成兩個可獨立 review 的 commit-sized diff；未經使用者要求不得實際 commit。
2. 每 phase executor 自驗後，**收案前固定派一輪獨立 Codex review**，只審
   correctness、契約漂移、fail-closed 與測試有效性。finding 修完重跑該 phase
   gate，review 通過才派下一 phase。
3. 每次派工前先讀 `git status --short` 與該 phase 的精確檔案；只 stage／提交
   明列檔案。工作樹既有變更一律視為使用者資產，不覆寫、不順手整理。
4. shell group 49–51 是新分組。新增分組時同步更新
   `tests/run-tests.sh:131-159` 的 `GRP_KNOWN`；若引用先前分組 fixture，
   同步補 `GRP_NEEDS`。partial run 只能證明該分組。
5. production 不複製 `docs/lineage-prototype/flow_layout.rs`。該檔只驗證
   演算法可行；正式型別、錯誤處理、ratatui render 與 tests 必須在 crate 內重寫。

### 相依與可平行性

- 推薦排程完全序列化：使用者已定 LV1 先行與 S1→S2→S3；LV4–LV6 又共同修改
  `model.rs`／`app.rs`／`view.rs`／`lib.rs`，跨 phase 平行的 merge 風險
  大於收益。
- 純檔案面上 LV1 與 LV2 大致分離（前者 ab-tui、後者 ab/ab-core），但**不建議
  平行**：LV1 gate 是開工後第一個可見切片，LV2 應在它收案後開始。
- LV2 與 LV3 互斥 `main.rs`／`spawn.rs`／`registry.rs`；LV3 的 S2/S3
  又互斥 `evict.rs`／`send.rs`／`task.rs`，必須依序。
- LV4、LV5、LV6 互斥全部 ab-tui 核心狀態／render 檔；LV5 必須吃 LV4 已凍結的
  `LineageProjection`，LV6 必須吃 LV4 的 generation selection。
- LV7 只在 LV1–LV6 unit gates 全綠後進行。LV8 是人工驗收，不與 LV9 平行。
- LV9 內部的 spec、設計正本、demo 三塊可由同一 executor 分段處理，但只在全部
  內容完成後跑一次全套；避免多 executor 同時改文件索引與測試數字。

## DEVIATION（已知正本瑕疵，不得靜默照抄）

1. proposal 第 3 行仍寫「待使用者定案」，但本輪 R6 任務已明示 LV0 通過。本計畫
   依最新使用者裁決把 LV0 視為完成；LV9 回填時才更新正本狀態。
2. proposal 第 334 行把
   `AGENT_BRIDGE_SPAWN_TAG=ab-spawn-p4w05-20260810T143000-5678f9e8c1d2`
   標成 canonical，但 STATE-AGENT-6（`spec/state.md:63-80`）要求 name 後一段
   是純十進位 PID；該例含 `T`，不合法。正式 fixture／spec 必須使用例如
   `AGENT_BRIDGE_SPAWN_TAG=ab-spawn-p4w05-143000-5678f9e8c1d2`。這只修正
   example，不改 generation 文法。

## LV0 — 設計定案（已通過）

### 輸入與檔案

- 不改程式碼；規範輸入只有 `docs/tui-lineage-view-proposal.md`。
- R6 已裁定鍵位 `t/y/i`、不增 `d`，以及 S1→S2→S3＋LV1 先行。

### Human gate（proposal 原文）

**human judgment**（rubric 四題，逐題可由本文證據答是/否，全 yes 才開工）：
(1) root 不冒充手動 orchestrator？(2) unknown edge／gap／archived-ended 三者
在畫面可區分？(3) archive 失敗不改既有 despawn/evict outcome？
(4) 80/81 模式切換與 selection 行為已明定？

### 不做

- 不在 LV0 實作或「順便」重寫 proposal；R6 已把它從決策 gate 轉為 normative
  input。上節 DEVIATION 只留可追溯記錄。

## LV1 — P0 首屏因果補洞

### 目標、相依、可平行性

- 目標：WORKERS 第二行顯示可證的直系 parent/root/unavailable；unattached task
  第二行顯示五類 fail-closed reason。
- 相依：只依現行 lineage 與 attach 判準；不等 S1/S2/S3。
- 排程：第一個實作 phase；與其他 phase 不平行。

### 修改檔案與錨點

- `crates/ab-tui/src/model.rs`
  - 已查證：`attached`（1094）、`worker_of_task`（1109）是 task 歸屬單一
    事實源；`row_height`（1496）是兩行列會計。
  - 新增 `ParentLabel`、`UnattachedReason` 與純函式
    `parent_label`／`unattached_reason`；reason 只能由目前 registry、
    `registered_at`、task `created_at` 與唯一匹配數得出。
- `crates/ab-tui/src/view.rs`
  - 已查證：`worker_second_line`（489）與 `render_tasks`（554）。
  - 將 parent 放在截斷前的高優先區；TASKS 維持兩行，不改權威 status 字。
- 測試留在上述模組的既有 `#[cfg(test)]`，不新增 fixture framework。

### 測試與 gate

- 新測試：
  - `p0_causal_parent_labels_distinguish_root_parent_and_unavailable`
  - `p0_causal_unattached_reasons_cover_all_five_fail_closed_cases`
  - `p0_causal_never_registered_is_never_rendered`
  - `p0_causal_worker_and_task_rows_stay_two_lines`
- `TEST_GROUPS`：既有 44（TUI replay regression）；不新增 shell group。
- Gate：

  ```bash
  cargo test -p ab-tui p0_causal
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=44 tests/run-tests.sh
  ```

### 不做

- 不推測 parent 名、不從 task sender／owner／時間鄰近補線。
- 不寫 `never registered`；registry 缺席只能說 current registry absent。
- 不改首頁 attention 排序、不把 tree 塞回首頁、不動 task 狀態機。

## LV2 — S1：`spawn_kind` 與 explicit `--role`

### 目標、相依、可平行性

- 目標：spawn/relay 在同一 registry 寫入點留下 optional `spawn_kind`；
  `--role` 經明確驗證後原樣留存。
- 相依：LV1 已收案。使用者指定 S1 必須早於 S2/S3。
- 排程：不得與 LV3 平行（共享 CLI parser、spawn registry）。

### 修改檔案與錨點

- `crates/ab/src/main.rs`
  - usage（48–55）、`parse_spawn_args`（1006）、`cmd_relay`（1070）。
  - spawn/relay usage 加 `[--role <slug>]`；relay parser 收 role 並傳進共用
    `SpawnRequest`，不可另寫 registry 路徑。
- `crates/ab-core/src/spawn.rs`
  - 已查證：validator 區（28–59）、`Relay`（143）、`SpawnRequest`（153）、
    `spawn_locked`（710）與 registry atomic-write（約 939–969）。
  - 新增 `is_valid_role`；`SpawnRequest.role: Option<String>`；
    `spawn_locked` 依 `req.relay.is_some()` 寫 `spawn_kind`，有 role 才寫 key。
- `crates/ab-core/src/registry.rs`
  - `AgentSnapshot`（353）與 `snapshot`（389）同一次 parse 補讀
    `spawn_kind`／`role`，缺欄與 invalid 保持可區分。
- `crates/ab/src/main.rs` 與 `crates/ab-core/src/spawn.rs` 的 inline tests。
- `tests/run-tests.sh`：登記並實作 group 49。

### 測試與 gate

- 新測試：
  - `role_slug_accepts_only_the_specified_ascii_shape`
  - `spawn_request_without_role_omits_the_registry_key`
  - `spawn_and_relay_write_distinct_spawn_kinds`
  - `relay_forwards_role_through_the_shared_spawn_path`
  - `legacy_or_invalid_spawn_kind_projects_to_unknown`
- Group 49 黑箱斷言：帶/不帶 role registry golden；invalid role 在任何 pane／
  registry 副作用前失敗；spawn/relay kind；既有 stdout/exit 不變。
- Gate：

  ```bash
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=49 tests/run-tests.sh
  ```

### 不做

- 不從 agent 名、request、runtime 推測 role；不為 manual register 新增 role。
- role/spawn_kind 不進 auth、CAS、cap、排序、回收或 task routing。
- legacy 缺 `spawn_kind` 必須是 unknown，絕不 fallback 成 spawn。

## LV3 — S2/S3：archive、generation refs、evict link

### 目標、相依、可平行性

- 目標：先完成 S2 agent archive/GC，再完成 S3 task generation refs 與 evict
  wrap-up 雙向 link。
- 相依：LV2 全綠；S2 review 通過後才在同一 phase 進 S3。
- 排程：本 phase 檔案高度互斥，不拆成平行 executor。

### 修改檔案與錨點

- `crates/ab-core/src/paths.rs`
  - `Paths`（9）新增 `agent_archive_dir`；`ensure_dirs`（41）補建目錄。
- `crates/ab-core/src/lib.rs:12-28`
  - 新增 `pub mod agent_archive`。
- `crates/ab-core/src/agent_archive.rs`（新增）
  - 定義 `ArchiveRecord`、`ArchiveEndKind`、canonical↔bare 唯一轉換、
    direct read、best-effort atomic archive、strict archive GC。
  - 轉換先呼叫已查證的 `spawn::is_generation_key`（`spawn.rs:578`）；
    任何未通過的 key 不組路徑。
- `crates/ab-core/src/spawn.rs`
  - `DespawnCtx`（1132）、`despawn_locked`（1162）。
  - Killed/Absent 與 Stale 分支各在刪 live registry 前呼叫同一 archive writer；
    stale 強制 `despawn-stale`，archive failure 只 warning。
- `crates/ab-core/src/evict.rs`
  - `EvictRequest`（52）、`audit_event`（198）、`evict`（216）；
    傳 `end_kind`／`wrapup_task_id` 至 `DespawnCtx`，stale 不寫 evicted*。
- `crates/ab-core/src/task.rs`
  - `create_task`（386）接受新增 `GenerationRefs`；metadata 只在 Some 時寫。
  - `InFlight`（709）、`in_flight`（732）、`recent_tasks`（779）同一次
    metadata parse 補讀兩 refs。
  - task `gc`（498）維持原路；archive GC 不混進 task deletion helper。
- `crates/ab-core/src/send.rs`
  - `create_send_task`（22）在不取 registry 鎖下呼叫新增
    `observe_generation_refs`；接受 optional exact target generation，僅供
    已持鎖的 evict 將 `to_generation` 綁定起始 `gen_tag`。
- `crates/ab-core/src/registry.rs`
  - 視需要新增「單檔同次 parse 的 generation observation」helper；不得把
    display refs 放進 CAS。
- `crates/ab/src/main.rs`
  - usage（65–67）、`do_send`（444）、`cmd_gc`（956）與 evict 呼叫；
    新增 `--include-agent-archive`，未帶時 stdout/stderr golden 逐字不變。
- `tests/run-tests.sh`：登記並實作 group 50。

### S2 實作順序

1. canonical↔bare round-trip 與 archive record parser 先紅後綠。
2. `Paths::ensure_dirs` 與 direct lookup。
3. despawn 三結果分支歸檔；故障注入證明原 outcome 不變。
4. opt-in archive GC；只認 regular file、strict filename、version 1、
   可解析 `archived_at`，不確定一律保留。

### S3 實作順序

1. `GenerationRefs` metadata writer/reader。
2. normal send 的 create-time observation；0/invalid/ambiguous 一律省略。
3. evict exact target ref 與 archive `wrapup_task_id`。
4. missing task 只投影 `task missing (possibly removed by gc)`。

### 測試與 gate

- 核心測試：
  - `archive_filename_round_trips_only_canonical_generation_keys`
  - `ensure_dirs_creates_agent_archive_directory`
  - `despawn_archives_killed_absent_and_stale_with_exact_end_kind`
  - `archive_failure_warns_without_changing_despawn_outcome`
  - `archive_gc_is_opt_in_strict_and_fail_closed`
  - `task_generation_refs_are_create_time_optional_provenance`
  - `send_does_not_lock_or_guess_ambiguous_generation_refs`
  - `evict_links_wrapup_task_and_archive_to_the_same_generation`
- Group 50 另驗：root→A→B 歸檔 A 後 direct lookup；live wins；不帶新 gc flag
  的既有 golden byte-identical。
- Gate：

  ```bash
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=50 tests/run-tests.sh
  ```

### 不做

- 不掃完整 archive 重建歷史旁支、不 backfill legacy、不新增 `ready_at`。
- generation refs 不作 routing/auth/CAS；normal send 不為此無條件取 registry 鎖。
- archive 失敗不阻止回收；archive GC 不跟 task GC 共用寬鬆 filename 判準。
- 不把 handoff path、relay depth/hop 寫進任何 schema。

## LV4 — `LineageProjection` 與 compact renderer

### 目標、相依、可平行性

- 目標：從 live snapshot＋按 canonical parent direct-read 的 archive 建唯一
  projection；完成 ≤80 欄 compact、TreeKey 與跨 reload selection。
- 相依：LV3 archive/ref API 已凍結。
- 排程：與 LV5/LV6 互斥；先把 projection API review 定案。

### 修改檔案與錨點

- `crates/ab-tui/src/lineage.rs`（新增）
  - 定義 `LineageProjection`、`LineageNode`、`LineageEdge`、
    `EdgeKind`、`TreeKey::Generation`、`CompactRow`。
  - `build_projection` 只從 live generation 起步，沿 parent direct lookup
    archive；cycle/duplicate/invalid 進 standalone/invalid band，gap 與 missing
    明文且不可選。
  - `compact_layout` 產固定兩行 generation/manual row；gap/tombstone 是單行
    decoration；DFS order 由 projection 唯一產生。
- `crates/ab-tui/src/lib.rs:13-18` 加 `mod lineage`。
- `crates/ab-tui/src/model.rs`
  - `Model`（23）／`Model::load`（42）持有 projection；沿用
    `tag_display_parts`（539）只作 display。
- `crates/ab-tui/src/app.rs`
  - `App`（244）新增 dashboard/lineage mode、TreeKey、DFS index；
    `handle_key`（508）接 `t/j/k/h/l/Esc` 並以 generation stable key relocate。
- `crates/ab-tui/src/view.rs`
  - `render`（158）依 mode 分流；新增 `render_lineage_compact` 與 contextual
    footer/help，不改 dashboard renderer。
- `crates/ab-tui/src/lib.rs`
  - `event_loop`（81）、`refresh_disk`（408）在 reload 後以 TreeKey relocate。

### 測試與 gate

- 新測試：
  - `lineage_projection_active_archive_gap_tombstone_legacy_duplicate_cycle`
  - `lineage_projection_never_uses_task_sender_owner_or_name_to_invent_edges`
  - `lineage_projection_dfs_order_is_deterministic`
  - `lineage_projection_manual_gap_and_missing_are_not_selectable`
  - `lineage_projection_compact_rows_are_fixed_height`
  - `lineage_projection_79x24_and_80x24_match_goldens`
  - `lineage_projection_reload_keeps_tree_key_or_relocates_to_nearest_generation`
- `TEST_GROUPS`：既有 44 regression；新 group 51 留到 LV7 真 tmux 整合。
- Gate：

  ```bash
  cargo test -p ab-tui lineage_projection
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=44 tests/run-tests.sh
  ```

### 不做

- 不做 flow/camera、timeline/info、collapse、free-pan 或 zoom。
- 不掃全 archive；manual standalone 只展示、不給虛構 generation key。
- 不以 blocker severity 改 projection canonical order。

## LV5 — 2D flow、SceneGeometry 與 camera

### 目標、相依、可平行性

- 目標：>80 欄以 O(V+E) layered forest render；同一 `SceneGeometry` 供
  render、navigation、camera、scrollbars。
- 相依：LV4 projection/DFS/TreeKey API review 已通過。
- 排程：與 LV4/LV6 互斥。

### 修改檔案與錨點

- `crates/ab-tui/src/lineage.rs`
  - 新增 `Rect`、`EdgeSegment`、`CanvasBounds`、`SceneGeometry`、
    `FlowLayout`；實作 span/interval/root band/terminal edge-kind 演算法。
  - generation box 固定 `BOX_W`、框內三行、總高 5；先畫 edge 後畫 box。
- `crates/ab-tui/src/app.rs`
  - App 新增 camera offset；selection 變更後只做最小位移，不提供 free-pan。
- `crates/ab-tui/src/view.rs`
  - 新增 `render_lineage_flow`、兩軸 scrollbar、canvas/offset title；
    selection 用 `▶`＋既有 selected background，外層 focused panel 才用 Thick。
- `crates/ab-tui/src/theme.rs`
  - 僅在需要時新增 edge/archived 的 additive style mapper；字元層與 ANSI/truecolor
    無關，不新增語意色軸。

### 測試與 gate

- 新測試：
  - `lineage_flow_rects_do_not_overlap_and_parent_is_left_of_child`
  - `lineage_flow_root_bands_do_not_overlap`
  - `lineage_flow_edges_do_not_cross_boxes_and_only_terminal_segments_encode_kind`
  - `lineage_flow_is_deterministic_and_cycle_bounded`
  - `lineage_flow_selection_follow_keeps_full_rect_visible`
  - `lineage_flow_120x40_and_81x24_match_goldens`
  - `lineage_flow_resize_80_to_81_keeps_tree_key_and_node_edge_sets`
- `TEST_GROUPS`：既有 44 regression；group 51 於 LV7。
- Gate：

  ```bash
  cargo test -p ab-tui lineage_flow
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=44 tests/run-tests.sh
  ```

### 不做

- 不引入 graph/layout/snapshot crate、Mermaid subprocess、Kitty graphics。
- 不做 Sugiyama/force、cross-edge、line wrap-back、zoom、free-pan、collapse。
- 不把預設 spawn cap 4 或 relay depth 10 當 canvas 硬界。

## LV6 — `y` timeline 與 `i` generation info

### 目標、相依、可平行性

- 目標：選中 generation 開兩個唯讀 overlay；task plane 與 derivation plane
  分區，所有檔案讀取在取得路徑有 record/byte 上限。
- 相依：LV4 selection/projection；LV3 refs/archive。可不依 LV5 幾何，但因共享
  app/view/lib，排程在 LV5 後。

### 修改檔案與錨點

- `crates/ab-core/src/task.rs`
  - 已查證：`request_first_line`（142）、`last_event_line`（177）已有 bounded
    單行模式；新增 `event_lines_bounded`，從檔尾/限定 bytes 取得最多 N records，
    切入半行丟棄，不整檔讀後再截。
- `crates/ab-tui/src/lineage_detail.rs`（新增）
  - 定義 `TimelineSeed`／`TimelineEvent`／`GenerationInfo` 與純 formatter。
  - raw events 與 status projection 分開；ready 僅 bool；missing wrap-up 原因不斷言。
- `crates/ab-tui/src/lib.rs:13-18` 加 module；`event_loop`（81）接 overlay result。
- `crates/ab-tui/src/worker.rs`
  - 已查證：`Handle::spawn_oneshot`（118）與 `Msg`（46）可共用同一收信口；
    新增帶 TreeKey 的 timeline terminal message，晚到且 selection 已換代就丟棄。
- `crates/ab-tui/src/app.rs`
  - `Effect`（175）、`App`（244）、`dispatch_key`（514）接 `y/i`；
    confirm overlay 先吃 `y=yes`，故與 lineage timeline 情境互斥。
- `crates/ab-tui/src/action.rs`
  - 既有 `info_page`（137）改為 dashboard wrapper；新增 generation info builder，
    只消費同一份 projection/recent snapshot。
- `crates/ab-tui/src/view.rs`
  - `render` overlay 優先序（193–207）加入 timeline/info；支援垂直捲動與
    `older events omitted by read limit`。

### 測試與 gate

- 新測試：
  - `lineage_timeline_orders_raw_events_without_inventing_completed_events`
  - `lineage_timeline_ready_has_no_timestamp`
  - `lineage_timeline_event_reader_is_record_and_byte_bounded`
  - `lineage_timeline_missing_archive_and_wrapup_are_explicit`
  - `lineage_timeline_late_result_cannot_attach_to_another_generation`
  - `lineage_info_keeps_inbound_outbound_and_derivations_separate`
  - `lineage_info_never_renders_hop_handoff_or_parent_from_task_sender`
- `TEST_GROUPS`：既有 44 regression；group 51 於 LV7。
- Gate：

  ```bash
  cargo test -p ab-tui lineage_timeline
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=44 tests/run-tests.sh
  ```

### 不做

- 不補 `ready_at`，不顯示 relay hop/handoff path，不把 relay 當 task。
- 不做全 pool lifecycle swimlane、不讀無界 events/archive、不新增 tmux 查詢。
- `i` 只列 loaded/bounded task window；截限必明示，不能暗示完整歷史。

## LV7 — CLI/TUI 真 tmux 整合

### 目標、相依、可平行性

- 目標：以真 tmux 驗證 `t/y/i`、80/81 模式、返回 stable selection 與零 mutation。
- 相依：LV1–LV6 unit/review 全部收案。
- 排程：單獨進行；失敗回到擁有該邏輯的 phase 修，不在 integration test 裡補規則。

### 修改檔案與錨點

- `tests/run-tests.sh:131-159`
  - 登記 group 51、必要時補 `GRP_NEEDS`；新增 isolated data dir＋tmux socket fixture。
- `crates/ab-tui/src/app.rs`／`view.rs`／`lib.rs`
  - 只允許 integration gate 揭露的 wiring 修正；不得在此重設 projection/layout。

### 測試與 gate

- Group 51 cases：
  - 120×40 進 flow、81×24 仍 flow、80×24/79×24 進 compact。
  - 80↔81 前後 generation/edge 集合與 TreeKey 相同。
  - `t` 返回 dashboard 映回同 `(name, spawn_tag)`；generation 消失時走既定
    nearest relocation。
  - `y/i` 只在 selectable generation 出現；manual/gap/missing footer 不宣稱可用。
  - 進出 view/overlay 前後 task status、metadata、registry、agents.log checksum
    不變；唯一例外是既有 Enter focus 的 tmux selection。
- Gate：

  ```bash
  cargo clippy --all-targets -- -D warnings
  cargo test
  TEST_GROUPS=51 tests/run-tests.sh
  ```

### 不做

- 不在 group 51 依賴牆鐘競速或網路；fixture 時刻固定。
- 不把 partial run 當整體收案，不趁整合階段新增 mutation 或新鍵。

## LV8 — 真實終端理解驗收

### 目標、相依、檔案

- 相依：LV7 全綠並經獨立 review。
- 不改任何檔案；使用真實 79/80/81 或 80/81/120 三寬 session，固定同一組資料。
- 受測者未通過時記下錯答與畫面證據，回到相應 phase；不得現場改 rubric。

### Human gate（proposal 原文）

**human judgment**（rubric 四題，60 秒 4/4 且指出畫面字面證據）：
(1) blocker 的直系 parent 是誰？(2) 哪條邊是 relay、哪條 unknown？
(3) 哪一代 missing、哪一代 archived-ended？
(4) 選中 worker 的 wrap-up task 如何結束？

### `TEST_GROUPS`

- 無；本 phase 是 human judgment，不能用 group 51 代替。

### 不做

- 不提示答案、不把 task sender 當 parent、不因受測者困惑臨時加 zoom/collapse。

## LV9 — 規格回填、demo、全回歸收案

### 目標、相依、可平行性

- 目標：把已驗證行為回填正本、重錄 demo、跑全 workspace＋全 shell suite。
- 相依：LV8 4/4。
- 排程：最後單一收案 phase。

### 修改檔案與錨點

- `spec/state.md`：合併
  `docs/tui-lineage-view-spec-draft.md` 的 STATE-AGENT-7/8/9、STATE-TASK-6。
- `spec/cli.md`：合併 CLI-SPAWN-8/9、CLI-RELAY-5/6、CLI-DESPAWN-4、
  CLI-EVICT-5/6、CLI-SEND-4、CLI-GC-4、CLI-UI-3/4/5。
- `spec/traceability.md`：合併 groups 49–51 三列，更新 untested 計數。
- `docs/tui-design.md`：把 proposal §2–§7 濃縮成新 phase rows 與驗收紀錄；
  保留失敗史，不改寫舊量測。
- `docs/rust/architecture.md`：補 `agent_archive`、
  `ab-tui::lineage`／`lineage_detail` 模組責任。
- `docs/demo/ui-driver.sh`、`docs/demo/ui.tape`：建立同一 fixture，錄
  dashboard→flow→timeline→compact 敘事。
- `docs/assets/ui.gif`：由 VHS 重新生成；不手改二進位。
- `docs/tui-lineage-view-proposal.md`：狀態改為已定案／已落地，附 phase gate
  實測數字；不刪 R1–R5 audit。

### 測試與 gate

- `TEST_GROUPS`：49、50、51 已各自通過；收案必跑無 filter 全套。
- Gate（proposal 原文）：

  ```bash
  cargo clippy --all-targets -- -D warnings && cargo test && tests/run-tests.sh
  ```

- Demo gate：

  ```bash
  vhs docs/demo/ui.tape
  test -s docs/assets/ui.gif
  ```

- 收案報告必附 Cargo test 數、shell `PASS/FAIL` 數、無 `PARTIAL RUN`、
  LV8 4/4 與 VHS 結果。若環境沒有 VHS，這是 blocker，不得宣稱 LV9 完成。

### 不做

- 不新增相容層、migration/backfill、外部 graph renderer 或新依賴。
- 不修改既有 task/registry payload 的非 additive 欄位，不重寫歷史 audit。
- 不把 prototype source 納入 crate，也不把 proposal 刪掉取代 audit trail。

## 每 phase 回報模板

1. 結果：完成／blocked；對應 phase。
2. 修改檔案：精確路徑。
3. 契約：哪些 MUST 已落地；哪些「不做」已以負向測試錨住。
4. 驗證：完整命令、PASS/FAIL、是否 partial。
5. Review：獨立 Codex finding 與修正結果。
6. 未解事項：無則明說；有則使用 `user clarification needed here`，不得猜。

# spawn 落點改版：`--here` 與「同任務同 window」預設（雛形）

狀態：**P3 契約化完成、P4 審查修復中**——spec（CLI-SPAWN-1/5、
CLI-RELAY-1/3、ENV-HERE-1、ENV-TAG-1）與測試（分組 32 修訂＋32j–32q）
已落地。裁決紀錄與審查 verdict 附文末。

## 動機

worker 預設落在 per-owner worker window（CLI-SPAWN-5）保住了「多 worker
不擠爆 orchestrator 視窗」，但代價是**人看不到 worker 在幹活**——README
主打的「可觀察、可介入」（裁判準則第 2 條）在預設路徑上要多按一次
`Ctrl+b n` 才成立。demo GIF 重錄時這一點成了實際痛點（worker 側動作
全程不入鏡，劇本被迫加切窗）。使用者的心智模型是「同一個任務的 panes
留在同一個 window」：主 session 大左、worker 直疊右（`main-vertical`）。

## 設計

### 三種落點模式

| 模式 | 旗標 | 落點 |
| --- | --- | --- |
| here | `--here` | 切進呼叫者當前 window，split 後套 layout（下述） |
| window | `--window` | 專屬視窗（現行為，不變） |
| auto | （預設） | 見下 |

`--here` 與 `--window` 同時給 MUST 在建 pane 前拒絕（CLI-SPAWN-2 的
precheck 不變量）。

### auto 預設的解析規則

1. **呼叫者是人工 session**（環境無 `AGENT_BRIDGE_SPAWN_TAG`，與
   CLI-RELAY-4 盯守提醒同一訊號）且在 tmux 內 → **here**。
   這是本次的行為翻轉：人手邊的任務，worker 就留在人看得到的 window。
2. **呼叫者是 spawn 出身**（tag 在環境中）→ 維持 per-owner worker
   window。理由：第三層委派的 fan-out 若也走 here，會層層堆進人類的
   window——worker 自己的視窗沒有人在看，整齊比可見重要。
3. **tmux 外呼叫** → 現行 fallback（切當前視窗）不變。

### here 的 split 與 layout

- `split-window -dP -t <owner_win>`，成功後
  `select-layout -t <owner_win> $AGENT_BRIDGE_HERE_LAYOUT`。
- `AGENT_BRIDGE_HERE_LAYOUT` 預設 `main-vertical`（主 pane 靠左全高、
  worker 直疊右）；合法值 `main-vertical|main-horizontal|tiled|
  even-vertical|even-horizontal|none`（`none`＝只 split 不重排）。
  非法值 MUST 在建 pane 前拒絕（fail-closed）；**空字串與非 UTF-8 也是
  非法值**，不套 `:-` 空值退預設慣例（codex plan 審查 R4）。
- layout 驗證 MUST 只在 here 路徑執行：`--window`／spawn 出身／tmux 外
  用不到 layout，無關壞值不得封鎖這些逃生口。
- 顯式 `--here` 在 tmux 外（無可解析呼叫者）：退回現行 untargeted
  current-window split fallback，不視為錯誤。
- layout 失敗不致命（pane 已落地、註冊照走）——與現行 tiled 重排同級。
- 已知取捨：每次 here-spawn 會重排呼叫者 window 的既有 panes。這是
  刻意行為（要的就是那個版面）；不要重排的人設 `none`。

### 信任根與生命週期不受影響（設計不變量）

- here 落點**不寫 `@ab_owner`**：呼叫者自己的 window 不是 bridge 管的
  worker window，不建立沿用語意；`find_worker_window` 與 reuse 路徑
  完全不碰。registry 的 worker window 欄留空（同 `--window`）。
- 回滾與 despawn 都以 pane 為單位（spawn tag 驗身），不依賴「sole-pane
  window 隨 pane 消失」——here pane 被 kill 時呼叫者 window 仍在，語意
  正確。
- spawn cap、審計、ready 探針、brief 注入全部共用既有路徑，零改動。
- relay 共用同一套落點解析（CLI-RELAY-1「與 spawn 完全一致」維持）：
  人工 session relay 的接棒者也落在當前 window。`--no-select` 的契約是
  「不改 active pane」而非「畫面完全不變」——here 的 layout 重排可能
  改變版面（codex plan 審查意見 B）。

### 相容性影響

- 行為翻轉只發生在「人工 session 在 tmux 內的預設 spawn」；spawn 出身
  呼叫者與 tmux 外呼叫者的行為不變。要舊行為的人有 `--window`。
- `pane-border-status top` 現在會設在呼叫者的 window 上（讓 worker pane
  有標題可辨）——文件明示，可自行 unset。

## Phases × Gates

| Phase | 內容 | Gate |
| --- | --- | --- |
| P1 設計雛形 | 本文件 | human judgment（rubric 如下）＋codex plan 審查 verdict 落檔 |
| P2 prototype | `--here`＋auto 預設實作 | `cargo check`＋`cargo clippy` 零錯誤；手動 tmux smoke：here 落點、layout、despawn 乾淨 |
| P3 契約化 | spec/cli.md（CLI-SPAWN-5 改寫）、spec/env.md、測試新增／修訂 | `bash tests/run-tests.sh` 全 PASS（含新 case） |
| P4 跨廠審查 | codex pane worker 審 diff | findings 全數 resolved 或升級 blocker |
| P5 文件＋GIF | README 雙語、demo tape 改版重錄 | 抽幀驗證：錄影內容與 README 主張一致 |

### P1 rubric（每條可依證據答是／否）

1. auto 規則對三種呼叫者（人工／spawn 出身／tmux 外）各給出唯一落點，
   且沒有任何預設路徑會把第三層 fan-out 堆進人類 window。
2. here 落點不新增任何寫在使用者自有 window 上的信任根（@ab_owner 不
   寫），reuse／confused-deputy 語意與現行完全相同。
3. 回滾、despawn、cap、審計在 here 落點下不需要新分支即語意正確。
4. `--here`＋`--window` 衝突與非法 layout 值都在建 pane 前拒絕。
5. 行為翻轉的影響面（誰變、誰不變、如何退回）在文件中明示。

## 裁決紀錄

- 2026-08-04 使用者裁決：路線採 `--here` 一等公民＋預設翻轉（「同個任務
  維持在一個 window」）；由本 session 協調規劃與驗收，獨立 codex worker
  審查；完成後重錄 GIF 作主打展示。
- auto 規則第 2 條（spawn 出身呼叫者不翻轉）為本文件的架構補充，動機是
  堵第三層 fan-out 灌爆人類 window 的 footgun；待 codex plan 審查與
  使用者過目。
- 2026-08-04 codex plan 審查（task 20260804T144125Z-ea2e）：rubric
  R1/R2/R3/R5 過；R4 CONFIRMED——`AGENT_BRIDGE_HERE_LAYOUT=''` 曾被
  `classify` 的 `:-` 慣例歸為 unset 而靜默退預設。已修：`here_layout()`
  改走獨立解析（空字串／非 UTF-8 致命）＋單元測試；layout 驗證移到只在
  here 路徑執行（審查建議 1）。意見採納：tag 判準定位為 **placement
  provenance hint、不是身份授權**（誤判後果止於版面 UX，`--here`／
  `--window` 可顯式修正，不為此新增信任根）；`main-vertical` 預設保留，
  reflow 侵入性明示於文件、`none` 為逃生口；「僅首次 here-spawn 套
  layout、之後只 split 不全域重排」記為 backlog 優化。CLI-SPAWN-5 改寫
  草案（審查意見 D）作為 P3 藍本。
- 2026-08-04 P3 執行者複核 finding：顯式 `--here` 在 tmux 外曾仍先驗
  layout（`use_here` 不看 owner 可否解析），與本文件「tmux 外不得被無關
  壞值封鎖」矛盾。已修：`use_here` 改以 `owner_win` 可解析為前提，tmux 外
  一律退回 fallback、不驗 layout；補測試 32q 釘住此格。
- 2026-08-05 P4 codex 驗收審查（task 20260804T153154Z-830e）：verdict
  CHANGES REQUIRED、4 CONFIRMED，全數修復——C1 人工判準三態不一致（relay
  提醒改共用 `config::caller_is_manual`，三態＝unset／空為人工、非 UTF-8
  視為在場；ENV-TAG-1／CLI-RELAY-4／CLI-SPAWN-5 措辭明文化）；C2 測試 32j
  的 @ab_owner 負斷言空包（改頂層查詢＋查詢失敗即 FAIL，32k 補同構斷言）；
  C3 32o 坍縮 tmux 外（改 pane 內執行，真正驗到 --window 與 tag-present
  不解析 layout）；C4 relay 契約 traceability（CLI-RELAY-1/3 標
  [tested: 23, 32]）＋32p 補 active-pane 斷言。修復後全套件
  1205 PASS、0 FAIL，check-contract 4/4。per-diff 審查回合已用畢。

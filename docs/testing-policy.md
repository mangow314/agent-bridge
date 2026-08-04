# 測試執行政策（testing policy）

（2026-08-03 立案。動機：session 審查實測，phase 內每個 commit 都跑全套
shell 測試曾單次吃掉 168 分鐘 wall-clock，是 commit 週期最大的單一流程項；
全套因獨立 tmux socket 限制必須序列化、協調者獨佔。）

## 分級規則

| 時機 | 必跑 | 說明 |
|---|---|---|
| phase 內 iteration／commit | `cargo clippy --all-targets -- -D warnings`＋`cargo test`＋`TEST_GROUPS=<受影響分組> tests/run-tests.sh` | 受影響分組＝本 diff 動到的行為所屬分組＋新增分組；partial run 輸出會自帶 `⚠ PARTIAL RUN` 標記 |
| 收案（phase 批收尾）／merge／cutover | 上述全部＋**全套** `tests/run-tests.sh`（不帶 `TEST_GROUPS`） | partial run 的輸出**不得作為收案／merge 證據**——收案宣稱必附全套數字 |
| bash 正本對照（rollback 期） | `SRC_KIND=bash BRIDGE=$PWD/bin/agent-bridge.bash tests/run-tests.sh` | 只在動到 bash 凍結面或收案時跑 |

## TEST_GROUPS 機制

- `TEST_GROUPS=44,45 tests/run-tests.sh`：逗號分隔分組 id（`8a`、`34.5+` 這類
  後綴形照原樣寫）。未知 id 直接 exit 1，不跑任何分組。
- 選中組的**已知靜態跨組依賴**（`GRP_NEEDS` 表）會自動拉入；該表由靜態審計
  產出（變數＋函式的「定義組≠使用組」掃描），新分組引用他組狀態時必須同步
  補表。漏補的失敗形態是選中組轉紅（fail-visible），不會假綠。
- 共用 setup（shim、假 runtime、測試 tmux server）永遠執行，不受過濾影響。

## 紀律

- 收案／merge 宣稱一律引用全套輸出；看到 `⚠ PARTIAL RUN` 字樣的輸出只能
  支持「該分組行為」的主張。
- 審查 worker（codex sandbox 擋 tmux socket）不能重跑本套件：套件數字對
  審查者永遠是 maker claim，此限制記錄於 `share/review-overlay.md`。

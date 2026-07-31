# agent-bridge 介面契約規格

本目錄是 agent-bridge 的**介面契約正本**，也是 Rust 實作
（ab-core/ab/ab-tui，M4 起為正本）與 bash 正本（`bin/agent-bridge.bash`，
rollback 基準）之間的 parity 判準：兩邊都必須逐條滿足這裡的條款，並以同一套
測試套件（`tests/run-tests.sh`）全綠作為黑箱驗收。

## 檔案

| 檔 | 協定面 |
|---|---|
| `cli.md` | 21 個子指令：argv 入 → stdout/stderr/exit code/檔案系統效果 出 |
| `hooks.md` | hook 協定：stdin 事件 JSON、block 回應、exit 0 鐵律、owner gate |
| `state.md` | 磁碟狀態：agents/ tasks/ state/ locks/ 四類檔案格式與寫入語意 |
| `env.md` | 14 個 `AGENT_BRIDGE_*` 環境變數：預設值與壞值行為 |
| `traceability.md` | 測試分組 ↔ 條款對照表、`[untested]` 缺口清單 |

## 條款規則

- **ID 格式**：`面-主題-序號`（如 `CLI-SEND-3`、`HOOK-OWNER-2`）。ID 穩定、
  只增不改號；廢止條款標 `[withdrawn]` 保留原文，不刪除。
- **黑箱措辭**：條款只描述可觀察行為（argv/stdin/環境 → stdout/exit code/
  檔案效果），MUST / MUST NOT 語氣。實作細節（jq、flock、bash 慣用法）
  不入條款；必要背景放 `Note:`。判準：Rust 重寫後這句話仍逐字成立。
- **Source 錨點**：引 bash 正本的**函式名**，不用行號（行號會漂移）。M4
  cutover 後 bash 正本改名為 `bin/agent-bridge.bash`；這些名字仍是有效錨點：
  Rust 側的對映集中在 `docs/rust/architecture.md` §2（模組 ↔ spec 域 ↔ bash
  函式群），行為模組另在 doc comment 標了局部錨點（如 `ab-core/src/hook.rs`、
  `notify.rs`）。條款因此不隨實作語言重寫。
- **測試對映**：每條標 `[tested: <分組號>]`（分組級，非逐條 assert）或
  `[untested]`（缺口清單與處置見 traceability.md）。新增條款先標 `[tbd]`，
  在 traceability 盤點時定案。traceability 表「條款」欄的語意是**完整列舉
  該分組直接斷言的條款**（間接觸及不列）——漏列即缺口，不是「主要關聯」。
- **強度不可放寬**：對應 exact-count argv 斷言（`argv.txt` NUL 分隔全集比對）
  的條款，規範句必須保留「完整參數集合恰好是什麼」的強度，並明標「不可放寬」。

## 機器核對

`tests/check-contract.sh` 交叉核對 spec 與實作／測試套件的集合一致性
（env 變數集合、cmd_* 全覆蓋、hook 函式與事件覆蓋、分組引用完整性）。
它是 grep/awk 級的形狀檢查，不解析語意；語意正確性由測試套件與人工複核把關。

## 既有規格文件的地位

`share/worker-brief.md`、`share/successor-brief.md`、
`share/claude-worker-hooks.json` 是行為正本的一部分（brief 策略不變量受
測試分組 27 保護；hooks.json 三事件裸命令不變量受分組 16 保護）——spec
**引用不搬家**，避免雙正本漂移。

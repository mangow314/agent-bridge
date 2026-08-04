# docs — 工程日誌索引

> *An engineering log, mostly in Traditional Chinese. These are working notes
> and measurements, not a user manual — for usage see the top-level
> [README.md](../README.md); the authoritative interface contracts are in
> [spec/](../spec/).*

這裡不是使用說明，是**工程記錄**：量測數據、設計取捨、以及當時判斷錯了又被
推翻的過程。留著是因為本專案的主張（例如「hook 優先、send-keys 只是 fallback」
「刮螢幕的分野在可不可稽核」）都該有憑據可查，而不是宣稱。

行為正典在 [spec/](../spec/)（機器核對）與 `tests/run-tests.sh`；本目錄的文件
一旦與它們衝突，以正典為準。

## 一手量測（現行參考）

跨 runtime 的實測記錄，只記量到的事實與當下判定。

| 檔 | 量的是什麼 | 受測版本／日期 |
|---|---|---|
| [agy-probe.md](agy-probe.md) | agy（Antigravity CLI）作為第三個 runtime 的能力與限制（無 hooks → 通知恆走 send-keys） | 2026-07-31 |
| [codex-hooks-probe.md](codex-hooks-probe.md) | codex 的 hook 內容雜湊信任機制（未受信任＝靜默略過）；後段補測 node launcher 造成的行程樹多一層 | codex-cli 0.145.0（2026-07-28）／補測 0.146.0（2026-07-31） |
| [herdr-probe.md](herdr-probe.md) | herdr 的狀態語意與偵測機制；本輪實測推翻了先前只看文件時的判斷 | herdr 0.7.5 / 2026-07-31 |
| [nested-runtime-canary.md](nested-runtime-canary.md) | owner gate 的三岔路語意在**真實 runtime** 上成不成立 | 2026-07-29 |
| [notify-native-canary.md](notify-native-canary.md) | 通知原生化整條 hook 鏈在真的 Claude Code session 上成不成立 | 2026-07-28 |

## 架構與流程（現行）

| 檔 | 內容 |
|---|---|
| [rust/architecture.md](rust/architecture.md) | Rust 側怎麼組織（ab-core／ab／ab-tui）。**結構錨**；行為疑義一律回 spec |
| [rust/flow-spawn.md](rust/flow-spawn.md) | spawn 生命週期：cap／ready 探針／原子回滾／出身防護 |
| [rust/flow-notify.md](rust/flow-notify.md) | 通知鏈：send → notify_or_defer → hook 的決策全圖 |
| [rust/flow-ttl.md](rust/flow-ttl.md) | state 通道 TTL：單一寫者／owner gate／通知端新鮮度三視角 |
| [testing-policy.md](testing-policy.md) | 測試執行政策：哪些情形跑全套、哪些跑分組 |

## 進行中

| 檔 | 狀態 |
|---|---|
| [tui-design.md](tui-design.md) | `agent-bridge ui` 的設計正本。**待定案**——含各輪驗收通過與**未通過**的記錄 |

## 歷史決策記錄

已執行完畢的提案與計畫。留著是為了回答「當初為什麼這樣設計」，不是現行說明；
落地形狀一律以 `spec/` 與 `docs/rust/architecture.md` 為準。

| 檔 | 結果 |
|---|---|
| [agent-spawn-proposal.md](agent-spawn-proposal.md) | spawn 的範圍與設計題盤點（2026-07-22）→ 已採納 |
| [spawn-plan.md](spawn-plan.md) | 依上案的實作計畫（第三輪）→ 已實作 |
| [orchestrator-plan.md](orchestrator-plan.md) | orchestrator 層實作計畫 → 五個 phase 全數完成，含真鏈驗收記錄 |
| [codex-worker-approval-proposal.md](codex-worker-approval-proposal.md) | codex worker 的核准放寬（agent-worker profile）→ 已落地驗收 |
| [owner-gate-boundary-assessment.md](owner-gate-boundary-assessment.md) | owner gate 兩個殘餘窗的評估（2026-07-29）→ 結論：關閉手段留給 Rust 期 |
| [rust/m5-proposal.md](rust/m5-proposal.md) | M5 兩個殘餘窗的關閉手段 → 已實作；經跨廠複核後其中一條被推翻 |

## 其他

- [demo/](demo/) — 頂層 README 那張 gif 的錄製腳本（stub runtime，不打真實 API）
- [assets/](assets/) — 圖檔

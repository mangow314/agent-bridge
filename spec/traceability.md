# 測試分組 ↔ 契約條款對照

- 對映粒度：**分組級**（不逐條 assert；parity 判定單位是分組全綠）。
- 本表是對映正本；`tests/run-tests.sh` 各分組標頭下的 `# spec:` 註解是
  唯讀鏡像，漂移時以本表為準。
- untested 計數：2（缺口處置見文末）。
- 機器核對：`tests/check-contract.sh 4`。

| 分組 | 主題 | 條款 |
|---|---|---|
| 1 | register / list | CLI-REGISTER-1, CLI-LIST-1, STATE-AGENT-1, STATE-AGENT-3, STATE-GEN-1, ENV-DATA-1 |
| 2 | send 錯誤路徑 | CLI-SEND-2, CLI-SEND-3, CLI-GEN-1, STATE-GEN-3 |
| 3 | 未知 task 的 receive/status/read | CLI-GEN-1, CLI-GEN-3, CLI-RECEIVE-1, CLI-STATUS-1 |
| 4 | send 快樂路徑＋read 於未 completed | CLI-SEND-1, CLI-GEN-2, CLI-RECEIVE-1, STATE-TASK-1, STATE-TASK-2, STATE-TASK-3 |
| 5 | reply 非法轉換 | CLI-REPLY-1, STATE-TASK-4 |
| 6 | 特殊字元 byte-for-byte 保真 | CLI-SEND-1 |
| 7 | reply / read / 重複 reply | CLI-REPLY-1, CLI-READ-1, STATE-TASK-4 |
| 8 | 通知失敗路徑（pane 已死） | CLI-SEND-3 |
| 8a | 權限對話框時不得送鍵 | HOOK-NOTIFY-2, ENV-NOTIFY-1 |
| 8b | 鎖失敗路徑 | STATE-LOCK-1 |
| 9 | unregister | CLI-UNREGISTER-1 |
| 10 | start | CLI-START-1 |
| 11 | fail | CLI-FAIL-1 |
| 12 | cancel | CLI-CANCEL-1 |
| 13 | await 等待終態 | CLI-AWAIT-1, CLI-AWAIT-2, ENV-POLL-1 |
| 14 | tmux 完整 round-trip | CLI-SEND-1, CLI-RECEIVE-1, CLI-REPLY-1 |
| 15 | 併發壓測 | STATE-TASK-1 |
| 16 | spawn 核心＋cap＋原子回滾 | CLI-SPAWN-1, CLI-SPAWN-2, CLI-SPAWN-3, CLI-SPAWN-4, ENV-SPAWN-1, ENV-HOOKS-1, ENV-PASS-1, ENV-TAG-1, HOOK-BIND-1, STATE-AGENT-1 |
| 17 | ready／探針 | CLI-READY-1, ENV-READY-1, ENV-READY-2 |
| 18 | despawn＋出身防護 | CLI-DESPAWN-1 |
| 18b | 出身證據＝啟動指令的 tag | CLI-DESPAWN-2, ENV-TAG-1 |
| 18c | tmux 失敗 ≠ pane 已消失 | CLI-DESPAWN-3 |
| 19 | 出身防護 TOCTOU 與回滾 | CLI-SPAWN-2, CLI-DESPAWN-2, STATE-TASK-5 |
| 20 | 損壞 registry fail-closed | STATE-AGENT-2, CLI-SPAWN-3 |
| 20b | readiness 參數建 pane 前驗 | CLI-SPAWN-2, ENV-READY-1, ENV-READY-2 |
| 21 | 解鎖失敗不得靜默 | STATE-LOCK-2 |
| 22 | worker brief 注入 | ENV-BRIEF-1, CLI-SPAWN-7 |
| 23 | relay 交棒 | CLI-RELAY-1, CLI-RELAY-2, CLI-RELAY-3, ENV-DEPTH-1, ENV-DEPTH-2, ENV-BRIEF-2 |
| 24 | disposable | CLI-DISPOSABLE-1 |
| 25 | idle 決策視圖 | CLI-IDLE-1, CLI-IDLE-2 |
| 26 | evict 三段式 | CLI-EVICT-1, CLI-EVICT-2, CLI-EVICT-3 |
| 27 | brief 正本策略不變量 | CLI-SPAWN-7 |
| 28 | gc 三道保留線 | CLI-GC-1, CLI-GC-2, CLI-GC-3, STATE-TASK-5 |
| 29 | 第二輪複核修補 | CLI-GC-3, CLI-EVICT-3 |
| 30 | CC 權限框特徵 canary | HOOK-NOTIFY-2 |
| 31 | 第三輪複核修補 | CLI-STATUS-1, CLI-READ-1, CLI-AWAIT-2, CLI-DESPAWN-3, CLI-EVICT-2, CLI-RO-1, ENV-POLL-1, CLI-GEN-3, CLI-SEND-3, STATE-TASK-4, STATE-TASK-5 |
| 32 | spawn 落點＋owner/actor 審計 | CLI-SPAWN-5, CLI-SPAWN-6 |
| 33 | 通知原生化 Phase 1 | CLI-HOOK-1, HOOK-ID-1, HOOK-ID-2, HOOK-EVT-1, HOOK-EVT-2, HOOK-EVT-3, HOOK-EVT-4, HOOK-NOTIFY-1, STATE-CHAN-1, STATE-CHAN-2, STATE-CHAN-3, ENV-TTL-1 |
| 34 | 獨立複核 blocker 修補 | HOOK-ID-3, HOOK-EVT-3, HOOK-NOTIFY-1, ENV-TTL-1, ENV-TTL-2 |
| 34.5+ | 巢狀 runtime 冒名（owner gate） | HOOK-OWNER-1, HOOK-OWNER-2, HOOK-OWNER-3, HOOK-OWNER-4, HOOK-EVT-4, ENV-TTL-3, STATE-CHAN-2 |
| 35 | 行程身分閘門（M5 窗 1） | HOOK-OWNER-5, STATE-AGENT-4 |
| 36 | codex launcher 形（HOOK-OWNER-5 自癒擴充） | HOOK-OWNER-5 |
| 37 | agy runtime（Antigravity CLI） | CLI-SPAWN-1, HOOK-NOTIFY-2 |
| 38 | list --long 介入視圖 | CLI-LIST-1, CLI-LIST-2 |

## `[untested]` 缺口處置（本輪只記錄，不補測）

| 條款 | 缺口 | 處置 |
|---|---|---|
| ENV-GEN-1 | 「未列變數不影響行為」無測試枚舉 | 認列不測：負面全稱句無法窮舉；由 check-contract.sh 第 1 項的集合 diff 守住新增變數必入 spec 的方向 |
| HOOK-OWNER-5（部分） | 「同快照夾讀」的單次 stat／夾讀取樣本身無 hermetic 注入面 | 取樣層屬靜態審查保障；判定條件（含兩邊 starttime 不等必拒、runtime 白名單）由 `hook::tests::launcher_hop_decision_confirms_only_the_exact_shape` 錨住，caller 整條路徑由分組 36 錨住 |
| STATE-GEN-2 | 原子寫入（暫存檔＋rename）無直接斷言 | 認列不測：黑箱難以斷言中間態不可見；分組 15 併發壓測為間接證據；Rust 遷移時以實作審查＋同款壓測守住 |

已知「有行為無測試」的既有缺口（繼承自 notify-native 交接，非本輪新增）：
cancel 延後路徑 canary、codex 互動 TUI 的 Stop hook、多任務連鎖阻擋實測、
hook 失效→TTL→legacy 實境 canary——均屬實境驗證層（hermetic 測試已有），
不計入上表 untested（上表只記 hermetic 層缺口）。

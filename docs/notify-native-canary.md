# 通知原生化：真 runtime canary 紀錄

驗的是通知原生化（`171bb7f` / `9c6c973` / `d525346`）最大的未驗證假設：整條
hook 鏈在**真的 Claude Code session** 上成不成立。在此之前只有單元層 fixture
證據——測試套件證明 `agent-bridge hook stop` 這支程式的行為正確，但沒有任何證據
證明 Claude Code 真的會呼叫它、真的接受它吐出的 `decision: block`。

- 日期：2026-07-28
- 環境：codex-cli 0.145.0 無關；claude worker 以 `--model sonnet` spawn，
  hooks 由 `share/claude-worker-hooks.json` 經 `--settings` 注入
- 執行者：orchestrator pane `%87`（notify-native-2），canary worker pane `%89`

## 復現步驟

```bash
AGENT_BRIDGE_MAX_SPAWN=6 agent-bridge spawn canary-notify --runtime claude --model sonnet

# 任務一：只跑 sleep 90 再回覆，用途是把對方的 turn 撐開
agent-bridge send canary-notify --from <you> --message-file task1.md

# 等 state 檔翻成 busy（UserPromptSubmit hook 標記，實測 2 秒內）
jq -r .state ~/.local/share/agent-bridge/state/canary-notify.json

# 趁 busy 派第二任務
agent-bridge send canary-notify --from <you> --message-file task2.md

agent-bridge await <task2-id> --timeout 240
```

## 觀察到的事實

**hook 真的被呼叫。** spawn 完成、worker 跑完 `agent-bridge ready` 那一輪之後，
`state/canary-notify.json` 就已存在並記為 `idle`——這份檔案只有 hook 會寫，等於
Claude Code 確實執行了 `agent-bridge hook`。派工後 2 秒內翻成 `busy`。

**任務二事件線（決定性證據）**

```
2026-07-28T03:45:03Z created from=notify-native-2 to=canary-notify
2026-07-28T03:45:03Z notify-deferred pane=%89 cmd=receive
2026-07-28T03:46:46Z delivered
2026-07-28T03:46:49Z replied
```

`notified` 出現 0 次、`notify-deferred` 出現在派工當下：**沒有任何按鍵送進 worker
的 pane**。`delivered` 落在 03:46:46，是任務一回覆（03:46:37）之後 9 秒——這段空
檔沒有任何外部通知，取件是 worker 自己在 turn 結束時做的，唯一可能的觸發源是
Stop hook 回傳的 block。兩個任務終態皆 `completed`，回覆內容正確。

**Stop hook 的迴圈剎車有落地。** 收尾後 state 為
`{"state":"idle","ts":"2026-07-28T03:46:53Z","last_delivered":"20260728T034503Z-7b54"}`，
`last_delivered` 確實記到任務二的 id——同 id 再擋一次就會放行的那條規則有它需要
的狀態。

**額外驗到 `respond_task` 呼叫點。** 兩個任務的 `replied` 事件都跟著
`notify-deferred pane=%87 cmd=read`：orchestrator（我）當時也在 turn 中，於是回覆
通知同樣被延後。這條路徑本來只打算靠測試涵蓋，這次是真實情境下的證據。
orchestrator 靠 `await` 拿到結果，沒有因為少收一次送鍵而卡住。

## 結論

Plan 的 rubric 第 5 條（真 runtime canary 一次通過）成立，且 `cmd_send` 與
`respond_task` 兩個呼叫點都有真實環境證據。Phase 1／2 的核心假設不再是假設。

## 這次沒有驗到的

- `cmd_cancel` 呼叫點的延後路徑只有測試涵蓋，canary 未觸及。
- 只跑了單一 worker、單一 runtime（claude）。多任務連鎖阻擋（同一 turn 結束時
  mailbox 有兩件以上）未實測。
- 沒有製造 hook 失效的情境（例如把 `agent-bridge` 從 PATH 拿掉），TTL 到期退回
  legacy 送鍵這條失效路徑仍只有測試證據。

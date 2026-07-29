# 巢狀 runtime 冒名：owner gate 真實環境 canary 紀錄

驗的是 `82eb966`（owner/session_id 所有權閘門）最大的未驗證假設：gate 的三岔路
語意（HOOK-OWNER-2）與「排隊任務不被巢狀 session 攔走」（HOOK-OWNER-1）在
**真實 runtime** 上成不成立。在此之前只有 hermetic 測試證據（分組 34.5+，
以直接呼叫 `agent-bridge hook` 模擬 payload）。

- 日期：2026-07-29
- 環境：claude worker 以 `--model sonnet` spawn；巢狀實例為 `claude -p
  --model haiku --settings share/claude-worker-hooks.json`（真實 headless
  claude，hooks 實際發射）；`AGENT_BRIDGE_DATA` 全程指向隔離目錄，經
  `AGENT_BRIDGE_PASS_ENV=AGENT_BRIDGE_DATA` 穿透給 worker pane
- 執行者：nn-next-plan（協調 session）；證物目錄為 session scratchpad
  `nested-canary/`（timeline、owner-samples、events.log 摘錄如下）

## A 段：gate 三岔路（三個真實 claude 實例共用一個 SPAWN_TAG）

```bash
CAND=<scratch>/nested-canary
# A1 首發認領（正控制：證明真實 runtime 的 hook 會發射、會寫 state）
env AGENT_BRIDGE_DATA=$CAND/data \
    AGENT_BRIDGE_SPAWN_TAG=ab-spawn-canary-solo-99999-0123456789ab \
    claude -p 'Reply with exactly: ok' \
    --settings share/claude-worker-hooks.json --model haiku
# → state/canary-solo.json 誕生，owner=850dce30-…（實例 A 的 session_id）

# A2 異主＋state 新鮮：第二個實例（不同 session_id）同 tag 再跑一次
# → diff 前後 state 檔【位元組級相同】；owner 仍為 850dce30-…
#   （A1 已證明同形呼叫會發射 hook，故「不變」＝被 gate 擋、不是沒發射）

# A3 接管：把 ts 改成 2 小時前（> TTL 1800）再跑第三個實例
# → owner 變為 09b5434c-…（實例 C）、ts 更新——過期 state 合法接管
```

三岔路（認領／擋異主／過期接管）與 spec HOOK-OWNER-2 逐條相符。

## B 段：巢狀 session 不得攔走排隊任務

```bash
env AGENT_BRIDGE_DATA=$CAND/data-b AGENT_BRIDGE_PASS_ENV=AGENT_BRIDGE_DATA \
    agent-bridge spawn canary-nested --runtime claude --model sonnet
# task1：指示 worker 在 turn 內執行巢狀 claude -p（同上旗標）＋sleep 20 再 reply
agent-bridge send canary-nested --from nn-next-plan --message-file task1.md
# state 翻 busy（2 秒內）後趁巢狀視窗送 task2
agent-bridge send canary-nested --from nn-next-plan --message '…reply task2-done'
agent-bridge await <task2> --timeout 300   # → completed
```

**決定性時間線**（隔離 DATA 的 events.log＋worker 側 timeline）：

```
10:06:29 task1 created / notified（legacy 送鍵，worker 當時 idle）
10:06:42 巢狀 claude 開始（worker turn 內）
10:06:43 task2 created / notify-deferred（parent busy，零送鍵）
10:06:50 巢狀 claude 結束（輸出 nested-done——巢狀真的跑完，其 Stop hook 已發射）
10:07:15 task1 replied（parent turn 結束）
10:07:22 task2 delivered ← parent 自己的 Stop block 取件
10:07:28 task2 replied（task2-done，內容正確）
```

- 巢狀 Stop 發射當下（≈10:06:50）task2 已排隊 7 秒。若 gate 缺席，巢狀
  session 會在此刻被 block JSON 指使 `receive` task2——實測 delivered 遲至
  10:07:22、由 parent 取件，佇列未被攔走。
- owner 取樣（每 3 秒）全程唯一值＝parent 的 `10220aed-…`；終態
  `last_delivered` 記 task2（迴圈剎車狀態正確）。

## 結論

owner gate 的 hermetic 證據升級為真實環境證據：三岔路語意成立
（HOOK-OWNER-2/3）、無身分與異主呼叫零副作用（HOOK-OWNER-1）、排隊任務
只由正主取件。計畫 P5 的三個觀察點（owner 維持／巢狀 stop 不受 block／
TTL 過期接管）全數命中。

## 這次沒有驗到的

- 巢狀實例是 headless `claude -p`：它的 hook **確實發射**（A1 正控制），但
  headless 模式對 block decision 的服從度未單獨驗證——本 canary 證明的是
  「gate 讓巢狀根本收不到 block」，而非「巢狀收到 block 也不會服從」。
- 巢狀 runtime 只測了 claude；codex exec 作巢狀（probe 當初的缺陷場景）未在
  真環境重跑——其 payload 同帶 session_id（docs/codex-hooks-probe.md 實測），
  gate 邏輯無 runtime 分支，推論同樣成立（Reasoned，非 Verified）。
- /clear 後 parent 換 session_id 的自癒路徑：A3 以人工 stale ts 等價模擬
  （時間窗語意相同），未用真 /clear 操作。

# codex hooks probe：Stop 能不能 block？

Phase 3 的選型 gate。plan 把這關標成 human judgment——probe 是對外部 CLI 的實測
觀察，沒有純腳本斷言能代勞，所以理由與復現步驟必須留檔。

- 日期：2026-07-28
- 版本：codex-cli **0.145.0**（`codex --version`）
- 結論：**(a) Stop 可 block → 對齊 claude 全套**

## 為什麼非實測不可

plan 立案時的已知事實只到「codex hooks 已 stable、事件含 Stop」，**Stop 能否
block＋續跑文件未載明**。這件事不能從「Claude Code 可以」推論過去：兩邊的 hook
協定即使欄位長得像，退出碼語意與 block 支援度都可能不同，而 agent-bridge 整個
通知原生化的主通道就架在「Stop 能擋下停止」這一條上。猜錯的代價是派工永久送不到。

## 過程與三個岔路

**第一次 probe 失敗，但失敗本身是發現。** 用 `codex exec -c 'hooks.Stop=[…]'`
把 hook 從命令列注入，hook 一次都沒被呼叫。加掛 `UserPromptSubmit`、`SessionStart`
一起試，同樣全無動靜。

對照組排除了「exec 模式不跑 hooks」這個解釋：同一個 exec 跑一條 shell 指令，基底
`~/.codex/config.toml` 裡的 PreToolUse hook（bash-log）確實有寫 log。所以 hooks
在 exec 模式會觸發，是 `-c` 注入的那些沒被採用。

原因在 `codex exec --help`：`--dangerously-bypass-hook-trust`「Run enabled hooks
without requiring **persisted hook trust** for this invocation」。codex 對 hook 有
**內容雜湊信任機制**，未受信任的 hook 會被**靜默略過**——沒有警告、沒有錯誤，
就是不執行。持久化位置在 `~/.codex/config.toml`：

```toml
[hooks.state."/home/mango/.codex/config.toml:pre_tool_use:0:0"]
trusted_hash = "sha256:cecc094d…"
enabled = true
```

key 是「宣告 hook 的 config 檔路徑 : 事件 : 索引 : 索引」。命令列 `-c` 注入的 hook
沒有這樣的登記項，於是被跳過。

## 實測（由人執行，2026-07-28）

probe hook 只做兩件事：把 stdin 記進 log，並在**第一次**被呼叫時印出 block JSON
（靠 mark 檔保證只 block 一次，免得真能 block 時打成無限迴圈）。判讀完全靠 log
的 invocation 次數，不依賴模型配合：1 次＝block 無效，2 次＝block 生效。

```bash
cd <probe-dir>
codex exec --skip-git-repo-check --dangerously-bypass-hook-trust \
  -c 'hooks.Stop=[{hooks=[{type="command",command="bash <probe-dir>/stop-hook.sh"}]}]' \
  'Reply with exactly the word ACK and nothing else.' </dev/null
```

輸出：

```
codex
ACK
hook: Stop
hook: Stop Blocked
codex
PROBE-CONTINUED
hook: Stop
hook: Stop Completed
```

hook log 記到兩次 invocation。**block 生效**，而且 `reason` 確實被當成指示交給
模型——它照著 reason 要求輸出了 `PROBE-CONTINUED`，不是隨便再跑一輪。

## stdin payload 與 Claude Code 同構

```json
{
  "session_id": "…", "turn_id": "…", "transcript_path": "…", "cwd": "…",
  "hook_event_name": "Stop", "model": "gpt-5.6-sol",
  "permission_mode": "bypassPermissions",
  "stop_hook_active": false,
  "last_assistant_message": "ACK"
}
```

第二次呼叫時 `stop_hook_active` 為 `true`。**迴圈剎車需要的欄位就位**，語意也與
Claude Code 相同。這表示 `cmd_hook` 現行實作（讀 `stop_hook_active`、比對
`last_delivered`、輸出 block JSON）**不必為 codex 改寫**。

## 退出碼語意：與 Claude Code 相同，鐵律照舊

binary 字串（`strings` 於 codex 原生執行檔）含：

```
Stop hook returned decision:block without a non-empty reason
Stop hook exited with code 2 but did not write a continuation prompt to stderr
hooks/src/events/stop.rs        struct StopCommandOutputWire
```

第二條直接回答 plan 那句「codex 的 hook 退出碼語意是否相同，不得假設一致」：
**exit 2 同樣是 block 訊號**。所以「`cmd_hook` 任何錯誤一律 exit 0、絕不 exit 2」
這條鐵律在 codex 側原封不動適用，不需要分歧的處理。

## 事件集差異：codex 沒有 Notification

從 binary 的 `HookEventNameWire` 列舉讀到的事件是 PreToolUse、PermissionRequest、
PostToolUse、PreCompact、PostCompact、SessionStart、SessionEnd、SubagentStart、
SubagentStop，外加獨立處理的 Stop。**沒有 Notification**。

claude 側用 Notification 的 `idle_prompt` 標記閒置；codex 沒這個事件，但 Stop 本來
就會在 turn 結束時標 idle，所以 codex 只需要 Stop ＋ UserPromptSubmit 兩個事件，
功能不缺角。

## 接線的真正代價：信任是一次性人工步驟

這是 probe 最有價值的副產品，也是接線前必須知道的：

1. hook 寫進 `~/.codex/agent-worker.config.toml` 後，**第一次仍需人接受信任提示**
   （沒有 `codex hooks trust` 這種子命令，只能互動式授予，或用那個 dangerous 旗標）。
2. 失敗方向是**靜默略過**——這是最糟的失效方向。hook 沒被信任時，worker 看起來
   一切正常，只是通知永遠不會由 hook 送達。所幸 agent-bridge 這側的失效方向是
   安全的：state 檔不會被寫出 → `notify_or_defer` 判定「未知」→ 退回 legacy 送鍵。
   也就是說**信任沒設好只會退回今天的行為，不會讓任務餓死**。
3. 信任記錄寫在 `~/.codex/config.toml`，而該檔是 **chezmoi 管理**的；信任項若不
   進 chezmoi source，下一次 `chezmoi apply` 就會被抹掉。
4. hook 宣告字串改變會使雜湊失效，需重新授予。所幸這裡的宣告是穩定的
   `agent-bridge hook stop`，不隨版本變動。

## 接線後的實測（2026-07-28，同日稍後）

信任由人互動式授予一次後：

- `hook: UserPromptSubmit` / `hook: Stop` 在 `codex exec --profile agent-worker`
  確實觸發（未授信任前完全不出現，也沒有任何警告——**靜默略過**已實測確認）。
- 信任記錄被 codex 追加寫進 **profile 疊加檔自己**（`~/.codex/agent-worker.config.toml`
  檔尾的 `[hooks.state]`），不是基底 `config.toml`。
- 雜湊算的是 **hook 宣告本身**，不是整個檔案：改動同檔的註解後重新 `chezmoi apply`，
  hook 仍受信任、照常觸發。
- 真 codex worker（`agent-bridge spawn --runtime codex`）的 hook 子行程讀得到
  `AGENT_BRIDGE_SPAWN_TAG`，state 檔正常生成。
- busy 派工 canary 通過：第二任務 `notified` 0 次、`notify-deferred` 1 次，
  `delivered` 落在前一任務回覆後 9 秒（由 worker 自己的 Stop hook 取件），
  `last_delivered` 正確、兩個任務終態 `completed`。

## ⚠️ 新發現的缺陷：巢狀 runtime 會汙染 parent 的 state

**這不是 codex 專屬問題，claude 側同樣成立**，是通知原生化本身引入的脆弱面，
在這次 probe 過程中被撞出來。

`AGENT_BRIDGE_SPAWN_TAG` 是環境變數，**子行程一律繼承**。所以在一個 worker 的
session 裡再啟動任何 agent runtime（`codex exec`、`claude -p`、review-overlay
的 fallback 載具 codex-rescue／codex MCP……），那個巢狀 session 的 hooks 會用
**parent worker 的名字**去寫 state。

實測（本 session 自己就是受害者，pane `%87`、agent `notify-native-2`）：主 session
從頭到尾沒有結束過 turn，state 檔的時間戳卻兩度前進並被標成 `idle`，時間點與
每次 `codex exec` 完全對應。

兩層後果，第二層更嚴重：

1. **state 被標錯** → `notify_or_defer` 讀到「idle」→ 走送鍵路徑，對著一個實際
   正忙的 worker 送 Enter。這正是整個通知原生化要消除的那個風險，等於被繞回來。
   （本次實際被 `notify_pane` 的畫面掃描 fail-closed 擋下，記成 `notify-failed`
   ——第二道防線接住了，但它本來只該是 belt-and-suspenders。）
2. **任務可能被巢狀 session 攔截**：子 session 的 Stop hook 會拿 parent 的名字去
   查 mailbox，查到 queued 任務就輸出 block JSON 叫它自己去 `agent-bridge receive`。
   `receive` 不驗身分，於是**本該給 parent 的任務被子 session 取走**，parent 永遠
   等不到。本次沒有觸發，只是因為當下 mailbox 恰好沒有排隊中的任務。

尚未修。可行的最小修法（未經核准、未實作）：hook payload 有 `session_id`，可以
讓第一次寫入的 session 把 id 記進 state 檔，之後 id 不符的一律忽略——worker 自己
的 `ready` 那一輪必然最早寫入，所以先到先得剛好選中正主；巢狀 session 的 id 不同
會被擋掉。失效方向安全（被擋 → state 停更 → TTL 過期 → 退回 legacy 送鍵）。需要
處理的邊界是 worker 自身 session id 變動（例如 `/clear`）後如何交還所有權。

## 未驗的部分

- 只在 `codex exec` 非互動模式測 block 語意。互動 TUI 模式的 Stop 行為未測
  （預期相同，但這份文件不宣稱）。
- block 語意本身是在 `--dangerously-bypass-hook-trust` 下測的；走正規信任流程後
  只驗到 hook 會觸發與 busy 派工端到端成功，沒有再單獨重測一次 block JSON 的
  拒絕分支（`stop_hook_active` ＋同 id 放行）。

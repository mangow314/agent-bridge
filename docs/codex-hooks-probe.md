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
[hooks.state."/home/<user>/.codex/config.toml:pre_tool_use:0:0"]
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
3. 信任記錄寫在**宣告該 hook 的那個檔案自己**——本例是
   `~/.codex/agent-worker.config.toml` 的檔尾，不是基底 `config.toml`（下一節
   有實測輸出）。立案時我原本以為會落在基底檔，是錯的。該檔是 **chezmoi 管理**
   的；信任項若不進 chezmoi source，下一次 `chezmoi apply` 就會被抹掉。
4. hook 宣告字串改變會使雜湊失效，需重新授予。所幸這裡的宣告是穩定的
   `agent-bridge hook stop`，不隨版本變動；實測改動同檔的**註解**不會使雜湊失效
   （雜湊算的是宣告本身）。

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
session 裡再啟動一個**自己也載入了 agent-bridge hooks** 的 agent runtime，那個
巢狀 session 的 hooks 會用 **parent worker 的名字**去寫 state。

**觸發的必要條件是「child 也載入這組 hooks」，不是「child 是個 agent runtime」**
（cross-vendor 複核第二輪的 finding，我原本的寫法過度宣稱）。具體來說：

- 會觸發：`codex exec --profile agent-worker`、`claude -p --settings
  <share/claude-worker-hooks.json>`——也就是把 worker 那組設定帶進去的呼叫。
- **不會**觸發：不帶 profile 的普通 `codex exec`、不帶 `--settings` 的普通
  `claude -p`。它們同樣繼承了那個環境變數，但沒有任何 hook 會去讀它，
  `cmd_hook` 根本不會被呼叫。

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

**已修（2026-07-29）**：state 檔加 `owner` 欄記 hook payload 的 `session_id`，
第一個帶 id 寫入者先到先得認領（worker 自己的第一個事件必然最早）；之後 id 不符
的呼叫一律靜默擋下——不寫 state，stop 也不發 block（兩層後果同時堵住）。接管
條件：state `ts` 超過 `AGENT_BRIDGE_STATE_TTL` 或落在未來——這同時是 worker
自身 session id 變動（例如 `/clear`）後的所有權交還路徑，自癒上限＝TTL。缺
`session_id` 的 payload 不參與 state 通道（不寫、不 block），失效方向仍是安全的
降級鏈（state 停更 → TTL 過期 → 退回 legacy 送鍵）。殘餘邊界（/clear 後最長一個
TTL 的降級窗、長壽巢狀 session 越過 TTL 的奪權可能）詳見 README.zh-TW.md
已知限制節。回歸測試：run-tests.sh 34.5–34.13。

## 未驗的部分

- 只在 `codex exec` 非互動模式測 block 語意。互動 TUI 模式的 Stop 行為未測
  （預期相同，但這份文件不宣稱）。
- block 語意本身是在 `--dangerously-bypass-hook-trust` 下測的；走正規信任流程後
  只驗到 hook 會觸發與 busy 派工端到端成功，沒有再單獨重測一次 block JSON 的
  拒絕分支（`stop_hook_active` ＋同 id 放行）。

---

# 補測（2026-07-31）：codex 的 hook 行程樹

M5 把「codex 行程樹未量測」列為開著的缺口（架構 §11.6）。這一節把它量掉。

- 版本：codex-cli **0.146.0**
- 方法：`bin/agent-bridge` shim 暫時加一段探針，`hook` 子命令進來時先把自身
  祖先鏈（pid／ppid／starttime／comm）附加到記錄檔再 `exec` 真的執行檔——
  stdin、argv、退出碼都不碰。spawn 一個真的 codex worker（`m5codex`）取其
  prompt-submit 與 stop 事件，量完 despawn、`git checkout` 還原 shim。
- 結論：**codex 的 hook 行程直接父行程不是 `pane_pid`，M5 的行程身分閘門對
  codex 一律確認不了，全面落回 M4 的時間窗判別。**

## 實測行程樹

```
hook_pid=1612290  ppid=1609575  starttime=56855455  bash
        1609575   ppid=1609509  starttime=56853512  codex   <- 原生執行檔，fork hook 的是它
        1609509   ppid=9411     starttime=56853508  node    <- pane_pid，registry 記的是它
           9411   ppid=1        starttime=11683     tmux: server
```

兩個行程的 argv：

```
1609509: node /home/<user>/.nvm/versions/node/v22.18.0/bin/codex --profile agent-worker
1609575: /home/<user>/.nvm/.../@openai/codex-linux-x64/vendor/.../bin/codex --profile agent-worker
```

（上列路徑是**本機 nvm 安裝**的實測值，非通用形狀。
**實作註記（非本輪量測）**：runtime 身分判定不看目錄——`cmdline_is_runtime`
（`crates/ab-core/src/proc.rs`）只比 argv[0]／argv[1] 的 basename。其他安裝
方式的行程樹層數本輪未量測。）

## 為什麼與 claude 不同

claude 是 tmux 直接 exec 的單一行程，pane_pid 就是 fork hook 的那個行程。
codex 經 npm 安裝，`bin/codex` 是一支 **node launcher**：tmux exec 的是
`node …/bin/codex`（＝pane_pid），它**不 exec 取代自己**，而是 fork 出平台
原生執行檔，再由原生執行檔 fork hook。中間因此多一層。

STATE-AGENT-4 的 argv 規則對 launcher **正確命中**（argv[0] 是直譯器 `node`、
argv[1] 的 basename 是 `codex`），所以 registry 兩欄照樣寫入：

```
worker_pid = 1609509（= tmux 回報的 pane_pid）
worker_starttime = 56853508（與該 pid 當下相符）
```

命中的是「pane 行程確實是 codex 的啟動器」——這句話為真。不為真的是後面那個
隱含前提：**啟動器不等於 fork hook 的那個行程**。

## 這同時是本輪修補的實證

把實測值代進 M5 首版的判別邏輯：`worker_pid` 1609509 仍活著（✓）、其
starttime 與紀錄相符（✓）、hook 的 PPID 1609575 不等於它（✓）——正好命中
首版那條「明確冒名 → 擋，且 ts 過期也不得接管」。也就是說首版一旦上線，
**每一個 codex worker 的每一個 hook 事件都會被永久擋死**，state 通道整條沒有
自癒路徑。

複核者當初的反對是論證加一個合成實測；這次量到的是本機真實存在的形狀。修補
後的行為是落回 M4：實測期間 `state/m5codex.json` 正常更新（`idle`，owner 為
codex 的 session id），通知通道與 M5 之前完全相同。

## 追測（同日）：這個窗對 codex 是否真的存在

前一節確認了 codex 拿不到自癒，但「拿不到」只有在 session_id 真的會變、而行程
不變的情況下才會痛。這是另案立案前的第一道 gate，先量掉。

- 方法：同一支 shim 探針，改記 hook payload 的 `session_id` 與 `PPID`
  （stdin 照 hook 本身的形狀讀完再餵回去）。spawn 一個 codex worker
  （`m5sid`），取基準事件，於 pane 內下 `/new`，再送一個 prompt 觸發 turn。
- 結論：**窗存在。**

```
/new 前  prompt-submit / stop   sid=019fb677-4b22-77b3-8b67-0814fe3a97ca  ppid=1646226
/new 後  prompt-submit / stop   sid=019fb677-bebd-7353-ad01-03936ba3f5a4  ppid=1646226
```

**PPID 完全不變（1646226），session_id 換了。** 這正是 M5 窗 1 的形狀：行程沒
變＝身分沒變，但 session_id 判別認不出來。

實際傷害也重現了：`/new` 之後兩次 hook 事件都被擋，`state/m5sid.json` 的
`owner` 停在**舊** sid，ts 凍結在 `/new` 前最後一次寫入。要等 TTL（預設 1800
秒）過期才交還。

傷害程度取決於凍結當下的值：停在 `idle` 時通知端照樣走 send-keys 送達（回到
legacy，不算致命）；停在**新鮮的 `busy`** 時通知端不送鍵、而 worker 的 stop
hook 又被擋著不發 block，**任務會卡在 mailbox 直到 TTL 過期**——那才是這個窗
真正的代價。

## canary（同日稍後）：launcher 形擴充落地後的復量

上一節的窗已由 launcher 形擴充收掉（HOOK-OWNER-5 現行文、architecture
§11.7；落地時「有界祖先鏈＋中間不夾 runtime」的草案規則被收窄成逐 runtime
白名單形狀——草案版對本檔 m5-proposal §1 的實測巢狀鏈會誤放行）。落地後
以同一支 shim 探針復量（worker `m6canary`，事件只記 PPID 不碰 stdin）：

```
/new 前  owner=019fb6da-64ff-…  ts=06:26:50（新鮮）  hook ppid=1999985
/new 後  owner=019fb6da-dcb4-…  ts=06:27:43          hook ppid=1999985
registry：worker_pid=1999927（launcher）  runtime=codex
```

`/new` 後第一個 turn 的 hook 事件**當場**把 owner 換成新 sid——舊 ts 距當下
僅 51 秒、遠小於 TTL，M4 行為下必被時間窗擋住。PPID 前後不變（1999985＝原生
執行檔），其父即 registry 記的 launcher（1999927）：身分閘門以 launcher 形
**確認成功，非落回**。codex 的窗 1 就此關閉。

## 尚未收的

- 只量了 npm 全域安裝這一種佈署形狀。若哪天 codex 改成單一原生執行檔直接
  上 PATH，行程樹會與 claude 同形（直接形命中、launcher 形不再需要），屆時
  要重量確認。
- launcher 形釘在「恰好一層」：codex 若改成多層 fork（launcher → broker →
  原生），比對會失敗、全面落回 M4 行為——安全但窗回來，屆時按新實測形狀擴充
  白名單。

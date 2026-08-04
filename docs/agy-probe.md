# agy（Antigravity CLI）runtime 量測

量測日：2026-07-31。受測版本 **1.1.9**（`~/.local/bin/agy`，單一 ELF）。
比照 `docs/codex-hooks-probe.md`：只記實測到的事實與當下的判定，不記推論。

## 0. 旗標面（`agy --help` 原文摘錄）

```
--print / --prompt / -p            單次非互動跑一個 prompt 並印出
--prompt-interactive / -i          跑完 initial prompt 後續留互動 session
--print-timeout                    print 模式等待逾時（預設 5m0s）
--model / --effort (low|medium|high)
--mode (accept-edits, plan)
--agent                            指定本次 session 的 agent
--sandbox                          終端受限沙箱
--dangerously-skip-permissions     自動核准所有工具權限請求
--output-format (text, json, stream-json) / --json-schema
--add-dir / --continue|-c / --conversation / --project / --new-project
--disable-slash-commands / --log-file
子命令：agent(s), changelog, help, install, models, plugin(s), update
```

**無 `--settings` 這類 per-invocation 設定覆蓋旗標**（claude runtime 靠它掛
hooks）；亦無 codex `--profile` 那種具名設定檔選擇。

## 1. `--prompt-interactive` 的參數形狀 —— 空白分隔旗標值，**形狀相容**

- `agy -p 'Reply with exactly the single word: PONG'` → 輸出 `PONG`，rc=0。
  空白分隔的旗標值可用（Go flag 風格），不必 `=`。
- tmux 拋棄式 window 實跑
  `exec agy --prompt-interactive "Reply with exactly the single word: PONG-I"`：
  pane 內顯示該 prompt 為第一則 user message、回 `PONG-I`，隨後停在互動輸入
  列（footer `? for shortcuts`）。

→ spawn 的注入形狀（`spawn.rs:551`，`exec <runtime_cmd> <prompt 位置參數>`）
直接可用，位置參數落為該旗標的值。**不需要改注入形狀。**

⚠️ 補測（跨廠複核 2026-07-31 抓出的 blocker）：`--prompt-interactive` 不是
布林開關，而是**吃下一個 token 當值**的 string flag。無副作用的 parser 探針：

```
agy --prompt-interactive --not-a-real-flag --help   # rc=0，照印 help
```

未知旗標沒有報錯，代表它被吞成了值。所以它**必須是最後一個旗標**：
`agy … --prompt-interactive --model <m> <prompt>` 會讓 `--model` 被吃成
initial prompt、模型旗標失效、真 prompt 變成錯位的位置參數。正確形狀是
`agy --dangerously-skip-permissions --sandbox [--model <m>] --prompt-interactive
<prompt>`（實作把尾旗標從 runtime_cmd 拆出，等 `--model` 附加完再接上，
見 `spawn.rs` 的 `runtime_tail`）。

## 2. 權限路線 —— 只有粗粒度，且**發現一個安全缺口**

- 可用者僅：`--dangerously-skip-permissions`（全開）、`--mode accept-edits|
  plan`、`--sandbox`、`--agent`。
- `agy agents` 輸出 `Available agents:` 後為空；`~/.gemini/agents/` 空目錄。
  沒有現成的受限 agent 可指定。
- 設定檔 `~/.gemini/antigravity-cli/settings.json` 當前只有 `statusLine` 與
  `trustedWorkspaces`（本 repo 已在列）。互動權限框第 3 選項寫著
  「Persist to settings.json」，故推測存在 allowlist 欄位，但**未實測其
  schema**，且無 per-invocation 覆蓋旗標可讓 worker 用獨立的一份。

→ 結論：**沒有可類比 codex `--profile agent-worker` 的細粒度路線。**
這是交接表列的 human judgment gate，須由使用者裁決 worker 的權限姿態。

**裁決（使用者，2026-07-31）：`--dangerously-skip-permissions --sandbox`。**
據此實測 sandbox 的實際限制（拋棄式 pane，同組旗標）：

- `agent-bridge --help` → rc=0，正常輸出 → **worker 契約的 CLI 呼叫不被擋**
- `touch .progress/agy-sbx-probe.tmp` → rc=0 → 專案內寫檔不被擋
- 全程無權限框（skip-permissions 生效）

→ runtime_cmd 定為
`agy --dangerously-skip-permissions --sandbox --prompt-interactive`。

### ⚠️ 缺口 AGY-PROMPT-1：agy 的權限框騙過 `screen_has_prompt`

實測 agy 權限框畫面（送鍵測試中自然觸發）：

```
Requesting permission for:
   ./bin/agent-bridge receive probe-test-id

Do you want to proceed?
> 1. Yes
  ...
  4. No

  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command
esc to cancel
```

`notify::screen_has_prompt`（`notify.rs:50`）的判定是
`contains("Do you want to ") && contains("Esc to cancel")` —— agy 印的是
**小寫 `esc to cancel`**，因此**不命中**。後果：worker 停在權限框時，
`notify_pane` 的 fail-closed 掃描會放行送鍵，那個 Enter 會落在預設選項
`1. Yes`，**替一個正在等人類決策的 worker 按下批准**。

處置（已實作，回歸鎖在分組 37d／37e）：`screen_has_prompt` 加**兩個**錨——
header 的 `Requesting permission for:`，以及下緣備援的
`Do you want to proceed?` ＋小寫 `esc to cancel` 成對。

備援錨不是冗餘（跨廠複核 2026-07-31 的 blocker）：掃描只看可見一屏
（`capture-pane -pJ`，不取 scrollback），而 worker 預設進共用 window 並
`tiled` 均分——pane 一多就矮到 header 被捲出畫面、只剩選項與 footer。
本案自己在測試裡先撞上過這個形狀（分組 37d 原本 split 進共用視窗、全套跑時
假紅），那不是佈景怪癖，正是 production 形狀。分組 37e 用固定 6 列高的 pane
鎖住它；mutation 反例確認：拿掉備援錨，37e 立刻紅。

`--dangerously-skip-permissions` 讓框在正常路徑不出現，但**不能取代**上述
修補：框仍可能因其他路徑出現，而漏判的代價是替人按下批准。

## 3. legacy send-keys 通知 —— **可靠**

- agy 無 hooks 子系統 → 不會寫 `state/<agent>.json` → `notify_or_defer`
  （`notify.rs:105`）讀不到 state 一律當「未知」走 `notify_pane` 送鍵。
- 實測：對 agy pane 送 `agent-bridge receive probe-test-id` ＋ 1 秒後送
  Enter，文字完整成為一則 user message，agy 隨即據以行動（去跑
  `./bin/agent-bridge receive …`）。文字與 Enter 未被當成貼上而吞掉。

→ legacy 通知路徑在 agy 上成立；沿用既有 `AGENT_BRIDGE_NOTIFY_DELAY`
（預設 0.3s）即可，本次以 1s 實測通過。

第二次實測：agy 偶爾在畫面下緣彈出問卷列
（`How's the CLI experience so far? [1] Good [2] Fine [3] Bad [0] Skip`）。
在該列顯示時送鍵，文字照樣落進輸入列成為 user message、agy 照樣行動——
**問卷列不吃鍵、不影響通知**。

另記：agy 執行中的 footer 常駐 `esc to cancel`（小寫）。既有判定要求
`Do you want to ` 與之同時出現，故單純放寬大小寫**不會**在執行畫面誤判；
但 agy 的助理輸出若自己寫出「Do you want to …」，配上常駐 footer 就會湊成
誤判（方向是 fail-closed，代價僅為漏送通知）。修補改錨在 agy 權限框獨有的
`Requesting permission for:`，語意更準。

## 4. model 命名 —— `--model` 直通

`agy models`：

```
gemini-3.6-flash-high / -medium / -low
gemini-3.5-flash-high / -medium / -low
gemini-3.1-pro-high / -low
claude-sonnet-4-6
claude-opus-4-6-thinking
gpt-oss-120b-medium
```

量測時的登入身分為 Google AI Pro 方案；未指定時的預設為
**Gemini 3.6 Flash (High)**。spawn 的 `--model <v>` 附加寫法（`spawn.rs:337`）
可直接沿用，值照上表原字串。

## 5. 真機 canary（Phase 4，2026-07-31）

`agent-bridge spawn agy-canary --runtime agy` → pane `%119`，rc=0。四條 rubric
各自的證據：

1. **spawn 出身**：registry
   `{"runtime":"agy","spawned":true,"spawn_tag":"…ab-spawn-agy-canary-2614764-…",
   "worker_pid":"2614772","worker_starttime":"58155234","ready":true}`
   ——兩欄照記，非人工 register。worker 自行跑完 `agent-bridge ready`
   （brief 注入生效）。
2. **完整任務狀態機**：`tasks/<id>/events.log` 軌跡
   `created → notified pane=%119 cmd=receive → delivered → started → replied`，
   終態 `completed`；reply 內容（notify.rs 為 256 行）與 `wc -l` 實測相符。
3. **通知走 legacy**：`state/agy-canary.json` 自始不存在（agy 無 hooks），
   通知端因此落在「未知」分支送鍵。對照組：同一份 log 裡送回我自己的通知記為
   `notify-deferred pane=%116 cmd=read`——我有 state 檔，走的是另一條路。
4. **回收乾淨**：`despawn agy-canary` 後 `list` 不含該名、
   `capture-pane -t %119` 回 `can't find pane`、registry 檔已移除。

補跑（修掉 `--model` 錯序後，2026-07-31）：`spawn agy-model --runtime agy
--model gemini-3.1-pro-low` → pane `%120`，worker footer 顯示
**`Gemini 3.1 Pro · low`**（修正前會是使用者預設的 3.6 Flash High），
且照樣自行跑完 `agent-bridge ready`。`pane_start_command` 實際形狀：

```
… exec agy --dangerously-skip-permissions --sandbox --model gemini-3.1-pro-low \
    --prompt-interactive "$(cat -- '…/share/worker-brief.md')  -- …"
```

這條只能真機驗：測試 shim 只記 argv、不解析 agy 旗標，錯序在 shim 下照樣綠。

## 附帶事實

- 版本漂移：交接檔記 1.1.8，實測 **1.1.9**。
- attestation：tmux 直接 `exec agy`，argv[0] basename = `agy`，兩欄照記；
  M5 身分閘門只活在 hook 路徑，agy 無 hooks 不會誤觸（此點沿用前一棒判定，
  本次未另行實測）。

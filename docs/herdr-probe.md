# herdr 量測（狀態語意）

量測日：2026-07-31。受測版本 **0.7.5**（`~/.local/bin/herdr`，單一 ELF
21 MB，官方 `install.sh` 裝，無 sudo、無系統設定、無 telemetry）。
比照 `docs/agy-probe.md`：只記實測到的事實與當下的判定，不記推論。

量測動機：先前那輪 herdr 比較只看官網與文件、**沒有實裝試用**，卻打算抄它的
狀態 vocabulary——這個驗證缺口在當時的交接記錄裡被點名。本檔補上一手證據。

跑法：`herdr --session probe` 起在一個拋棄式 tmux window，CLI 端以
`HERDR_SESSION=probe herdr <noun> <verb>` 走該 session 的 socket
（**具名 session 有自己的 socket**：`~/.config/herdr/sessions/<name>/herdr.sock`；
不帶 `HERDR_SESSION` 會打到 default socket 並回 `NotFound`）。

## 0. 定位（官網原文）

> Agent multiplexer · a binary, not an app
> Run all your coding agents from one terminal, on any box, even over ssh.
> Each runs in its own real terminal, on a server that keeps it alive when you
> close the laptop. See blocked, working, and done at a glance.

即：它是**終端承載層**（PTY server／layout／attach／remote），要換掉的是
tmux。ab 是任務郵箱＋跨 runtime 生命週期協定，真相在檔案上的 task。
兩者不是同層替代品——這一點實裝後與文件判斷一致，不修正。

## 1. 狀態詞彙 —— 五態，逐字

`herdr agent wait --until <STATUS>` 的值域即為權威列舉：

```
[possible values: idle, working, blocked, done, unknown]
```

`herdr agent list` 的每個 agent 帶 `agent_status`；`herdr pane list` 的 pane
也帶 `agent_status`（無 agent 時為 `unknown`）；`herdr workspace list` 的每個
workspace 帶**聚合**後的 `agent_status`。

## 2. 偵測機制 —— **主力是有紀律的刮螢幕，不是 hook**

這一條**推翻**了先前只看文件時的判斷（「blocked 必須有 hook 證據，不得靠
screen scraping 猜」）。實測結構如下。

### 2.1 版號化的 per-runtime manifest

判定規則不寫死在程式裡，而是**遠端下載、有版號的 TOML**，本機快取在
`~/.local/state/herdr/agent-detection/remote/<kind>.toml`。量測當下拿到 19 份：

```
agy amp claude cline codex copilot cursor devin droid gemini
grok hermes kilo kimi kiro maki opencode pi qodercli
```

`claude.toml` 前言：

```toml
id = "claude"
version = "2026.07.13.1"
min_engine_version = 2
updated_at = "2026-07-13T00:00:00Z"
aliases = ["claude-code"]
```

`explain` 回報 `manifest_source = "remote:…/claude.toml"`、
`manifest_version = "2026.07.13.1"`、`local_override_shadowing_remote = false`
——即支援本機覆蓋且會明說有沒有覆蓋掉遠端。

### 2.2 規則的形狀

每條規則指定 **state ＋ priority ＋ region ＋ 比對器**：

```toml
[[rules]]
id = "osc_title_working"
state = "working"
priority = 1100
region = "osc_title"
visible_working = true
regex = ['^[\x{2800}-\x{28FF}] ']      # 盲文 spinner 字元開頭
```

實測出現過的 region：`osc_title`、`osc_progress`、`prompt_box_body`、
`after_last_horizontal_rule`、`bottom_non_empty_lines(N)`、`whole_recent`。
比對器：`contains` / `regex` / `line_regex` / `any = [...]` / `not`。

值得注意的旗標：

- `visible_blocker` / `visible_working` / `visible_idle`：規則命中時同時點亮的
  聚合旗標，與最終 state 分開。
- `skip_state_update = true`（`transcript_viewer` 規則用）：畫面當下**讀不出
  真實狀態**（使用者開著 transcript 檢視），此時 MUST NOT 更新狀態、沿用上一個
  ——把「不知道」與「維持原判」分成兩件事。

### 2.3 hook integration 是**可選加強**，不是前提

`herdr integration status` 列出 14 家可裝的整合，各自寫進該 runtime 自己的
hooks 目錄：

```
claude:   ~/.claude/hooks/herdr-agent-state.sh
codex:    ~/.codex/herdr-agent-state.sh
opencode: ~/.config/opencode/plugins/herdr-agent-state.js
…（pi / omp / copilot / devin / droid / kimi / kilo / hermes / qodercli /
   cursor / mastracode）
```

**本次量測全部在「一個 integration 都沒裝」的狀態下進行**（`claude` 那條的
落點在 chezmoi 管的 `~/.claude/hooks/`，不動）。即使如此，狀態照樣判得出來
——見下節。

## 3. 判定過程可稽核 —— `agent explain`

`herdr agent explain <target> --json` 是一等公民的除錯面，吐出：

| 欄位 | 內容 |
|---|---|
| `evaluated_rules[]` | **全部**規則的評估結果：`id` / `priority` / `region` / `state` / `matched` / `evidence` |
| `evidence` | `contains` / `regex` / `line_regex` 各自的命中數、`region_bytes`、`region_preview`（實際被掃到的那段文字） |
| `matched_rule` | 勝出的那條 |
| `state` | 最終判定 |
| `visible_blocker` / `visible_idle` / `visible_working` | 聚合旗標 |
| `manifest_source` / `manifest_version` / `local_override_shadowing_remote` | 判定用的是哪份規則、哪個版本 |
| `screen_detection_skipped` / `skip_state_update` / `skipped_update_reason` / `fallback_reason` / `warning` | 為什麼沒判／降級原因 |

實測一份 idle 的 claude，12 條規則的評估結果（依 priority 排）：

```
[ 1100] osc_title_working          state=working  region=osc_title                  matched=False
[ 1000] transcript_viewer          state=unknown  region=bottom_non_empty_lines(3)  matched=False
[  980] live_blocked_form          state=blocked  region=after_last_horizontal_rule matched=False
[  980] dynamic_workflow_prompt    state=blocked  region=whole_recent               matched=False
[  975] btw_overlay_working        state=working  region=bottom_non_empty_lines(5)  matched=False
[  950] live_prompt_box            state=idle     region=prompt_box_body            matched=True   ← 勝出
[  900] model_picker_menu          state=unknown  region=whole_recent               matched=False
[  850] bash_permission_prompt     state=blocked  region=whole_recent               matched=False
[  840] generic_permission_prompt  state=blocked  region=after_last_horizontal_rule matched=False
[  300] legacy_no_prompt_blocker   state=blocked  region=whole_recent               matched=False
[  250] osc_title_idle             state=idle     region=osc_title                  matched=True
[  250] osc_progress_idle          state=idle     region=osc_progress               matched=False
```

### 3.1 離線畫面也能跑判定

`herdr agent explain --file <畫面檔> --agent <kind>` 對一份靜態畫面文字跑同一
套規則——**即可寫 hermetic 測試**，不必真的把 agent 開進某個狀態。

## 4. 實測：狀態轉換與延遲

claude agent（**未裝 integration**）以 `herdr agent prompt <name> <text> --wait`
送出一則會用到 Bash 工具的 prompt，以 0.5s 取樣 `agent_status`：

| 時間（epoch） | 狀態 |
|---|---|
| 1785493982.327 | `idle` |
| 1785493983.891 | `working`（送出後 **1.56s**） |
| 1785493991.176 | `idle`（**7.29s** 後回到 idle） |

判定：輪詢式、非事件推播；秒級延遲。`agent start` 當下即回報
`agent_status: "idle"` 與 `interactive_ready: true`，並用 `terminal_title`
（OSC 序列，量測到 `"✳ Claude Code"`，送出 prompt 後變成
`"✳ Create temporary probe marker file"`）當其中一路訊號。

本次未取得 live `blocked` 樣本（探測用的 Bash 指令被既有 permission 設定自動放行）；
`blocked` 改以離線畫面驗證，見下節。

## 5. 交叉驗證：拿 **ab 自己的**權限框畫面打 herdr 的 matcher

用 `explain --file`，餵入我們 spec／測試裡逐字保存的兩份權限框畫面：

| 畫面 | manifest | 判定 | 勝出規則 |
|---|---|---|---|
| agy 權限框 | `agy` | `blocked` | `permission_prompt` |
| agy 權限框 | `claude` | `blocked` | `legacy_no_prompt_blocker`（priority 300 的兜底） |
| claude 權限框 | `claude` | `blocked` | `bash_permission_prompt` |
| claude 權限框 | `agy` | **`idle`（漏判）** | — |

→ **manifest 綁 runtime，配錯 runtime 就是偽陰性**，失敗方向與 ab 的
`screen_has_prompt` 相同（漏判＝放行送鍵）。

herdr 的 agy 規則：

```toml
[[rules]]
id = "permission_prompt"
state = "blocked"
priority = 300
region = "whole_recent"
visible_blocker = true
contains = ["requesting permission for:"]
any = [
  { contains = ["do you want to proceed?"] },
  { contains = ["tab amend", "edit command"] },
]
```

→ 與 ab 的 AGY-PROMPT-1 修補**獨立收斂到同一個錨**（`Requesting permission
for:`），且同樣採「header 錨 ＋ 成對備援」的形狀。這是對該修補的第三方佐證。

## 6. 平行專案：herdr 的「spaces」

TUI 側欄按 workspace 分組，標籤是**專案名＋branch**（量測當下自動認出
`agent-bridge` / `rust/m0.5`）。`herdr workspace list` 每個 workspace 帶：

```json
{ "workspace_id": "w1", "label": "agent-bridge", "number": 1,
  "agent_status": "idle", "pane_count": 1, "tab_count": 1,
  "active_tab_id": "w1:t1", "focused": true }
```

即 **workspace 層有聚合狀態**——「這個專案整體現在是什麼狀態」是一等公民，
不必自己把 worker 列表折疊起來看。

## 7. 架構觀察：TUI 只是 socket API 的一個 client

所有東西都有 `herdr <noun> <verb>` 的 CLI 形並吐 JSON：
`agent list|get|read|send-keys|prompt|rename|focus|wait|attach|start|explain`、
`pane list|get|read|split|swap|move|zoom|resize|…`、`workspace`／`worktree`／
`tab`／`notification`／`session`／`api`。官方文件亦建議：shell orchestration 與
human debugging 走 CLI，raw socket 只給長連線事件訂閱。

## 8. 判定（對 ab 的意涵）

只記本次量測直接支持的結論：

1. **狀態 vocabulary 可以照抄五態**（`idle/working/blocked/done/unknown`），
   這是實測過的權威列舉，不是文件推測。
2. **「不得刮螢幕」這條先前共識要修正**。真正的分野不是「刮不刮」，而是
   **刮得可不可稽核、規則能不能跟著 runtime 版本走**。herdr 的三件套是：
   版號化 manifest（規則是資料不是程式碼）＋ 限定 region（不是整屏亂比）
   ＋ `explain` 把判定過程攤開。ab 現有的 `screen_has_prompt` 是硬編碼版本，
   靠 canary 測試對付文案漂移。
3. **`skip_state_update` 的區分值得抄**：「畫面現在讀不出來，維持上一個判定」
   跟「狀態未知」是兩件事，混在一起會讓 UI 抖動。
4. **workspace 層聚合狀態**是平行專案視圖的關鍵原語，不是把 worker 列表分組
   就等於有了。
5. `explain --file` 這種「對離線畫面跑判定」的介面，讓偵測規則變成可寫
   hermetic 測試的東西——ab 若要 manifest 化，這個介面應一起做，否則規則變成
   一坨測不到的資料。

## 9. 未量測 / 缺口

- **未裝任何 integration hook**：hook 路徑的狀態精度、延遲、與刮螢幕路徑的
  優先序關係**沒有量測**。`claude` 的落點在 chezmoi 管的 `~/.claude/hooks/`，
  刻意不動。
- **未取得 live `blocked` 樣本**：`blocked` 只用離線畫面驗證過。
- **未量測多 workspace 的實際使用**：本次只有一個 workspace（一個專案）。
  「平行專案時側欄好不好用」沒有一手證據。
- **未量測 remote / SSH attach、plugin 生態、mobile client**。
- 未測 `min_engine_version` 不符時的降級行為（`claude.toml` 要求 2，`agy.toml`
  要求 1）。

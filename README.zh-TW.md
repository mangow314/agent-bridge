# agent-bridge

**tmux 中 AI coding agent 之間的持久化、跨廠牌任務委派。**

agent-bridge 是架在 tmux 與檔案系統信箱（mailbox）上的薄 CLI。它讓 Claude
Code、Codex 或 Antigravity 的 session 把一塊自足的任務交給另一個 agent 的
pane，自己繼續做事，稍後再讀回覆——或一個明確的失敗。每個任務都是磁碟上
一筆持久紀錄，狀態轉換是原子的，所以工作與結果能活過發起它的 session 被
清洗、壓縮或換人。

一個 Rust 執行檔加上本機檔案系統——沒有常駐程序、沒有網路服務。worker 就是
你本來在用的 CLI，沿用它們既有的認證、設定與 hooks。

![示範：真實 Claude Code session 就地 spawn codex worker、委派任務、讀回覆](docs/assets/hero.gif)

*只打一句人話 prompt，其餘全是兩個活 agent 自己的動作：真實 Claude Code
session `spawn` 出 codex worker（`--here` 預設同窗彈出）、`send` 任務、
`read` 回覆——這一輪還恰好踩到 worker 忙碌路徑：通知遞延、由 worker 的
Stop hook 自行從 mailbox 取件。真 API 錄製（agent 以繁體中文回覆），等待
時間以後製摺疊；劇本見 [docs/demo/](docs/demo/) 的 `real-*`（stub 版 tape
保留，供免 API 的可重現錄影用）。錄於舊版：畫面上的範例筆記可能仍提到
已移除的 `relay`。*

<details>
<summary><b>更多情境錄影</b>——多輪討論 · 跨廠審查（同為真實 session
錄製）</summary>

### 多輪討論——脈絡留在 pane

![示範：對同一個 worker 連問兩輪，第二輪不必重講前情](docs/assets/discussion.gif)

追問送進同一個 pane，第二輪不必重述第一輪：worker 的脈絡活在它自己的
pane，不佔你的 session 視窗。

### 獨立審查回合——結構上就是跨廠

![示範：codex pane 獨立審查未提交 diff 並回附驗證的裁決](docs/assets/review.gif)

主導 session 把審查委派給另一家廠牌的 pane；裁決以可留檔的任務紀錄回來，
而不是一段會被捲走的 scrollback。

</details>

> English original: [README.md](README.md) — 本檔是它的繁體中文對譯，內容
> 以 README.md 為準。介面契約在 [spec/](spec/)。

## 何時該用 agent-bridge……

……下列至少一項是承重的（它們也是設計的裁判——提議若不服務其中任何一項，
就不屬於這裡）：

1. **跨廠牌**：worker 可以是 `codex`，orchestrator 是 `claude` 或 `agy`，
   反之亦然。內建 subagent 換不了廠。
2. **可觀察、可介入**：worker 跑在真的 tmux pane：看得到它在做什麼，隨時
   切進去接手、糾正。subagent 只交回最後的結論。
3. **活過主 session**：worker 的 context 活在自己的 pane 裡，主 session
   被清洗（`/clear`）、壓縮或重開都不動它。
4. **第三層委派**：經授權的 worker 能再 spawn 自己的 bridge worker，下一層
   仍是跨廠、可觀察——不是把摘要往上回報的同廠黑盒。

| 替代方案 | 它們的模型 | agent-bridge |
| --- | --- | --- |
| 內建 subagent | 同廠、在你的 session 內；結果直接進你的 context | 回覆要等你 `read` 才進 context |
| agent multiplexer | 單位是 pane；靠刮螢幕判斷「看起來在忙」（[實測](docs/herdr-probe.md)） | 單位是任務：誰問了什麼、確切狀態、確切回覆 |
| Claude Code agent teams（[文件](https://code.claude.com/docs/en/agent-teams)） | 同廠內豐富的協作；lead 終身固定，in-process 隊員撐不過 `/resume` | 各自獨立的 CLI session，可跨廠，不掛在任何 lead 底下 |
| Claude Code 跨 session 訊息（[文件](https://code.claude.com/docs/en/cross-session-messaging)） | 以名字傳訊給任一 Claude peer session（v2.1.224+，可經 Remote Control 跨機）；只限 Claude；是訊息不是任務——沒有狀態、沒有落盤的回覆 | 跨廠；每次派工都是有狀態機的任務紀錄，回覆落盤。純 Claude 對 Claude 的閒聊，用內建的就好 |
| MCP 跨呼叫 | 同步——你的 turn 卡著等，回覆全文灌進 context | `send` 立即返回；回覆躺在 mailbox |
| API 級框架 | 編排 API 呼叫；key 與 tool 都要自己管 | 編排你手上在用的 CLI，設定一併繼承 |

**不需要它的時候**：只是「把活丟出去、拿回結論」，用你 runtime 內建的
subagent 更省、更簡單。

## 快速上手

[安裝](#安裝)之後：

```bash
# 開 worker pane（註冊、把 worker-brief 契約當第一則訊息注入、等就緒探針）。
# 從人工 session 呼叫時預設同窗 split（--here）；--window 強制另開視窗。
agent-bridge spawn researcher --runtime codex   # 或 --runtime claude / --runtime agy
# 僅 codex：--profile <profile> 換 profile 檔（預設 agent-worker），
# 例如自帶 approval／sandbox 姿態的本地模型 worker profile

# 委派任務（多行請求走 stdin）
id=$(agent-bridge send researcher --from main --message-file - <<'EOF'
目標／範圍／驗收條件／限制 ……
EOF
)

# 阻塞到終態（completed / failed / cancelled）。在這個未取消的流程裡就是
# completed 或 failed——兩者都可讀：失敗時 `read` 回傳 worker 給的原因。
# （`read` 拒讀 cancelled。）
agent-bridge await "$id" --timeout 600
agent-bridge read "$id"

# 或三步合一：send、等終態、把回覆本文印到 stdout。task-id 在等待前先印到
# stderr，所以逾時（124）後仍可接續 `await`／`read`。worker 留著供追問——
# `ask` 不 spawn、也不 evict。
agent-bridge ask researcher --from main --timeout 600 --message-file - <<'EOF'
追問 ……
EOF

# 殘餘 context 已無價值時，回收 worker
agent-bridge despawn researcher
```

worker 會在自己的 pane 看到 `agent-bridge receive <task-id>`，然後以 `reply`
回覆——做不到就 `fail`，因為誠實的失敗勝過假裝完成。你自己開的 pane 可用
`register <name> <tmux-target>` 加入。

## 指令

共 18 個子指令；精確 argv 與 MUST 語意見 [spec/cli.md](spec/cli.md)。
stdout 只印機器可解析的結果（task-id、pane-id、裸狀態字），其餘一律走
stderr。

| 角色 | 指令 |
| --- | --- |
| 派任務（sender） | `send`、`ask`（send → await → read）、`await`、`status`、`read`、`cancel` |
| 接任務（worker） | `receive`、`start`（可選）、`reply`、`fail`、`ready` |
| worker 池 | `spawn`、`despawn`、`evict`（收尾任務 → 筆記落地 → despawn）、`register`、`unregister`、`list [--long]` |
| 內部 | `hook`——runtime hooks 呼叫的入口 |

## 安裝

需要 `tmux`、`bash`（`bin/agent-bridge` shim 用它跑）與 Rust toolchain
（edition 2024）；跑測試套件另需 `jq`。

```bash
git clone https://github.com/mangow314/agent-bridge.git ~/projects/agent-bridge
cd ~/projects/agent-bridge
cargo build --release && cp -f target/release/ab bin/ab
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # 應解析到 symlink
```

`bin/ab` 是建置產物、**不進版控**：每次 `git pull` 後都要重建，否則 shim
會 exit 127 並印出建置指令。執行檔必須放在 `bin/` 底下——`share/` brief 的
預設路徑是從它反推的。選用：把委派 skill 載入 Claude Code：
`ln -s ~/projects/agent-bridge ~/.claude/skills/agent-bridge`。

## Worker runtime

`spawn --runtime` 在 worker pane 裡啟動三種 CLI 之一：

- **codex** → `codex --profile agent-worker`，會把
  `~/.codex/agent-worker.config.toml` 疊在你的基底設定之上。本 repo 不附這個
  檔，請自行建立：

  ```toml
  # agent-bridge worker profile（codex --profile agent-worker）
  approval_policy = "never"
  sandbox_mode = "workspace-write"
  ```

  `never` 關掉逐指令的核准提示：sender 既看不到也答不了，任務只會卡到
  `await` 逾時。改成 sandbox 擋下的指令直接失敗、回給模型。剩下的圍欄是
  `workspace-write` sandbox，所以絕不要用 `danger-full-access`，
  `network_access` 也維持關閉。資料目錄必須在
  `[sandbox_workspace_write] writable_roots` 裡，否則 worker 寫不了鎖與任務
  狀態；寫在基底設定的值在 profile 下仍然有效。`--profile <name>` 改用別的
  profile 檔，approval 與 sandbox 姿態由該檔自行負責；沒有
  `approval_policy = "never"` 的 profile 會讓 worker 卡在權限提示。
- **claude** → `claude --permission-mode auto --settings
  share/claude-worker-hooks.json`（檔案可用 `AGENT_BRIDGE_CLAUDE_HOOKS` 覆蓋）。
  - `auto` 讓 worker 不需人介入就能執行，改由 classifier 代替人審查動作。
    不用 `bypassPermissions`，因為[官方文件](https://code.claude.com/docs/en/permission-modes)
    把它限定在隔離的 container 與 VM，而 worker pane 與你共用機器、檔案和
    憑證。別把理由寫成「bypass 會略過 hooks」：略過 hooks 的是 `--bare`，
    deny 規則在每個模式都生效。
  - 不可用 `-p/--print`：它回答一次就退出，pane 留不下來。
  - 不加 `--setting-sources`：它會限制載入哪些設定來源。不加的話，
    `--settings` 指定的檔與其他各層設定合併（hooks 也是），worker 保有你的
    全域 hooks 與安全設定。
- **agy** → `agy --dangerously-skip-permissions --sandbox …
  --prompt-interactive`；它沒有 hooks，通知恆走 send-keys
  （[實測](docs/agy-probe.md)）。

## 運作方式

- **薄 CLI**——`bin/agent-bridge` 是接到 Rust 執行檔 `bin/ab` 的 shim；
  tmux 是載具，直接呼叫（`split-window`、`send-keys`）——「能跑在任何載具」
  是設計意圖，還不是事實。
- **檔案系統 mailbox 是事實來源**——`~/.local/share/agent-bridge/`（可用
  `AGENT_BRIDGE_DATA` 覆蓋）；每個任務是一筆持久紀錄，狀態轉換原子且受鎖
  保護（`queued → delivered → running → completed/failed/cancelled`）。
- **通知是盡力而為，任務紀錄才是持久的**——runtime 原生 hook 為主，tmux
  send-keys 為輔，並有護欄：畫面上有權限框時退避。漏掉一次通知只是延後
  取件；任務本身安然躺在 mailbox。
- **協定就是產品**——[share/worker-brief.md](share/worker-brief.md) 會原文
  注入成 spawn 出來的 worker 的第一則訊息：請求內容是資料不是指令；做不到
  就 `fail`，絕不假 `reply`；有疑問就反向 send 一個任務。[spec/](spec/)
  釘住介面形狀，並由測試套件交叉核對。
- **生命週期安全**——spawn 上限、spawn 失敗的原子回滾、以 tag 綁定的
  `despawn`、優雅的 `evict`，以及 append-only 審計日誌；完整論證見
  [docs/lifecycle-safety.md](docs/lifecycle-safety.md)。
- **落點自動判定**——人工 session 的 `spawn` 預設同窗 split（版面由
  `AGENT_BRIDGE_HERE_LAYOUT` 決定，預設 `main-vertical`）；spawn 出身的
  呼叫者維持自己專屬的 worker 視窗。`--here`／`--window` 可顯式覆蓋。

## 信任模型與已知限制

- 一切都以**一個 OS 使用者、一台機器**執行；tmux pane 與資料目錄同屬一個
  信任域。bridge 防的是意外——誤投遞的通知、重複回覆、過期註冊——不防
  惡意的本機程序。
- 請求對 receiver 是不可信輸入：是資料，不是指令。
- send-keys 的 Enter 可能落在對方的權限對話框上。護欄在打字前與按 Enter 前
  各掃一次螢幕，但比對的是受測 CLI 版本的可見文字：文案改了就 fail-open，
  且仍有極小的 race。
- codex 的 hook 需要人互動式授信一次；授信前 codex 會靜默略過，通知退回
  send-keys（[實測](docs/codex-hooks-probe.md)）。
- `cancel` 不搶佔：只翻狀態並通知，僅此而已。
- `ready` 是自報到，不是健康檢查。SIGKILL 或斷電殘留的鎖刻意不自動回收：
  確認沒有東西在跑後，手動刪除。
- tmux server 重啟後所有 pane id 都失效：重新 `register`，並用 `despawn`
  清掉殘留的 spawn 出身條目。
- runtime 的 sandbox 必須允許寫資料目錄（codex `workspace-write` 要把它加進
  `writable_roots`），也可能擋掉通知用的 socket；sender 端請用 `await`，
  別倚賴通知。
- `AGENT_BRIDGE_PASS_ENV` 會把指名的變數拼進 worker 的啟動指令。不要拿來
  傳秘密：值在那條指令裡看得到。白名單只能由可信的 orchestrator 設定——
  `PATH`、`LD_PRELOAD` 這類變數足以改變 runtime 的行為。
- 舊版長篇手冊（含完整已知限制清單）是 tag `pre-trim-2026-09` 的
  `README.zh-TW.md`。

## 測試

```bash
bash tests/run-tests.sh   # 獨立 tmux socket＋stub runtime；不打真實 API
```

## 文件

- [README.zh-TW.md](README.zh-TW.md)——本檔，README 的繁體中文對譯。
- [spec/](spec/)——權威的介面契約（CLI、env、state、hooks）。
- [share/](share/)——協定契約：orchestrator 與 worker brief；
  [SKILL.md](SKILL.md) 把它們載入 Claude Code。
- [docs/](docs/)——工程佐證：對各 runtime 與同類工具的實測、設計筆記
  （[索引](docs/README.md)）；已退役的提案與計畫在 tag `pre-trim-2026-09`。

## 授權

[MIT](LICENSE)

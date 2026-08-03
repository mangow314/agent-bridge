//! Page 層（`docs/tui-design.md` §1 兩層架構的**上層**）：把「需要人現在出手」
//! 的事件推到人面前，**不需要打開任何東西**。
//!
//! 與 `notify` 模組是兩件事，別混：`notify` 是 **agent 對 agent** 的 send-keys
//! 通知（含 `screen_has_prompt` 這道避免替人按下批准的安全護欄）；這裡是
//! **機器對人**的呼叫器（pager）。
//!
//! **事件恰兩類**（§1 明訂，不得擴充）：
//!
//! - `TaskFailed`：task 進 fail 終態。純磁碟事件，由寫盤的那個 CLI 行程同步發。
//! - `WorkerDied`：pane 死了但還掛著非終態 task。tmux＋磁碟事件。
//!
//! **不上 daemon、不開 socket**（§1 非目標）：沒有常駐行程，事件一律由「正在
//! 寫盤的那個 CLI 行程」順手發出去。
//!
//! # 為什麼推播失敗不是錯誤
//!
//! 呼叫端（`fail`／`reply`／`scan`…）的既有 invocation 語意**零改變**是 §1 的
//! 硬非目標。推播是副作用：notify-send 不在、tmux 沒起來、使用者的自訂命令
//! 掛了——一律只影響「人有沒有被叫到」，不影響 exit code 與 stdout 契約。
//! 事件本身照樣落盤（rubric v2 page 層條 4），所以事後回頭查得到。
//!
//! 保證的強度是 **durable record ＋ 至多嘗試推播一次**，不是 exactly-once；
//! crash window 明列在 `emit` 的文件裡。唯一不落盤的情形是 key 本身不可信
//! （`key_is_frameable`）。

use std::fs::OpenOptions;
use std::io::Write;

use crate::error::{Error, Result};
use crate::lock::acquire_lock;
use crate::paths::Paths;
use crate::time::now_iso;

/// 事件流檔名（append-only，一行一 JSON）。
pub const EVENTS_FILE: &str = "page-events.jsonl";
/// 已處理過的 event key（一行一個）。
pub const SEEN_FILE: &str = "page-seen";
/// 去重與寫入共用的鎖 id（`locks/page.lock`）。
const LOCK_ID: &str = "page";

/// 需要人出手的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageEvent {
    /// task 進 fail 終態。
    TaskFailed { agent: String, task: String },
    /// pane 不在了，但這個 agent 還掛著非終態 task。
    ///
    /// `spawn_tag` 是 generation key（P4.7）：把它放進 event key 是為了
    /// **同名 respawn 不重推**——新一代的同名 agent 是另一個事實，舊那一代的
    /// 事件不該因為名字被重用而重放（rubric v2 page 層條 2）。
    WorkerDied {
        agent: String,
        spawn_tag: String,
        task: String,
    },
}

/// 通知**呈現**用的兩個易變欄位。與 `PageEvent` 分成兩個型別是刻意的：
/// `PageEvent` 是事件的**身分**（去重軸就從它算），這裡是「給人看的補充」。
///
/// 兩者混在同一個型別裡，遲早有人把地點寫進 `key()`——那會讓「window 改個
/// 名字」或「worker 換一個 session」變成一則新事件重推一次。分成兩個型別，
/// 那個錯誤就不再是一行之差，而是要跨型別搬欄位（PG4 使用者實測後裁定）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageDetails {
    /// agent 所在位置，`session:window索引 window名` 的形狀（例：`dev:3 build`）。
    /// **PG4 rubric 條 3 的一半**：受測者說得出「誰、出了什麼事」，但「要不要
    /// 切過去看」卡住——通知沒說切去哪裡，那個動作因此沒有起點。
    pub location: Option<String>,
    /// 失敗原因的第一行。**條 3 的另一半**：決定「要不要現在出手」的是原因
    /// 本身，不是「進了 failed 終態」這個狀態字。
    pub reason: Option<String>,
}

/// 通知文字的長度上限（字元數，非 byte）。桌面通知與 tmux status line 都會
/// 自行截斷，與其讓它們攔腰砍在任意位置，不如自己截並補省略號。
const MAX_REASON: usize = 80;
const MAX_LOCATION: usize = 40;

/// 把不可信字串修成「能安全放進一行通知」的樣子。
///
/// 兩個來源都不可信：`reason` 來自 worker 寫的 response.md（架構 §3 的 payload
/// 紅線），`location` 來自 tmux 的 window 名（使用者或任何程式改得到）。控制
/// 字元會把單行通知撐成多行、把 tmux status line 洗掉；截斷一律按**字元**，
/// 按 byte 切會把多位元組字元剖半。
fn one_line(s: &str, max: usize) -> Option<String> {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= max {
        return Some(trimmed.to_string());
    }
    Some(trimmed.chars().take(max).collect::<String>() + "…")
}

impl PageDetails {
    /// 從失敗訊息取原因：**第一個非空行**。worker 的失敗訊息常是多行（stack
    /// trace、指令輸出），第一行通常就是那句話。
    pub fn with_reason(mut self, message: &str) -> Self {
        self.reason = message.lines().find_map(|l| one_line(l, MAX_REASON));
        self
    }

    fn location_suffix(&self) -> String {
        match &self.location {
            Some(l) => format!(" · {l}"),
            None => String::new(),
        }
    }
}

/// 這串真的是一個地點，還是 tmux 對著不存在的目標吐出來的空殼。
///
/// **`display-message` 對已死的 pane 是 exit 0 ＋ 空展開**（實測 tmux 3.7b，
/// 分組 47 逮到）：格式裡每個欄位都空掉，結果是 `":"`。它非空、無控制字元，
/// 於是一路通過 `one_line`，被當成有效地點回傳——最需要 fallback 的
/// `WorkerDied`（pane 依定義已經死了）因此永遠停在第一條路徑上，window 那條
/// 根本走不到，通知裡的地點是一個冒號。
///
/// 判準取 `window_index` **是數字**：那是這個格式裡唯一有形狀可驗的欄位，
/// session 名與 window 名都可以是任意字串。
fn plausible_location(s: &str) -> bool {
    let Some((session, rest)) = s.split_once(':') else {
        return false;
    };
    let index = rest.split(' ').next().unwrap_or("");
    !session.is_empty() && !index.is_empty() && index.chars().all(|c| c.is_ascii_digit())
}

/// agent 現在人在哪：`session:window索引 window名`。
///
/// 先問 pane，pane 沒了（`WorkerDied` 的常態）再退到 registry 的 `owner` 欄
/// ——它的形狀是 `session:@winid`，window 通常還活著。兩條都問不到就回 `None`，
/// 通知照發、只是少一段地點（**推播失敗不得改變呼叫端行為**，少一個欄位更
/// 不該讓事件發不出去）。
///
/// 每則事件多一次 bounded tmux 呼叫。事件本來就罕見（掃到才發、發過不重發），
/// 而 `scan` 只對**已判定死掉**的 agent 走這裡——健康的池子一次都不會問。
pub fn resolve_location(
    tmux: &dyn crate::tmux::TmuxClient,
    pane: &str,
    owner: &str,
) -> Option<String> {
    const FMT: &str = "#{session_name}:#{window_index} #{window_name}";
    let ask = |target: &str| {
        tmux.exec(&["display-message", "-p", "-t", target, FMT])
            .and_then(|o| o.ok_stdout())
            .and_then(|s| one_line(&s, MAX_LOCATION))
            .filter(|s| plausible_location(s))
    };
    if !pane.is_empty()
        && let Some(loc) = ask(pane)
    {
        return Some(loc);
    }
    // owner＝`session:@winid`；window id 才是穩定的目標（session 可能改名）
    let win = owner.split_once(":@").map(|(_, w)| format!("@{w}"))?;
    ask(&win)
}

impl PageEvent {
    /// 去重軸。同一個 key **至多推一次**，跨行程、跨重啟都算數（key 落在磁碟
    /// 上，不在記憶體裡——行程重啟不重推是 rubric v2 page 層條 2 的一半）。
    /// 「至多」而非「恰」的理由見 `emit` 的保證強度那一節。
    pub fn key(&self) -> String {
        match self {
            PageEvent::TaskFailed { task, .. } => format!("failed:{task}"),
            PageEvent::WorkerDied {
                agent,
                spawn_tag,
                task,
            } => format!("died:{agent}:{spawn_tag}:{task}"),
        }
    }

    /// 事件類別字串，進事件流的 `kind` 欄位。
    pub fn kind(&self) -> &'static str {
        match self {
            PageEvent::TaskFailed { .. } => "task-failed",
            PageEvent::WorkerDied { .. } => "worker-died",
        }
    }

    /// 通知標題：**誰、出了什麼事、人在哪**。
    ///
    /// 三件事都在標題，是因為 PG4 實測（2026-08-03）打出來的：受測者答得出
    /// 「哪個 agent」與「失敗還是死了」，卻卡在「要不要切過去看」——通知從
    /// 沒說過切去哪裡。地點解不出來就整段省略，不留空殼。
    ///
    /// **不再自帶 `agent-bridge:` 前綴**：來源改由各通道用自己的慣用法標記
    /// ——桌面通知走 `notify-send -a agent-bridge`（appname 欄），tmux status
    /// line 由 `SystemPager` 自己接前綴。前綴擠在標題裡只是把最貴的前幾個字
    /// 讓給一段每則都一樣的字。
    pub fn title(&self, d: &PageDetails) -> String {
        let at = d.location_suffix();
        match self {
            PageEvent::TaskFailed { agent, .. } => format!("{agent} 任務失敗{at}"),
            PageEvent::WorkerDied { agent, .. } => format!("{agent} 死了{at}"),
        }
    }

    /// 通知內文：**第一行答「要不要現在出手」，第二行是 task id**。
    ///
    /// 失敗的那句原因就是決策依據（PG4 使用者裁定）；狀態字「進了 failed
    /// 終態」不是——它只是把標題再說一次。原因取不到才退回狀態字。
    ///
    /// task id 留在最後一行而不是拿掉：它是 `ab read <id>` 的把手，拿掉就
    /// 複製不到，而同一個 agent 同時有多筆 task 時也對不起來。
    pub fn body(&self, d: &PageDetails) -> String {
        let task = self.task();
        match self {
            PageEvent::TaskFailed { .. } => match &d.reason {
                Some(r) => format!("{r}\n{task}"),
                None => format!("已進 failed 終態，回覆在信箱\n{task}"),
            },
            PageEvent::WorkerDied { .. } => {
                format!("pane 已不存在，這一筆沒人會回\n{task}")
            }
        }
    }

    fn agent(&self) -> &str {
        match self {
            PageEvent::TaskFailed { agent, .. } | PageEvent::WorkerDied { agent, .. } => agent,
        }
    }

    fn task(&self) -> &str {
        match self {
            PageEvent::TaskFailed { task, .. } | PageEvent::WorkerDied { task, .. } => task,
        }
    }
}

/// 把一則事件送到人面前的管道。實作在 PG3（分層 ladder）；抽成 trait 是為了
/// 測試能塞一個「一定失敗」的假管道，證明推播壞掉時事件照樣落盤。
pub trait Pager {
    /// 回 `true`＝至少送出一則。**回 `false` 不是錯誤**，呼叫端不得因此改變
    /// 行為（見模組說明）。
    fn page(&self, title: &str, body: &str) -> bool;
}

/// 什麼都不做的管道：只落盤、不推播。給測試與「使用者只要事件流」的情境。
pub struct NullPager;

impl Pager for NullPager {
    fn page(&self, _title: &str, _body: &str) -> bool {
        false
    }
}

/// 跑一個外部命令。抽成 trait 只為了測試——測試要看得到 argv，而不是真的去
/// 彈使用者的桌面通知。
pub trait CmdRunner {
    /// 回 `true`＝命令跑起來且退出碼為 0。
    fn run(&self, argv: &[String]) -> bool;
}

/// 自訂 notifier 的執行上限。**沒有解除的逃生口**（對比 tmux 那條的
/// `AGENT_BRIDGE_TMUX_TIMEOUT=0`）：一支要跑超過五秒的「通知」不是通知，而
/// 且它卡住的是一個正在寫盤的原子指令——那正是跨廠複核 blocker 1 的形狀。
const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 真的去跑。
///
/// **三個 stdio 全部接 null**（跨廠複核 blocker 1）：`Command::status()` 預設
/// **繼承**三者，而機會式掃描跑在 dispatch 路徑上——`send/reply/fail
/// --message-file -` 的 payload 會先被使用者的自訂 notifier 讀走，指令照樣
/// 成功但落盤內容已被截短。stdout／stderr 接 null 另有一層理由：推播的雜訊
/// 不得混進呼叫端的輸出契約（§1 非目標）。
///
/// **有界**（同一個 blocker 的另一半）：`.status()` 會無限期等，一支不退出的
/// 自訂命令能讓原子指令永遠沒有終局。逾時＝殺掉＋收屍＋視同失敗。
pub struct SubprocessRunner;

impl CmdRunner for SubprocessRunner {
    fn run(&self, argv: &[String]) -> bool {
        let Some((prog, rest)) = argv.split_first() else {
            return false;
        };
        let Ok(mut child) = std::process::Command::new(prog)
            .args(rest)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        crate::tmux::wait_with_timeout(&mut child, Some(NOTIFY_TIMEOUT))
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// 決定走哪一層 ladder 所需的環境事實。獨立成結構是為了測試能直接餵情境，
/// 不必去動行程的真實環境變數（測試平行跑時改 env 是共享狀態，會互相打架）。
#[derive(Debug, Clone, Default)]
pub struct PagerEnv {
    /// `AGENT_BRIDGE_NOTIFY_CMD`：使用者自訂命令。
    pub notify_cmd: Option<String>,
    /// 有沒有本機圖形 session（`WAYLAND_DISPLAY` 或 `DISPLAY`）。
    pub has_display: bool,
    /// 是不是從 SSH 連進來的（`SSH_CONNECTION`）。
    pub over_ssh: bool,
}

impl PagerEnv {
    pub fn from_env() -> Self {
        let non_empty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        PagerEnv {
            notify_cmd: non_empty(crate::config::ENV_NOTIFY_CMD),
            has_display: non_empty("WAYLAND_DISPLAY").is_some() || non_empty("DISPLAY").is_some(),
            over_ssh: non_empty("SSH_CONNECTION").is_some(),
        }
    }

    /// 桌面通知這一層能不能用。
    ///
    /// **SSH 下一律不能**（使用者 2026-08-03 提出）：遠端 session 的
    /// `DISPLAY`／`WAYLAND_DISPLAY` 若有值，指的是**遠端那台**的桌面——通知會
    /// 彈在沒有人的螢幕上，比不通知更糟（人以為自己被通知過了）。這一層於是
    /// 自動跳過，由 tmux 那一層承接；要在遠端接通知請用
    /// `AGENT_BRIDGE_NOTIFY_CMD`。
    pub fn desktop_usable(&self) -> bool {
        self.has_display && !self.over_ssh
    }
}

/// 真正把人叫起來的管道：**分層 ladder，逐層降級而不是靜默壞掉**。
///
/// 1. `AGENT_BRIDGE_NOTIFY_CMD`（使用者自訂）——設了就用它取代桌面那一層。
/// 2. 否則本機桌面 session → `notify-send`。SSH 下跳過（見 `desktop_usable`）。
/// 3. **一律再補一發 tmux status line**：對每個 attached client 送一次
///    `display-message -c <client>`。tmux 3.7b 的 `display-message` 只送
///    **target-client**，不逐個指定就只有一個 client 看得到；而 SSH 進來的
///    client 同樣掛在同一個 server 上，所以這是遠端唯一可靠的一層。
///
/// 三層都沒送出去也只是回 `false`——呼叫端不得因此改變行為。
pub struct SystemPager<'a> {
    pub env: PagerEnv,
    pub runner: &'a dyn CmdRunner,
    pub tmux: &'a dyn crate::tmux::TmuxClient,
}

impl<'a> SystemPager<'a> {
    pub fn new(runner: &'a dyn CmdRunner, tmux: &'a dyn crate::tmux::TmuxClient) -> Self {
        SystemPager {
            env: PagerEnv::from_env(),
            runner,
            tmux,
        }
    }

    /// 第一、二層：自訂命令或桌面通知。回傳送出去了沒。
    fn page_desktop(&self, title: &str, body: &str) -> bool {
        if let Some(cmd) = &self.env.notify_cmd {
            return self
                .runner
                .run(&[cmd.clone(), title.to_string(), body.to_string()]);
        }
        if self.env.desktop_usable() {
            // `-a`：來源掛在 appname 欄，不佔標題的前幾個字（標題最貴的位置
            // 要留給 agent 名與地點）
            return self.runner.run(&[
                "notify-send".to_string(),
                "-a".to_string(),
                "agent-bridge".to_string(),
                title.to_string(),
                body.to_string(),
            ]);
        }
        false
    }

    /// 第三層：tmux status line，逐個 client。
    fn page_tmux(&self, title: &str, body: &str) -> bool {
        if !self.tmux.available() {
            return false;
        }
        let Some(out) = self
            .tmux
            .exec(&["list-clients", "-F", "#{client_name}"])
            .and_then(|o| o.ok_stdout())
        else {
            return false;
        };
        // status line 是單行，內文的換行要壓平；來源前綴在這一層自己接
        // （桌面通知那一層走 `-a`，見 `page_desktop`）
        let msg = format!("agent-bridge: {title} — {}", body.replace('\n', " "));
        let mut sent = false;
        for client in out.lines().filter(|l| !l.is_empty()) {
            // `-l` 讓訊息原文照印：事件文字裡的 `#{...}` 不得被當成 tmux format
            // 展開（agent 名與 task id 是不可信輸入）
            let ok = self
                .tmux
                .exec(&["display-message", "-c", client, "-l", &msg])
                .map(|o| o.status_ok)
                .unwrap_or(false);
            sent = sent || ok;
        }
        sent
    }
}

impl Pager for SystemPager<'_> {
    fn page(&self, title: &str, body: &str) -> bool {
        // 兩層都送：桌面通知會被錯過（通知中心堆疊、螢幕鎖著），status line 是
        // 回到終端機時的第二次機會。不是 fallback 而是並行，所以用 `|`
        let desktop = self.page_desktop(title, body);
        let tmux = self.page_tmux(title, body);
        desktop || tmux
    }
}

/// `emit` 的結果。呼叫端通常不看它——它是給測試與 `scan` 的計數用的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitOutcome {
    /// 事件是新的：已落盤，且推播已**嘗試過一次**（`paged` 說明有沒有送出去）。
    Emitted { paged: bool },
    /// 這個 key 先前已處理過——事件流不再多一筆，也不再推一次。
    AlreadySeen,
    /// key 本身不可信（含換行／控制字元），**不落盤也不推**。見 `key_is_frameable`。
    Rejected,
}

/// 一行一個 key 的檔案格式撐得住這個 key 嗎。
///
/// **registry 的 `spawn_tag` 與 agent name 對本程式是不可信輸入**（worker 寫得
/// 到那個檔）。含換行的 tag 會讓 `mark_seen` 把一個 key 寫成兩行，而 `is_seen`
/// 逐行比對永遠比不中原 key——於是每一輪掃描都重新落盤、重新推播，去重整個
/// 失效（跨廠複核 should-fix 1）。`display-message -l` 只防 tmux format 展開，
/// 防不到持久化格式的 framing。
///
/// 這裡 fail-closed：**證明不了就不叫人**。方向與 `scan` 拿不到 pane 清單時
/// 一致——寧可漏一則，不可把一個壞掉的 registry 變成無限的通知源。
fn key_is_frameable(key: &str) -> bool {
    !key.is_empty() && !key.chars().any(|c| c.is_control())
}

/// 發一則事件：**先落盤、後推播**。
///
/// 順序是刻意的，而且鎖的範圍也是：
///
/// 1. 鎖外先讀一次 seen——絕大多數呼叫（沒有新事件）到這裡就結束，不碰鎖。
/// 2. 真的要發時才取鎖，**進鎖後再讀一次** seen：兩個 CLI 行程同時發現同一件
///    事是常態（機會式掃描掛在每個非唯讀子指令上），沒有這次覆核就會推兩遍。
/// 3. 落盤（事件流）→ 記 seen → **放鎖** → 才推播。推播要跑外部命令，可能慢，
///    不能佔著鎖；而且它失敗與否都不該回頭改變已落盤的事實。
///
/// seen 在推播**之前**寫：notifier 若壞掉，這一則就此作罷而不是每次呼叫重推
/// 同一則洗版。事件流裡有它，事後查得到（rubric v2 page 層條 4）。
///
/// # 保證的強度：durable record ＋ **至多嘗試一次**，不是 exactly-once
///
/// （跨廠複核 blocker 2 的更正。先前 spec 寫「恰推一次」，那句話這份實作
/// 兌現不了，而且不是調換兩行寫入順序就能兌現的。）事件流與 seen 是兩次獨立
/// 的 append，兩個 crash window 明擺著：
///
/// - `append_event` 成功、`mark_seen` 之前行程死掉 → 重啟後同一 key 會再落一筆。
/// - `mark_seen` 成功、`pager.page` 之前行程死掉 → 之後永遠 `AlreadySeen`，
///   這一則實際推播 **0 次**（事件流裡仍找得到）。
///
/// 真正的 exactly-once 需要具 ack／idempotency key 的 outbox 協定，而收件端是
/// `notify-send` 或使用者的任意一支腳本——那個保證在這一層根本無從建立。
/// 明講取捨，不冒充完成。
pub fn emit(
    paths: &Paths,
    event: &PageEvent,
    details: &PageDetails,
    pager: &dyn Pager,
) -> Result<EmitOutcome> {
    let key = event.key();
    if !key_is_frameable(&key) {
        return Ok(EmitOutcome::Rejected);
    }
    if is_seen(paths, &key)? {
        return Ok(EmitOutcome::AlreadySeen);
    }
    {
        let guard = acquire_lock(paths, LOCK_ID)?;
        if is_seen(paths, &key)? {
            guard.release();
            return Ok(EmitOutcome::AlreadySeen);
        }
        let r = append_event(paths, event, details).and_then(|()| mark_seen(paths, &key));
        guard.release();
        r?;
    }
    let paged = pager.page(&event.title(details), &event.body(details));
    Ok(EmitOutcome::Emitted { paged })
}

/// 事件流的絕對路徑。
pub fn events_path(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir.join(EVENTS_FILE)
}

/// seen 檔的絕對路徑。
pub fn seen_path(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir.join(SEEN_FILE)
}

/// key 是否已處理過。檔案不存在＝還沒有任何事件，不是錯誤。
fn is_seen(paths: &Paths, key: &str) -> Result<bool> {
    let path = seen_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s.lines().any(|l| l == key)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::new(format!("無法讀取 {}：{e}", path.display()))),
    }
}

/// 追加一行到 append-only 檔。比照 `task::log_event` 的 `OpenOptions::append`。
fn append_line(path: &std::path::Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| Error::new(format!("無法開啟 {}：{e}", path.display())))?;
    f.write_all(line.as_bytes())
        .map_err(|e| Error::new(format!("無法寫入 {}：{e}", path.display())))
}

fn append_event(paths: &Paths, event: &PageEvent, details: &PageDetails) -> Result<()> {
    // 欄位序＝人讀 jsonl 時的掃描序：時間、類別、誰、在哪、哪一筆、去重軸、
    // 給人看的話。`location` 進事件流是為了事後回查「當時它在哪」——window
    // 後來被關掉或改名，這裡仍留著當時解出來的值
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), now_iso().into());
    obj.insert("kind".into(), event.kind().into());
    obj.insert("agent".into(), event.agent().into());
    obj.insert(
        "location".into(),
        details.location.clone().unwrap_or_default().into(),
    );
    obj.insert("task".into(), event.task().into());
    obj.insert("key".into(), event.key().into());
    obj.insert("message".into(), event.body(details).into());
    let line = format!("{}\n", serde_json::Value::Object(obj));
    append_line(&events_path(paths), &line)
}

fn mark_seen(paths: &Paths, key: &str) -> Result<()> {
    append_line(&seen_path(paths), &format!("{key}\n"))
}

/// Page 層事件 2／2：**pane 死了，但還掛著非終態 task**。
///
/// 沒有 daemon（§1 非目標），所以沒有人會「即時」發現 pane 沒了。這支掃描器
/// 由兩個地方叫：顯式的 `agent-bridge scan`，以及每個非唯讀子指令進場時的
/// 機會式呼叫（使用者 2026-08-03 裁定）。代價明擺著：沒人呼叫就沒人發現。
///
/// # 沒有證據就不叫人
///
/// `TmuxClient::pane_exists` 在 tmux 查詢失敗時回 `false`（送鍵路徑的
/// fail-closed 方向）。照抄那個方向到這裡是**錯的**：tmux 一沒起來，整池
/// worker 會同時被判定為死，一次推爆使用者的通知中心。呼叫器的 fail-closed
/// 方向相反——**拿不到 pane 清單就整輪不掃**，寧可晚一輪，不可假警報。
/// 所以這裡自己抓一次 `list-panes -a`，而不是逐 pane 問。
///
/// # 哪些 task 算「掛在它身上」
///
/// 與 TUI 的歸屬判定同一條規則（`ab-tui` `model.rs` 的 `attached()`）：收件人
/// 名字相符**且** task 的 `created_at` 嚴格晚於該 agent 的 `registered_at`。
/// 只比名字的話，同名 respawn 會把上一代的歷史 task 認領過來——那是把一件
/// 不成立的事推到人面前。同秒（相等）＝不可證＝不掛。
///
/// 兩份實作而非共用一份函式，是因為 `ab-core` 不依賴 `ab-tui`；共同的
/// oracle 是 `tests/p5-fixture.sh` 的 u1／u3 兩筆。
pub fn scan(
    paths: &Paths,
    tmux: &dyn crate::tmux::TmuxClient,
    pager: &dyn Pager,
) -> Vec<EmitOutcome> {
    if !tmux.available() {
        return Vec::new();
    }
    let Some(list) = tmux
        .exec(&["list-panes", "-a", "-F", "#{pane_id}"])
        .and_then(|o| o.ok_stdout())
    else {
        // 拿不到清單＝沒有證據，整輪不掃（見上面的說明）
        return Vec::new();
    };
    let alive: std::collections::HashSet<&str> = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let agents = crate::registry::snapshot(paths);
    let tasks = crate::task::in_flight(paths);
    let mut out = Vec::new();
    for a in &agents {
        // pane 欄空的（人工 register 沒給 pane）不是「死了」，是沒有這個軸
        if a.pane.is_empty() || alive.contains(a.pane.as_str()) {
            continue;
        }
        // 地點只對**已判定死掉**的 agent 解一次：健康的池子一次 tmux 都不多問，
        // 同一個 agent 掛著三筆 task 也只解一次
        let details = PageDetails {
            location: resolve_location(tmux, &a.pane, &a.owner),
            reason: None,
        };
        for t in &tasks {
            if !task_belongs_to(t, a) {
                continue;
            }
            let event = PageEvent::WorkerDied {
                agent: a.name.clone(),
                spawn_tag: a.spawn_tag.clone(),
                task: t.id.clone(),
            };
            if let Ok(o) = emit(paths, &event, &details, pager) {
                out.push(o);
            }
        }
    }
    out
}

/// 這一筆 task 掛在這一代的這個 agent 身上嗎：收件人名相符**且** task 的
/// `created_at` 嚴格晚於該 agent 的 `registered_at`。
///
/// 只比名字的話，同名 respawn 會把上一代的歷史 task 認領過來。同秒（相等）
/// ＝不可證＝不掛。時間戳解析不出來也不掛（fail-closed）。
///
/// **單一事實源**（跨廠複核 should-fix 3）：page 層與 TUI 的歸屬軸是同一條
/// 規則，先前各寫一份。依賴方向本來就是 `ab-tui → ab-core`，所以正本放這裡，
/// `ab_tui::model::attached` 轉呼叫。共同的 oracle 是 `tests/p5-fixture.sh`
/// 的 u1／u3 兩筆。
pub fn task_belongs_to(
    task: &crate::task::InFlight,
    agent: &crate::registry::AgentSnapshot,
) -> bool {
    if task.to != agent.name {
        return false;
    }
    match (
        crate::time::parse_iso_to_epoch(&task.created_at),
        crate::time::parse_iso_to_epoch(&agent.registered_at),
    ) {
        (Some(t), Some(r)) => t > r,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 沒有地點也沒有原因——去重與落盤那幾條與呈現無關，用空的最不容易誤讀。
    /// 文案本身另有專門的條目測。
    const D: PageDetails = PageDetails {
        location: None,
        reason: None,
    };

    /// 記錄被推播了幾次、內容是什麼。
    struct RecordingPager {
        calls: RefCell<Vec<(String, String)>>,
        ok: bool,
    }

    impl RecordingPager {
        fn new(ok: bool) -> Self {
            RecordingPager {
                calls: RefCell::new(Vec::new()),
                ok,
            }
        }
        fn count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl Pager for RecordingPager {
        fn page(&self, title: &str, body: &str) -> bool {
            self.calls
                .borrow_mut()
                .push((title.to_string(), body.to_string()));
            self.ok
        }
    }

    /// 只數次數，但跨執行緒可用（`RecordingPager` 的 `RefCell` 不是 `Sync`）。
    #[derive(Default)]
    struct CountingPager {
        n: std::sync::atomic::AtomicUsize,
    }

    impl CountingPager {
        fn count(&self) -> usize {
            self.n.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Pager for CountingPager {
        fn page(&self, _title: &str, _body: &str) -> bool {
            self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        }
    }

    /// 記錄跑過哪些 argv 的假 runner。
    struct FakeRunner {
        argvs: RefCell<Vec<Vec<String>>>,
        ok: bool,
    }

    impl FakeRunner {
        fn new(ok: bool) -> Self {
            FakeRunner {
                argvs: RefCell::new(Vec::new()),
                ok,
            }
        }
        fn progs(&self) -> Vec<String> {
            self.argvs.borrow().iter().map(|a| a[0].clone()).collect()
        }
    }

    impl CmdRunner for FakeRunner {
        fn run(&self, argv: &[String]) -> bool {
            self.argvs.borrow_mut().push(argv.to_vec());
            self.ok
        }
    }

    /// 一個有兩個 attached client 的 tmux。`exec` 記下每一次呼叫。
    struct FakeTmux {
        clients: Option<&'static str>,
        /// `list-panes -a` 的回答。`None`＝查詢失敗（沒有證據）。
        panes: Option<&'static str>,
        calls: RefCell<Vec<Vec<String>>>,
        available: bool,
    }

    impl FakeTmux {
        fn with_clients(clients: &'static str) -> Self {
            FakeTmux {
                clients: Some(clients),
                panes: Some(""),
                calls: RefCell::new(Vec::new()),
                available: true,
            }
        }
        fn unavailable() -> Self {
            FakeTmux {
                clients: None,
                panes: None,
                calls: RefCell::new(Vec::new()),
                available: false,
            }
        }
        /// 活著的 pane 清單（`list-clients` 一律有一個 client）。
        fn with_panes(panes: &'static str) -> Self {
            FakeTmux {
                clients: Some("/dev/pts/3\n"),
                panes: Some(panes),
                calls: RefCell::new(Vec::new()),
                available: true,
            }
        }
        /// `list-panes` 查不出來（tmux 掛了／逾時）。
        fn panes_unknown() -> Self {
            FakeTmux {
                clients: Some("/dev/pts/3\n"),
                panes: None,
                calls: RefCell::new(Vec::new()),
                available: true,
            }
        }
        fn display_calls(&self) -> Vec<Vec<String>> {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c[0] == "display-message")
                .cloned()
                .collect()
        }
    }

    impl crate::tmux::TmuxClient for FakeTmux {
        fn exec(&self, args: &[&str]) -> Option<crate::tmux::TmuxOutput> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            match args.first().copied() {
                Some("list-clients") => Some(crate::tmux::TmuxOutput {
                    status_ok: true,
                    stdout: self.clients.unwrap_or("").to_string(),
                    stderr: String::new(),
                }),
                Some("list-panes") => self.panes.map(|p| crate::tmux::TmuxOutput {
                    status_ok: true,
                    stdout: p.to_string(),
                    stderr: String::new(),
                }),
                _ => Some(crate::tmux::TmuxOutput {
                    status_ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }
        fn available(&self) -> bool {
            self.available
        }
        fn resolve_pane(&self, _t: &str) -> Option<String> {
            None
        }
        fn pane_exists(&self, _p: &str) -> bool {
            true
        }
        fn capture_pane(&self, _p: &str) -> Option<String> {
            None
        }
        fn pane_in_mode(&self, _p: &str) -> Option<bool> {
            Some(false)
        }
        fn send_keys(&self, _p: &str, _k: &str) -> bool {
            true
        }
    }

    fn tmp_paths(tag: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "ab-core-page-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Paths {
            agents_dir: dir.join("agents"),
            tasks_dir: dir.join("tasks"),
            locks_dir: dir.join("locks"),
            state_dir: dir.join("state"),
            data_dir: dir,
        };
        p.ensure_dirs().unwrap();
        p
    }

    fn failed(task: &str) -> PageEvent {
        PageEvent::TaskFailed {
            agent: "w1".into(),
            task: task.into(),
        }
    }

    fn event_lines(paths: &Paths) -> Vec<String> {
        std::fs::read_to_string(events_path(paths))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// 同一個 key 再發：事件流**不再多一筆**，也**不再推一次**。
    /// 這是 rubric v2 page 層條 1（恰推一次）與條 2（重複事件不重推）。
    #[test]
    fn the_same_event_is_only_paged_once() {
        let paths = tmp_paths("dedup");
        let pager = RecordingPager::new(true);
        assert_eq!(
            emit(&paths, &failed("t1"), &D, &pager).unwrap(),
            EmitOutcome::Emitted { paged: true }
        );
        assert_eq!(
            emit(&paths, &failed("t1"), &D, &pager).unwrap(),
            EmitOutcome::AlreadySeen
        );
        assert_eq!(pager.count(), 1, "重複事件 MUST NOT 重推");
        assert_eq!(event_lines(&paths).len(), 1, "重複事件 MUST NOT 重複落盤");
    }

    /// 行程重啟不重推：seen 落在磁碟上，換一個 `Pager` 實例（＝換一個行程）
    /// 照樣認得先前處理過的 key。
    #[test]
    fn a_restarted_process_does_not_page_the_same_event_again() {
        let paths = tmp_paths("restart");
        let first = RecordingPager::new(true);
        emit(&paths, &failed("t1"), &D, &first).unwrap();
        let second = RecordingPager::new(true);
        assert_eq!(
            emit(&paths, &failed("t1"), &D, &second).unwrap(),
            EmitOutcome::AlreadySeen
        );
        assert_eq!(second.count(), 0, "行程重啟 MUST NOT 重推");
    }

    /// **notifier 故障時仍有 durable event 落盤**（rubric v2 page 層條 4）。
    /// 推播回 false 不是錯誤：`emit` 照樣回 Ok，事件照樣在檔案裡。
    #[test]
    fn a_broken_notifier_still_leaves_the_event_on_disk() {
        let paths = tmp_paths("broken");
        let pager = NullPager;
        assert_eq!(
            emit(&paths, &failed("t1"), &D, &pager).unwrap(),
            EmitOutcome::Emitted { paged: false }
        );
        let lines = event_lines(&paths);
        assert_eq!(lines.len(), 1, "推播失敗 MUST NOT 影響落盤");
        assert!(
            lines[0].contains("\"task\":\"t1\""),
            "落盤內容：{}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"kind\":\"task-failed\""),
            "落盤內容：{}",
            lines[0]
        );
    }

    /// 不同事件各推各的——去重不得寬到把兩件事併成一件。
    #[test]
    fn distinct_events_each_get_their_own_page() {
        let paths = tmp_paths("distinct");
        let pager = RecordingPager::new(true);
        emit(&paths, &failed("t1"), &D, &pager).unwrap();
        emit(&paths, &failed("t2"), &D, &pager).unwrap();
        emit(
            &paths,
            &PageEvent::WorkerDied {
                agent: "w1".into(),
                spawn_tag: "ab-spawn-w1-1-aaaaaaaaaaaa".into(),
                task: "t1".into(),
            },
            &D,
            &pager,
        )
        .unwrap();
        assert_eq!(pager.count(), 3);
        assert_eq!(event_lines(&paths).len(), 3);
    }

    /// **同名 respawn 是另一個事實**：generation key 進 event key，所以同一個
    /// 名字、同一筆 task、不同世代會各推一次（rubric v2 page 層條 2 的反面
    /// ——不該被去重吃掉的那一半）。
    #[test]
    fn a_respawned_agent_is_a_different_generation_and_pages_again() {
        let paths = tmp_paths("respawn");
        let pager = RecordingPager::new(true);
        let died = |tag: &str| PageEvent::WorkerDied {
            agent: "w1".into(),
            spawn_tag: tag.into(),
            task: "t1".into(),
        };
        emit(&paths, &died("ab-spawn-w1-1-aaaaaaaaaaaa"), &D, &pager).unwrap();
        emit(&paths, &died("ab-spawn-w1-2-bbbbbbbbbbbb"), &D, &pager).unwrap();
        assert_eq!(pager.count(), 2, "換代 MUST 是新事件");
    }

    fn pager_with<'a>(
        env: PagerEnv,
        runner: &'a FakeRunner,
        tmux: &'a FakeTmux,
    ) -> SystemPager<'a> {
        SystemPager {
            env,
            runner,
            tmux: tmux as &dyn crate::tmux::TmuxClient,
        }
    }

    /// **SSH 下不得呼叫 notify-send**（使用者 2026-08-03 提出）：遠端 session 的
    /// `DISPLAY` 指的是遠端那台的桌面，通知會彈在沒有人的螢幕上——比不通知
    /// 更糟，因為人會以為自己已經被通知過。自動降級到 tmux 那一層。
    #[test]
    fn a_remote_session_never_fires_a_desktop_notification() {
        let runner = FakeRunner::new(true);
        let tmux = FakeTmux::with_clients("/dev/pts/3\n");
        let env = PagerEnv {
            notify_cmd: None,
            has_display: true,
            over_ssh: true,
        };
        assert!(pager_with(env, &runner, &tmux).page("T", "B"));
        assert!(
            runner.progs().is_empty(),
            "SSH 下 MUST NOT 跑桌面通知：{:?}",
            runner.progs()
        );
        assert_eq!(
            tmux.display_calls().len(),
            1,
            "MUST 降級到 tmux status line"
        );
    }

    /// 本機桌面 session 才走 notify-send。
    #[test]
    fn a_local_desktop_session_uses_notify_send() {
        let runner = FakeRunner::new(true);
        let tmux = FakeTmux::with_clients("/dev/pts/3\n");
        let env = PagerEnv {
            notify_cmd: None,
            has_display: true,
            over_ssh: false,
        };
        assert!(pager_with(env, &runner, &tmux).page("T", "B"));
        assert_eq!(runner.progs(), vec!["notify-send".to_string()]);
    }

    /// 自訂命令取代桌面那一層，並收到 `<title> <body>` 兩個參數——這是 SSH／
    /// 無桌面環境的逃生口，argv 形狀是它的契約。
    #[test]
    fn a_custom_notify_command_replaces_the_desktop_layer_and_gets_title_and_body() {
        let runner = FakeRunner::new(true);
        let tmux = FakeTmux::with_clients("/dev/pts/3\n");
        let env = PagerEnv {
            notify_cmd: Some("/usr/local/bin/my-pager".into()),
            // 桌面就在旁邊也一樣：使用者設了自訂命令就是他說了算
            has_display: true,
            over_ssh: false,
        };
        assert!(pager_with(env, &runner, &tmux).page("標題", "內文"));
        assert_eq!(
            runner.argvs.borrow()[0],
            vec![
                "/usr/local/bin/my-pager".to_string(),
                "標題".to_string(),
                "內文".to_string()
            ]
        );
    }

    /// tmux 這一層**逐個 client 送**：`display-message` 只送 target-client，
    /// 不逐個指定就只有一個 client 看得到（tmux 3.7b man）。
    #[test]
    fn every_attached_client_gets_the_status_line_message() {
        let runner = FakeRunner::new(true);
        let tmux = FakeTmux::with_clients("/dev/pts/3\n/dev/pts/9\n");
        let env = PagerEnv::default();
        assert!(pager_with(env, &runner, &tmux).page("T", "B"));
        let calls = tmux.display_calls();
        assert_eq!(calls.len(), 2, "兩個 client MUST 各送一次：{calls:?}");
        assert_eq!(calls[0][2], "/dev/pts/3");
        assert_eq!(calls[1][2], "/dev/pts/9");
        // `-l`：事件文字含 agent 名與 task id，是不可信輸入，不得被當 tmux
        // format 展開
        assert!(
            calls[0].contains(&"-l".to_string()),
            "MUST 用 -l：{calls:?}"
        );
    }

    /// 三層全滅只是回 `false`，**不是錯誤**——呼叫端行為零改變（§1 非目標）。
    #[test]
    fn a_pager_with_nowhere_to_send_just_returns_false() {
        let runner = FakeRunner::new(false);
        let tmux = FakeTmux::unavailable();
        let env = PagerEnv::default();
        assert!(!pager_with(env, &runner, &tmux).page("T", "B"));
    }

    /// 推播全滅時 `emit` 照樣回 Ok、照樣落盤（rubric v2 page 層條 4，走真正的
    /// ladder 而不是 `NullPager`）。
    #[test]
    fn the_event_survives_even_when_the_whole_ladder_fails() {
        let paths = tmp_paths("ladder-fail");
        let runner = FakeRunner::new(false);
        let tmux = FakeTmux::unavailable();
        let pager = pager_with(PagerEnv::default(), &runner, &tmux);
        assert_eq!(
            emit(&paths, &failed("t1"), &D, &pager).unwrap(),
            EmitOutcome::Emitted { paged: false }
        );
        assert_eq!(event_lines(&paths).len(), 1);
    }

    // ---- scan（page 層事件 2／2）----

    const REG_AT: &str = "2026-08-01T00:00:00Z";

    fn write_agent(paths: &Paths, name: &str, pane: &str) {
        let json = format!(
            r#"{{"name":"{name}","pane_id":"{pane}","registered_at":"{REG_AT}","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"AGENT_BRIDGE_SPAWN_TAG=ab-spawn-{name}-1-aaaaaaaaaaaa","owner":"s:@1"}}"#
        );
        std::fs::write(paths.agents_dir.join(format!("{name}.json")), json).unwrap();
    }

    /// task 目錄名必須是本工具生成的形狀（`in_flight` 只認那一種）。
    fn write_task(paths: &Paths, id: &str, to: &str, status: &str, created: &str) {
        let dir = paths.tasks_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("status"), format!("{status}\n")).unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!(
                r#"{{"version":1,"task_id":"{id}","from":"boss","to":"{to}","created_at":"{created}","updated_at":"{created}","working_directory":"/tmp","status":"{status}"}}"#
            ),
        )
        .unwrap();
    }

    /// 掛著非終態 task 的 pane 死了 → **恰推一次**（rubric v2 page 層條 1）。
    #[test]
    fn a_dead_pane_holding_live_work_pages_exactly_once() {
        let paths = tmp_paths("scan-hit");
        write_agent(&paths, "w1", "%99");
        write_task(
            &paths,
            "20260802T000000Z-aa01",
            "w1",
            "running",
            "2026-08-02T00:00:00Z",
        );
        let pager = RecordingPager::new(true);
        let tmux = FakeTmux::with_panes("%1\n%2\n");
        assert_eq!(scan(&paths, &tmux, &pager).len(), 1);
        assert_eq!(pager.count(), 1);
        // 再掃一次不重推（機會式掃描掛在每個子指令上，這是最常走到的路）
        scan(&paths, &tmux, &pager);
        assert_eq!(pager.count(), 1, "重複掃描 MUST NOT 重推");
    }

    /// 零推播的四種情形（rubric v2 page 層條 3）。
    #[test]
    fn nothing_worth_waking_someone_for_pages_nobody() {
        // (a) pane 活著
        let paths = tmp_paths("scan-alive");
        write_agent(&paths, "w1", "%1");
        write_task(
            &paths,
            "20260802T000000Z-aa01",
            "w1",
            "running",
            "2026-08-02T00:00:00Z",
        );
        let pager = RecordingPager::new(true);
        scan(&paths, &FakeTmux::with_panes("%1\n"), &pager);
        assert_eq!(pager.count(), 0, "pane 活著 MUST NOT 推");

        // (b) pane 死了但沒有掛任何 task
        let paths = tmp_paths("scan-notask");
        write_agent(&paths, "w1", "%99");
        let pager = RecordingPager::new(true);
        scan(&paths, &FakeTmux::with_panes("%1\n"), &pager);
        assert_eq!(pager.count(), 0, "沒有 task 的 pane-exit MUST NOT 推");

        // (c) pane 死了、task 已進終態
        let paths = tmp_paths("scan-done");
        write_agent(&paths, "w1", "%99");
        write_task(
            &paths,
            "20260802T000000Z-aa01",
            "w1",
            "completed",
            "2026-08-02T00:00:00Z",
        );
        let pager = RecordingPager::new(true);
        scan(&paths, &FakeTmux::with_panes("%1\n"), &pager);
        assert_eq!(pager.count(), 0, "終態 task MUST NOT 推");

        // (d) stale generation：task 早於這一代的 registered_at＝上一代的歷史
        // 任務，同名 respawn 不承接（與 TUI `attached()` 同一條規則）
        let paths = tmp_paths("scan-stale");
        write_agent(&paths, "w1", "%99");
        write_task(
            &paths,
            "20260731T000000Z-aa01",
            "w1",
            "running",
            "2026-07-31T00:00:00Z",
        );
        let pager = RecordingPager::new(true);
        scan(&paths, &FakeTmux::with_panes("%1\n"), &pager);
        assert_eq!(pager.count(), 0, "上一代的歷史 task MUST NOT 推");
    }

    /// **tmux 查不出 pane 清單時整輪不掃**：照抄送鍵路徑的 fail-closed（查詢
    /// 失敗＝當作死）會在 tmux 一掛掉時把整池 worker 一次推爆。呼叫器的方向
    /// 相反：沒有證據就不叫人。
    #[test]
    fn a_tmux_outage_never_turns_the_whole_pool_into_alarms() {
        let paths = tmp_paths("scan-outage");
        // 三筆各自不同的 task id：撞成同一個目錄的話，讀者會誤以為這條在測
        // 別的東西（跨廠複核 should-fix 5 的可讀性一項）
        for (i, n) in ["w1", "w2", "w3"].iter().enumerate() {
            write_agent(&paths, n, "%99");
            write_task(
                &paths,
                &format!("20260802T00000{i}Z-aa0{i}"),
                n,
                "running",
                "2026-08-02T00:00:00Z",
            );
        }
        let pager = RecordingPager::new(true);
        assert!(scan(&paths, &FakeTmux::panes_unknown(), &pager).is_empty());
        assert_eq!(pager.count(), 0, "拿不到 pane 清單 MUST NOT 推任何一則");
        let pager2 = RecordingPager::new(true);
        assert!(scan(&paths, &FakeTmux::unavailable(), &pager2).is_empty());
        assert_eq!(pager2.count(), 0, "tmux 不在 MUST NOT 推");
    }

    /// **同一則事件被兩個行程同時發現時只推一次**（跨廠複核 should-fix 5）。
    ///
    /// 先前只有序列呼叫的測試：把鎖內那次 `is_seen` 覆核刪掉、只留鎖外的
    /// fast path，測試照樣綠。機會式掃描掛在每個非唯讀子指令上，兩個 CLI
    /// 同時撞上同一件事是常態，不是稀有情形。
    ///
    /// 用 barrier 讓 N 條執行緒盡量同時進入 `emit`。**它只會漏抓、不會誤紅**：
    /// 沒撞上時斷言照樣成立，撞上了才有機會抓到少一道覆核的實作。
    #[test]
    fn two_processes_finding_the_same_event_at_once_still_page_once() {
        let paths = tmp_paths("race");
        let pager = std::sync::Arc::new(CountingPager::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let paths = paths.clone();
                let pager = std::sync::Arc::clone(&pager);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    emit(&paths, &failed("t1"), &D, pager.as_ref())
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        assert_eq!(pager.count(), 1, "同時發現 MUST 只推一次");
        assert_eq!(event_lines(&paths).len(), 1, "同時發現 MUST 只落盤一筆");
    }

    /// worker 寫得到 registry，所以 `spawn_tag` 是不可信輸入。含換行的 tag 會
    /// 把一個 seen key 寫成兩行，之後逐行比對永遠比不中——去重整個失效，每輪
    /// 掃描重推同一則（跨廠複核 should-fix 1）。fail-closed：不落盤、不推。
    #[test]
    fn a_key_that_the_seen_file_cannot_frame_is_refused_outright() {
        let paths = tmp_paths("framing");
        let pager = RecordingPager::new(true);
        let evil = PageEvent::WorkerDied {
            agent: "w1".into(),
            spawn_tag: "ab-spawn-w1-1-aaaa\ndied:w1:x:t1".into(),
            task: "t1".into(),
        };
        assert_eq!(
            emit(&paths, &evil, &D, &pager).unwrap(),
            EmitOutcome::Rejected
        );
        assert_eq!(pager.count(), 0, "不可信的 key MUST NOT 推");
        assert!(
            !events_path(&paths).exists() || event_lines(&paths).is_empty(),
            "不可信的 key MUST NOT 落盤"
        );
        // 重掃也一樣，不會因為「沒記進 seen」而變成無限的通知源
        assert_eq!(
            emit(&paths, &evil, &D, &pager).unwrap(),
            EmitOutcome::Rejected
        );
        assert_eq!(pager.count(), 0);
    }

    /// 自訂命令**取代**桌面那一層，不是加在它前面：runner 的呼叫總數恰 1。
    /// 先前只斷言第 0 次 argv，實作若同時又跑一次 notify-send 照樣會綠
    /// （跨廠複核 should-fix 5）。
    #[test]
    fn a_custom_notify_command_is_the_only_desktop_call() {
        let runner = FakeRunner::new(true);
        let tmux = FakeTmux::with_clients("/dev/pts/3\n");
        let env = PagerEnv {
            notify_cmd: Some("/usr/local/bin/my-pager".into()),
            has_display: true,
            over_ssh: false,
        };
        pager_with(env, &runner, &tmux).page("T", "B");
        assert_eq!(
            runner.argvs.borrow().len(),
            1,
            "自訂命令 MUST 取代桌面層而不是疊加：{:?}",
            runner.progs()
        );
    }

    /// **自訂 notifier 不得繼承 stdin**（跨廠複核 blocker 1）：機會式掃描跑在
    /// CLI 路徑上，`send --message-file -` 的 payload 會被它讀走。真的跑一支
    /// 會吸乾 stdin 的腳本，然後確認我方 stdin 一個 byte 都沒少。
    ///
    /// 另一半（不退出的命令要有界）由 `NOTIFY_TIMEOUT` 守著；這裡順帶量它
    /// 真的會回來，不是永遠等下去。
    #[test]
    fn a_custom_notifier_neither_eats_our_stdin_nor_hangs_forever() {
        let dir = std::env::temp_dir().join(format!("ab-core-page-stdin-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("greedy.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\ncat > /dev/null\nsleep 0.2\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let started = std::time::Instant::now();
        let ok = SubprocessRunner.run(&[
            script.to_string_lossy().into_owned(),
            "T".into(),
            "B".into(),
        ]);
        // `cat` 讀到的是 /dev/null（stdin 已被接掉），所以它會立刻 EOF 而不是
        // 卡住等我方的 tty／pipe——這正是「沒有繼承 stdin」的可觀察證據
        assert!(ok, "腳本應正常退出");
        assert!(
            started.elapsed() < NOTIFY_TIMEOUT,
            "MUST NOT 卡到逾時（實測 {:?}）",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 通知文字本身要答得出「哪個 agent、為何現在需要我」——不必打開任何面板
    /// （rubric v2 page 層 human judgment 的機器可判定那一半）。
    #[test]
    fn the_page_text_names_the_agent_and_the_task() {
        let e = failed("20260803T000000Z-abcd");
        assert!(
            e.title(&D).contains("w1"),
            "標題要有 agent 名：{}",
            e.title(&D)
        );
        assert!(
            e.body(&D).contains("20260803T000000Z-abcd"),
            "內文要有 task id：{}",
            e.body(&D)
        );
        let d = PageEvent::WorkerDied {
            agent: "w9".into(),
            spawn_tag: "ab-spawn-w9-1-cccccccccccc".into(),
            task: "t7".into(),
        };
        assert!(
            d.title(&D).contains("w9"),
            "標題要有 agent 名：{}",
            d.title(&D)
        );
        assert!(
            d.body(&D).contains("t7"),
            "內文要有 task id：{}",
            d.body(&D)
        );
    }

    /// **標題要說得出「切去哪裡」**（PG4 實測 2026-08-03）：受測者答得出誰、
    /// 出了什麼事，卻卡在「要不要切過去看」——因為通知從沒說過切去哪。
    #[test]
    fn the_title_says_where_to_go() {
        let d = PageDetails {
            location: Some("dev:3 build".into()),
            reason: None,
        };
        let t = failed("t1").title(&d);
        assert!(t.contains("w1"), "標題要有 agent 名：{t}");
        assert!(t.contains("dev:3 build"), "標題要有地點：{t}");
        // 解不出地點時整段省略，不留 ` · ` 這種空殼
        let bare = failed("t1").title(&D);
        assert!(!bare.contains('·'), "沒有地點時 MUST NOT 留分隔符：{bare}");
    }

    /// **內文第一行是失敗原因**（同上）：決定「要不要現在出手」的是原因本身，
    /// 「進了 failed 終態」只是把標題再說一次。task id 退到最後一行但**必須
    /// 還在**——它是 `ab read <id>` 的把手。
    #[test]
    fn the_body_leads_with_the_reason_and_keeps_the_task_id() {
        let d = PageDetails::default().with_reason("編譯失敗：缺 libfoo\n第二行不該進通知");
        let b = failed("20260803T000000Z-abcd").body(&d);
        let mut lines = b.lines();
        assert_eq!(
            lines.next(),
            Some("編譯失敗：缺 libfoo"),
            "首行要是原因：{b}"
        );
        assert_eq!(
            lines.next(),
            Some("20260803T000000Z-abcd"),
            "末行要是 task id：{b}"
        );
        assert!(
            !b.contains("第二行不該進通知"),
            "只取第一行，多行訊息 MUST NOT 整份灌進通知：{b}"
        );
        // 原因取不到（空訊息）才退回狀態字，且 task id 照樣在
        let empty = PageDetails::default().with_reason("\n  \n");
        let fallback = failed("t1").body(&empty);
        assert!(fallback.contains("failed"), "退路要說得出狀態：{fallback}");
        assert!(
            fallback.contains("t1"),
            "退路 MUST 保留 task id：{fallback}"
        );
    }

    /// 不可信輸入不得撐破單行通知：worker 寫得到 response.md，window 名也是
    /// 任人改的。控制字元一律壓成空白，過長按**字元**截斷（按 byte 切會把
    /// 多位元組字元剖半）。
    #[test]
    fn untrusted_text_cannot_break_a_one_line_notification() {
        let d = PageDetails::default().with_reason("壞掉了\r\n偽造的第二則通知");
        let r = d.reason.as_deref().unwrap_or("");
        assert!(!r.contains('\n'), "原因 MUST NOT 帶換行：{r:?}");
        assert_eq!(r, "壞掉了", "只取第一行");

        let long = "字".repeat(MAX_REASON + 20);
        let cut = PageDetails::default().with_reason(&long);
        let c = cut.reason.as_deref().unwrap_or("");
        assert_eq!(
            c.chars().count(),
            MAX_REASON + 1,
            "MUST 截到上限再補省略號：{c}"
        );
        assert!(c.ends_with('…'));
        // 截斷點若按 byte 算，這裡會 panic 在 char boundary 上
        assert!(c.starts_with('字'));
    }

    /// pane 死了就退到 owner 的 window——`WorkerDied` 的常態正是「pane 沒了」，
    /// 只問 pane 的話最需要地點的那一類事件永遠拿不到地點。
    #[test]
    fn a_dead_pane_falls_back_to_the_owner_window() {
        /// 只認得出 `@7` 這個 target。問死掉的 pane 時**照抄真實 tmux 的行為**
        /// ——exit 0 ＋ 每個欄位空展開（結果是 `":"`），不是失敗。第一版的假件
        /// 回 `None`，於是實作的 fail-open 在單元層一路綠到分組 47 才被逮到。
        struct LocTmux {
            asked: RefCell<Vec<String>>,
        }
        impl crate::tmux::TmuxClient for LocTmux {
            fn exec(&self, args: &[&str]) -> Option<crate::tmux::TmuxOutput> {
                let target = args.get(3).copied().unwrap_or("");
                self.asked.borrow_mut().push(target.to_string());
                Some(crate::tmux::TmuxOutput {
                    status_ok: true,
                    stdout: if target == "@7" {
                        "dev:3 build\n"
                    } else {
                        ":\n"
                    }
                    .to_string(),
                    stderr: String::new(),
                })
            }
            fn available(&self) -> bool {
                true
            }
            fn resolve_pane(&self, _t: &str) -> Option<String> {
                None
            }
            fn pane_exists(&self, _p: &str) -> bool {
                false
            }
            fn capture_pane(&self, _p: &str) -> Option<String> {
                None
            }
            fn pane_in_mode(&self, _p: &str) -> Option<bool> {
                Some(false)
            }
            fn send_keys(&self, _p: &str, _k: &str) -> bool {
                true
            }
        }

        let t = LocTmux {
            asked: RefCell::new(Vec::new()),
        };
        assert_eq!(
            resolve_location(&t, "%99", "zzb:@7").as_deref(),
            Some("dev:3 build"),
            "pane 問不到 MUST 退到 owner 的 window"
        );
        assert_eq!(
            t.asked.borrow().as_slice(),
            &["%99".to_string(), "@7".to_string()],
            "順序 MUST 是先 pane 後 window"
        );
        // 兩條都問不到就是沒有地點——通知照發，只是少一段
        let t2 = LocTmux {
            asked: RefCell::new(Vec::new()),
        };
        assert_eq!(resolve_location(&t2, "%99", "沒有冒號at的字串"), None);
    }

    /// 地點與原因**不得進去重軸**：window 改個名、失敗訊息換句話說，都不是
    /// 新事件。它們分屬兩個型別就是為了防這件事，這條把它釘死。
    #[test]
    fn presentation_details_never_change_the_dedup_key() {
        let e = failed("t1");
        let plain = e.key();
        let dressed = PageDetails {
            location: Some("dev:3 build".into()),
            reason: Some("完全不同的原因".into()),
        };
        assert_eq!(e.key(), plain, "key MUST NOT 隨呈現欄位改變");
        // 同一個事件配不同的 details，推播仍只有一次
        let paths = tmp_paths("details-dedup");
        let pager = RecordingPager::new(true);
        emit(&paths, &e, &D, &pager).unwrap();
        assert_eq!(
            emit(&paths, &e, &dressed, &pager).unwrap(),
            EmitOutcome::AlreadySeen,
            "換一組 details MUST NOT 變成新事件"
        );
        assert_eq!(pager.count(), 1);
    }
}

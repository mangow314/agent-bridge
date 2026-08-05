//! 送鍵通知：權限框雙掃、通知失敗語意、`notify_or_defer` 的 state TTL gate
//! （hooks.md HOOK-NOTIFY-*、env.md ENV-TTL-1/2、ENV-NOTIFY-1）。對映 bash
//! `notify_pane`:330、`screen_has_prompt`:318、`notify_or_defer`:371。
//!
//! 警告訊息直接 `eprintln!` 到 stderr（同 `lock::LockGuard::release` 的既有
//! 先例）：它們是流程中的提示而非指令失敗，回不到 `ab` 的 `Err` 收斂層——
//! 那條路徑會把整個指令變成非零退出，而 bash 這裡是 `err` + 繼續。

use crate::config;
use crate::error::Result;
use crate::json;
use crate::paths::Paths;
use crate::task::log_event;
use crate::time::{now_epoch, parse_iso_to_epoch};
use crate::tmux::TmuxClient;
use serde_json::Value;

/// PANE_RE `^%[0-9]+$`（bin/agent-bridge:30）。registry 的 pane_id 對本程式是
/// 不可信輸入且會餵給 tmux，`%1 ; kill-server` 這種值必須擋在送鍵之前。
pub fn is_valid_pane(pane: &str) -> bool {
    let mut bytes = pane.bytes();
    bytes.next() == Some(b'%') && {
        let rest: Vec<u8> = bytes.collect();
        !rest.is_empty() && rest.iter().all(|b| b.is_ascii_digit())
    }
}

/// 比對前先把窗內空白（含換行）摺疊成單一空格，對映 `tr -s '[:space:]' ' '`：
/// TUI 的 word-wrap 與 tmux 軟折行會把特徵片段拆到兩行，逐行比對必偽陰性
/// （漏判＝放行誤批的 Enter，是最壞方向）。Rust 的 `char::is_whitespace` 涵蓋
/// Unicode 空白、比 C locale 的 `[:space:]` 寬，差異只會讓更多片段被拼接
/// 起來——偽陽性方向，落在 bash 註解既定的 fail-closed 偏攔代價內。
fn fold_whitespace(s: &str) -> String {
    let mut norm = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                norm.push(' ');
            }
            prev_ws = true;
        } else {
            norm.push(c);
            prev_ws = false;
        }
    }
    norm
}

/// 只掃畫面**下緣**這麼多行：權限框永遠貼著 pane 底部，助理輸出／指令回顯不會。
///
/// 取值 14 是量出來的，不是拍的（2026-08-01 實測，記錄見 docs/tui-design.md §4）：
/// - 真權限框（Claude Code v2.1.220，33×143 pane）：框出現時**取代**輸入框與
///   statusline，整框貼底——`Esc to cancel` 距底 0 行、`Do you want to proceed?`
///   距底 5 行、框頂分隔線距底 13 行。agy 的框（docs/agy-probe.md 實測十行）
///   header 距底 9 行。故真陽性所需的最深片段是 9。
/// - 誤判語料（19 個 coordinator 誤判幀，poll-133-*）：命中片段最淺的一個距底
///   **16** 行——那是 `rg` 指令回顯與散文引用，都在畫面上半部。
///
/// 14 行的窗涵蓋距底 0–13（含）。兩側餘裕的**精確語意**：
/// - 真框側：所需最深片段在 9，還能再深 4 行仍被涵蓋；**第 5 行開始漏判**。
/// - 誤判側：語料最淺的一幀在 16，要再往下移 3 行（到 13）才會重新被掃到。
///
/// 窗**不得**越過下緣區往上吃（早期版本讓鄰近窗跨出區界，19 個誤判幀有 12 個
/// 照樣命中）。
const TAIL_LINES: usize = 14;

/// 同一特徵組的片段必須落在這麼多行內，而不是「同屏任意處」。
///
/// 取值 12 的下界來自真框跨距：agy 框 header→footer 距 9 行、claude 框
/// `Do you want to proceed?`→`Esc to cancel` 距 5 行；12 對前者留 2 行餘裕。
/// 上界是 `TAIL_LINES`——比它大就退化成「整個下緣區同屏比對」，鄰近條件失效。
const PROXIMITY_LINES: usize = 12;

/// screen_has_prompt:318 — 一屏可見文字裡是否有會被 Enter 誤批的確認對話框。
/// 前兩組特徵（claude 權限框／plan mode 退出框）依 bash 逐字。
///
/// 第三組是 agy（Antigravity CLI）的權限框，**Rust 獨有**：bash 正本自 M4
/// 凍結、也不支援 agy runtime，這裡的分歧是設計而非漂移。agy 的框長成
/// 「Requesting permission for: … Do you want to proceed? … esc to cancel」
/// ——footer 是**小寫** `esc`，前兩組特徵一個都不命中（量測見
/// docs/agy-probe.md，缺口 AGY-PROMPT-1）。若不補，送鍵的 Enter 會落在預設
/// 選項 `1. Yes`，替一個正等人類決策的 worker 按下批准。
///
/// 錨用 agy 獨有的 `Requesting permission for:` 而非放寬 `esc` 的大小寫：
/// agy 執行中 footer 常駐小寫 `esc to cancel`，只放寬大小寫會讓「助理輸出
/// 自己寫出 Do you want to …」湊成誤判。
///
/// **header 單錨不夠**（跨廠複核 2026-07-31 的 blocker）：掃描只看可見一屏
/// （`capture-pane -pJ`，不取 scrollback），而預設 worker 會進共用 window 並
/// `tiled` 均分——pane 一多就矮，框的 header 會被捲出畫面、只剩下緣的
/// 選項與 footer。那時 header 錨失效、claude 那組又因大寫 `Esc` 不命中，
/// 送鍵的 Enter 就落在 `1. Yes`。故第四組是下緣備援：完整句
/// `Do you want to proceed?` ＋小寫 `esc to cancel` 成對。
///
/// **位置有錨＋單錨降級**（2026-08-01 收窄，量化依據見 `TAIL_LINES`）：原本
/// 是整屏無錨 substring，於是任何**談論**權限框的畫面都會命中——實測一個正常
/// 工作中的 coordinator pane 被誤判 19/24≈79%，`rg 'Requesting permission
/// for:|Do you want to proceed'` 這行指令回顯自己就湊齊三組特徵。三道收窄：
/// ①只掃下緣 `TAIL_LINES` 行；②組內片段須落在 `PROXIMITY_LINES` 行的鄰近窗內；
/// ③`Requesting permission for:` 單錨降級——MUST 與同框的選項行／footer 成對。
/// 誤判方向是漏送通知（任務仍在 mailbox）＋TUI 假警報，比誤批權限框輕；但
/// 收窄同時把漏判風險壓在量出來的餘裕內，不是拿安全換乾淨。
pub fn screen_has_prompt(screen: &str) -> bool {
    prompt_window(screen).is_some()
}

/// 一個鄰近窗（已摺疊空白）是否命中特徵。判定**只有這一份**：`screen_has_prompt`
/// 與 `prompt_snippet` 共用它，否則 TUI 顯示的「框內容」會有機會與送鍵防線
/// 實際攔到的東西不是同一件事。
fn window_hits(window: &str) -> bool {
    // 特徵組：**組內全部片段**都要落在同一個鄰近窗內才算命中。
    let groups: [&[&str]; 3] = [
        &["Do you want to ", "Esc to cancel"],
        &["has written up a plan", "Would you like to proceed"],
        &["Do you want to proceed?", "esc to cancel"],
    ];
    // 單錨降級後的伴隨特徵：同框的問句、footer、或第一個選項行，任一即可。
    let companions: [&str; 4] = [
        "Do you want to ",
        "esc to cancel",
        "Esc to cancel",
        "1. Yes",
    ];
    groups
        .iter()
        .any(|g| g.iter().all(|frag| window.contains(frag)))
        || (window.contains("Requesting permission for:")
            && companions.iter().any(|c| window.contains(c)))
}

/// **第一個**命中的鄰近窗在 `screen_lines` 裡的行範圍（含兩端）。
///
/// `screen_has_prompt` 原本就是「掃到第一個命中的窗即回 true」，抽出範圍不改
/// 那條語意，只是把「是哪一個窗命中」這件已經算出來的事保留下來。
fn prompt_window(screen: &str) -> Option<(usize, usize)> {
    tail_windows(screen)
        .into_iter()
        .find(|(_, _, w)| window_hits(w))
        .map(|(s, e, _)| (s, e))
}

/// **最後**一個命中窗的行範圍——snippet 專用。
///
/// 判定用的是第一個命中窗（掃到就回，語意不變），但那個窗在特徵湊齊的當下就
/// 結束了：agy 的框在 `Do you want to proceed?` 那行就已經與 header 湊成一組，
/// 於是第一個命中窗切在問句上、選項與 footer 全在窗外。要顯示「框在問什麼、
/// 有哪些選項」就得取延伸到最深的那一個。
fn prompt_window_last(screen: &str) -> Option<(usize, usize)> {
    tail_windows(screen)
        .into_iter()
        .rfind(|(_, _, w)| window_hits(w))
        .map(|(s, e, _)| (s, e))
}

/// TUI 顯示用的框內容上限：命中窗的**最後**這麼多行。
///
/// 為什麼有界：鄰近窗最寬 `PROXIMITY_LINES` 行，整段塞進 DETAIL 會把等價 CLI
/// 原文推出畫面（薄殼原則）。取尾端而非開頭，是因為選項行與 footer 貼在框底
/// ——「它在問什麼、有哪些選項」都在那裡。
pub const PROMPT_SNIPPET_MAX_LINES: usize = 6;

/// 命中窗的原文尾行（**不摺疊空白**：那是給比對用的正規化，不是人要讀的東西）。
///
/// 回 `None` 代表這一屏沒有命中——與 `screen_has_prompt` 同一份判定，不會出現
/// 「標了 blocked 卻沒有內容」或反過來的矛盾。
///
/// 空行**不佔額度**：框內的留白在終端機上是排版，搬進 DETAIL 那幾行的預算裡
/// 就是把選項行擠出去。丟掉的只有空白，每一行文字都照原文（含縮排）。
pub fn prompt_snippet(screen: &str) -> Option<Vec<String>> {
    let (start, end) = prompt_window_last(screen)?;
    let lines = screen_lines(screen);
    let mut kept: Vec<String> = lines[start..=end]
        .iter()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if kept.len() > PROMPT_SNIPPET_MAX_LINES {
        kept.drain(..kept.len() - PROMPT_SNIPPET_MAX_LINES);
    }
    Some(kept)
}

/// 下緣區內的鄰近窗序列（每個窗已摺疊空白，可直接 `contains`）。
///
/// 尾端全空白行先剝掉：`capture-pane` 會把 pane 底部沒寫到的列補成空行，
/// 不剝的話「距底幾行」會被這些空行整批推高，下緣錨等於白設。
///
/// 窗一律**夾在下緣區內**（`start` 不小於 `tail_start`）：讓窗往上越界，
/// 等於把上半部的指令回顯又拉回比對範圍，收窄失效。
fn tail_windows(screen: &str) -> Vec<(usize, usize, String)> {
    let lines = screen_lines(screen);
    if lines.is_empty() {
        return Vec::new();
    }
    let tail_start = lines.len().saturating_sub(TAIL_LINES);
    let tail = &lines[tail_start..];
    (0..tail.len())
        .map(|end| {
            let start = end.saturating_sub(PROXIMITY_LINES - 1);
            (
                tail_start + start,
                tail_start + end,
                fold_whitespace(&tail[start..=end].join("\n")),
            )
        })
        .collect()
}

/// 一屏的行（尾端全空白行已剝除）。`tail_windows` 的行索引與 `prompt_snippet`
/// 取原文都吃這一份，兩邊各切一次的話索引會對不上。
fn screen_lines(screen: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = screen.split('\n').collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// 送鍵前的兩道畫面關卡，`notify_pane` 在送文字前與送 Enter 前各跑一次。
///
/// 第一道是 copy-mode（**AB-COPYMODE-1**）：pane 停在 tmux 的 copy-mode 時，
/// `send-keys` 送出的字元會落進 copy-mode 的按鍵表而不是 worker 的輸入，實測
/// 還會讓 `send-keys` 子行程**永不返回**（`agent-bridge receive <id>` 這串在
/// vi copy-mode 下 5 秒逾時砍不掉自己）。這正是「人為介入」情境本身——人一
/// 捲動 worker pane 就進 copy-mode——卻會把 orchestrator 的 `send` 整個鎖死。
///
/// **不得**自作主張送 `-X cancel` 把人踢出 copy-mode：那會清掉人正在看的捲動
/// 位置，是在「人正在介入」時破壞人的現場。降級成 notify-failed 即可，訊息
/// 仍在 mailbox，人在 pane 裡自己收得到。
///
/// 第二道是權限確認對話框：那個 Enter 會被當成「確認預設選項」，替一個正等
/// 人類決策的 worker 按下批准。
///
/// 兩道都 fail-closed（讀不出來一律回 `false`）：無法確認 pane 狀態時放行送
/// 鍵，等於整條防線被略過。
fn pane_accepts_keys(
    tmux: &dyn TmuxClient,
    pane: &str,
) -> std::result::Result<(), NotifyFailReason> {
    match tmux.pane_in_mode(pane) {
        Some(false) => {}
        Some(true) => return Err(NotifyFailReason::CopyMode),
        // mode 查不到＝tmux 查詢失敗／逾時，fail-closed；歸 query-failed 桶
        // （pane 是否存在前面已驗過，這裡的 None 是查詢層失敗，不是 pane 不在）
        None => return Err(NotifyFailReason::QueryFailed),
    }
    match tmux.capture_pane(pane) {
        Some(screen) if screen_has_prompt(&screen) => Err(NotifyFailReason::Prompt),
        Some(_) => Ok(()),
        None => Err(NotifyFailReason::QueryFailed),
    }
}

/// 一次送鍵失敗的**原因分類**，寫進 events.log 的 `notify-failed … reason=<v>`。
///
/// 存在的理由：四個關卡原本共用同一個 `false`，事後只剩「失敗了」三個字，
/// 誰擋的分不出來。2026-08-01 那次 matcher 誤判調查最大的阻力就是這個缺口
/// ——只能靠「哪些 runtime／哪些 cmd 失敗率高」的事後統計反推根因。
///
/// 五個值是**分類桶**，不是關卡的一對一映射。原本 `PaneGone` 同時吃下
/// 「pane 真的不在」與「pane 狀態查不到」；2026-08-03 拆出 `QueryFailed`：
/// codex sandbox 擋 tmux socket 時查詢層整排失敗，全記 `pane-gone` 會把
/// **活著的 pane** 誤報成消失（實例 2026-08-01T13:03:58Z pane=%139 活著），
/// 事後分析與 await 的 blocker 探測都需要分開「暫時查不到（別當死亡證據）」
/// 與「確認不在」。兩者對送鍵呼叫端的處置仍相同（fail-closed，訊息留
/// mailbox）——拆的是**證據語意**，不是處置分支。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotifyFailReason {
    /// pane 停在 tmux copy-mode（AB-COPYMODE-1）
    CopyMode,
    /// 畫面停在權限／計畫確認框（HOOK-NOTIFY-2）
    Prompt,
    /// `send-keys` 這一步失敗。**不等於逾時**：`send_keys()` 只回 bool，
    /// 逾時（ENV-TMUX-1 的 bounded 上限）、非零退出、pane 在 TOCTOU 空窗裡
    /// 消失、tmux 子行程根本沒起來，全都塌成同一個 `false`——從這裡分不出
    /// 根因，所以字面值 MUST 是 `send-keys-failed` 而不是 `-timeout`，
    /// 免得事後分析把「pane 剛好消失」誤讀成「tmux 卡住」。
    SendKeysFailed,
    /// pane 確認不存在，或 pane id 非法（資料層壞掉，pane 實質不可用）
    PaneGone,
    /// tmux 查詢層失敗（mode／capture 回 None、tmux 不可用）：pane 可能
    /// 還活著，只是此刻查不到——MUST NOT 當成 pane 死亡的證據
    QueryFailed,
}

impl NotifyFailReason {
    /// events.log 用的字面值（**穩定契約**：消費端據此分類，改動要同步 spec）
    pub fn as_str(self) -> &'static str {
        match self {
            NotifyFailReason::CopyMode => "copy-mode",
            NotifyFailReason::Prompt => "prompt",
            NotifyFailReason::SendKeysFailed => "send-keys-failed",
            NotifyFailReason::PaneGone => "pane-gone",
            NotifyFailReason::QueryFailed => "query-failed",
        }
    }
}

/// await 的 blocker 探測結果（唯讀，不送鍵）。與 `NotifyFailReason` 分開：
/// 這裡是「pane 現在處於什麼狀態」的觀測，不是「一次送鍵敗在哪」的事故分類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneBlocker {
    /// 畫面正常，無阻塞跡象
    Clear,
    /// 停在 copy-mode——通常是人正在介入（捲動查看），不是卡死
    CopyMode,
    /// 停在權限／計畫確認框——無人值守時的黑洞來源
    Prompt,
    /// pane 確認不存在
    Gone,
    /// 查詢層失敗（tmux 不可用、mode／capture 回 None、pane id 非法）：
    /// 此刻無法判定，呼叫端 MUST NOT 據此警告或歸零去抖計數
    Unknown,
}

/// `agent-bridge await` 的週期性 blocker 探測（CLI-AWAIT-3：唯讀，只做 tmux
/// 查詢，不寫檔、不寫事件、不送任何鍵）。重用送鍵防線同一套判定
/// （`pane_in_mode`＋`capture_pane`＋`screen_has_prompt`），避免兩套 matcher
/// 各自漂移。誤判防護（去抖、grace）是呼叫端的責任——本函式只回單次觀測。
pub fn probe_blocker(tmux: &dyn TmuxClient, pane: &str) -> PaneBlocker {
    if !is_valid_pane(pane) || !tmux.available() {
        return PaneBlocker::Unknown;
    }
    if !tmux.pane_exists(pane) {
        return PaneBlocker::Gone;
    }
    match tmux.pane_in_mode(pane) {
        Some(true) => return PaneBlocker::CopyMode,
        Some(false) => {}
        None => return PaneBlocker::Unknown,
    }
    match tmux.capture_pane(pane) {
        Some(screen) if screen_has_prompt(&screen) => PaneBlocker::Prompt,
        Some(_) => PaneBlocker::Clear,
        None => PaneBlocker::Unknown,
    }
}

/// notify_pane:330 — 先驗 pane 存活、過畫面關卡，再分兩次送鍵（文字、Enter）。
/// 任一關卡失敗都回 `false`（呼叫端走 notify-failed 降級：訊息仍在 mailbox，
/// 可復原）。
pub fn notify_pane(tmux: &dyn TmuxClient, pane: &str, cmd: &str) -> bool {
    notify_pane_reason(tmux, pane, cmd).is_ok()
}

/// `notify_pane` 的帶原因版本：成功回 `Ok(())`，失敗回擋下它的關卡。
///
/// 兩者同一份流程（`notify_pane` 只是丟掉原因的薄殼），避免兩條路徑各自漂移。
pub fn notify_pane_reason(
    tmux: &dyn TmuxClient,
    pane: &str,
    cmd: &str,
) -> std::result::Result<(), NotifyFailReason> {
    if !is_valid_pane(pane) {
        return Err(NotifyFailReason::PaneGone);
    }
    // tmux 整個不可用＝查詢層失敗：分不出 pane 死活，不得記 pane-gone
    if !tmux.available() {
        return Err(NotifyFailReason::QueryFailed);
    }
    if !tmux.pane_exists(pane) {
        return Err(NotifyFailReason::PaneGone);
    }
    pane_accepts_keys(tmux, pane)?;
    if !tmux.send_keys(pane, cmd) {
        return Err(NotifyFailReason::SendKeysFailed);
    }
    sleep_notify_delay();
    // 送 Enter 前再過一次同樣的關卡：worker 可能在這段延遲內才彈框，人也可能
    // 剛好在這段延遲內捲動 pane 進 copy-mode。殘留 race 與 bash 同（檢查與
    // send-keys 之間的微小空窗，tmux 給不了 pane-side 原子性）——那道空窗由
    // `send_keys` 自身的逾時兜底（tmux.rs `wait_with_timeout`），不會變成
    // 永久鎖死。
    pane_accepts_keys(tmux, pane)?;
    if !tmux.send_keys(pane, "Enter") {
        return Err(NotifyFailReason::SendKeysFailed);
    }
    Ok(())
}

/// `sleep "${AGENT_BRIDGE_NOTIFY_DELAY:-0.3}"`：agent REPL 會把同批抵達的
/// 文字＋Enter 當成貼上而吞掉 Enter，故兩次送鍵之間隔一小段。
///
/// bash 未驗證這個值的格式，壞值讓 `sleep` 立刻失敗；而 `notify_pane` 是被
/// `if` 包住呼叫的（errexit 在該語境抑制），失敗後**繼續往下執行**。此處
/// 對齊該終態：解析不出正數就不睡，直接進第二次掃描。
fn sleep_notify_delay() {
    if let Some(secs) = config::notify_delay_secs() {
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    }
}

/// 一次通知嘗試的終態，與寫進 events.log 的事件字一一對應。
///
/// 存在的理由：alternate-screen 的消費端（TUI）不能讓 helper 直接 `eprintln!`
/// ——stderr 會畫花畫面且訊息進不了 footer。故把「決定」與「印字」拆開：
/// 這裡只回終態，印字留在 CLI 外殼（`notify_or_defer`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotifyOutcome {
    /// 送鍵成功（`notified`）
    Notified,
    /// 對方新鮮 busy，延後由 hook 取件（`notify-deferred`）
    Deferred,
    /// 送鍵失敗，需人工在對方 session 執行（`notify-failed`）
    Failed,
}

/// notify_or_defer:371 — send／respond_task／cancel 三個通知呼叫點共用的 gate。
///
/// state/<agent>.json 的語意是「建議非權威」：讀不到、解析失敗、或 ts 已超過
/// `AGENT_BRIDGE_STATE_TTL` 秒都視為「未知」，直接落回 notify_pane 路徑。
/// 只有明確讀到 state=busy 且新鮮，才完全不送鍵。
///
/// **函式內不印任何字**（事件照記）：呈現層由呼叫端決定。CLI 走
/// `notify_or_defer`（印 stderr，逐字沿用 bash 正本文案）。
pub fn notify_or_defer_outcome(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    agent: &str,
    pane: &str,
    cmdline: &str,
    task_id: &str,
    tag: &str,
) -> Result<NotifyOutcome> {
    let ttl = config::state_ttl_strict()?;
    let mut fresh_busy = false;

    // ttl=0＝整條 state 通道關閉（比照 AGENT_BRIDGE_READY_TIMEOUT 的 0 語意），
    // 一律當「未知」走 legacy 送鍵。
    let state_file = paths.state_dir.join(format!("{agent}.json"));
    if ttl > 0
        && let Ok(content) = std::fs::read_to_string(&state_file)
        && let Ok(Value::Object(fields)) = json::parse(&content)
    {
        let st = json::str_field(&fields, "state").unwrap_or_default();
        let ts = json::str_field(&fields, "ts").unwrap_or_default();
        if !st.is_empty()
            && !ts.is_empty()
            && let Some(epoch) = parse_iso_to_epoch(ts)
        {
            let age = now_epoch() - epoch;
            // 下界（age >= 0）：ts 落在未來一律不可信——若不擋，未來時間戳會
            // 讓 busy 永遠判定為新鮮，通知因此永久停擺，且沒有 TTL 能救回來。
            if st == "busy" && age >= 0 && age <= ttl {
                fresh_busy = true;
            }
        }
    }

    if fresh_busy {
        log_event(
            paths,
            task_id,
            "notify-deferred",
            &format!("pane={pane} cmd={tag}"),
        )?;
        return Ok(NotifyOutcome::Deferred);
    }

    match notify_pane_reason(tmux, pane, cmdline) {
        Ok(()) => {
            log_event(
                paths,
                task_id,
                "notified",
                &format!("pane={pane} cmd={tag}"),
            )?;
            Ok(NotifyOutcome::Notified)
        }
        Err(reason) => {
            // `reason=` **append 在既有欄位之後**：現有解析（測試、hook、人眼）
            // 都是前綴比對或 grep，加在尾端不會挪動 pane=／cmd= 的位置。
            log_event(
                paths,
                task_id,
                "notify-failed",
                &format!("pane={pane} cmd={tag} reason={}", reason.as_str()),
            )?;
            Ok(NotifyOutcome::Failed)
        }
    }
}

/// CLI 外殼：終態 → stderr 文案（與 bash 正本逐字一致，既有測試以 byte 級
/// 比對這兩行）。TUI 不走這條，改用 `notify_or_defer_outcome` 自行呈現。
pub fn notify_or_defer(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    agent: &str,
    pane: &str,
    cmdline: &str,
    task_id: &str,
    tag: &str,
) -> Result<()> {
    let outcome = notify_or_defer_outcome(paths, tmux, agent, pane, cmdline, task_id, tag)?;
    if let Some(msg) = outcome_message(outcome, agent, pane, cmdline) {
        eprintln!("agent-bridge: {msg}");
    }
    Ok(())
}

/// 通知終態 → 給人看的一句話（**不含** `agent-bridge: ` 前綴）。
///
/// 單一正本：CLI 的 `notify_or_defer` 與 evict 編排（它走不印字的
/// `notify_or_defer_outcome`，訊息經事件交回外殼）共用同一份文案，兩邊分家
/// 就會有一邊悄悄漂移，而既有測試以 byte 級比對這兩行。
/// `Notified`＝沒有話要說（成功不吵人）。
pub fn outcome_message(
    outcome: NotifyOutcome,
    agent: &str,
    pane: &str,
    cmdline: &str,
) -> Option<String> {
    match outcome {
        NotifyOutcome::Notified => None,
        NotifyOutcome::Deferred => Some(format!(
            "提示：{agent} 目前忙碌中，通知延後——訊息已在 mailbox，對方 turn 結束時會由 hook 自行取件"
        )),
        NotifyOutcome::Failed => Some(format!(
            "警告：無法通知 {agent}（pane {pane}）；請手動在對方 session 執行：{cmdline}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 可調三個關卡回應的 tmux 替身，並記下實際送出的鍵。
    ///
    /// `modes` 是**逐次查詢**的回應序列（用完後沿用最後一個）：`notify_pane`
    /// 會掃兩次，而「第一次乾淨、第二次才進 copy-mode」正是人在 0.3 秒延遲裡
    /// 捲動 pane 的情形——固定值的替身測不出第二道關卡是否真的存在。
    struct FakeTmux {
        modes: Vec<Option<bool>>,
        mode_calls: std::cell::Cell<usize>,
        screen: Option<&'static str>,
        sent: RefCell<Vec<String>>,
        /// `send_keys` 的回傳。`false` 對映實作拿得到的全部送鍵失敗
        /// （逾時／非零退出／pane 剛好消失／子行程沒起來）。
        send_ok: bool,
    }

    impl FakeTmux {
        fn new(in_mode: Option<bool>, screen: Option<&'static str>) -> Self {
            Self::with_modes(vec![in_mode], screen)
        }

        fn with_modes(modes: Vec<Option<bool>>, screen: Option<&'static str>) -> Self {
            Self {
                modes,
                mode_calls: std::cell::Cell::new(0),
                screen,
                sent: RefCell::new(Vec::new()),
                send_ok: true,
            }
        }

        /// 送鍵一律失敗的替身（唯一構造得出 `SendKeysFailed` 的路徑）。
        fn with_failing_send_keys(screen: Option<&'static str>) -> Self {
            let mut f = Self::new(Some(false), screen);
            f.send_ok = false;
            f
        }
    }

    impl TmuxClient for FakeTmux {
        fn exec(&self, _args: &[&str]) -> Option<crate::tmux::TmuxOutput> {
            None
        }
        fn available(&self) -> bool {
            true
        }
        fn resolve_pane(&self, _t: &str) -> Option<String> {
            None
        }
        fn pane_exists(&self, _p: &str) -> bool {
            true
        }
        fn capture_pane(&self, _p: &str) -> Option<String> {
            self.screen.map(|s| s.to_string())
        }
        fn pane_in_mode(&self, _p: &str) -> Option<bool> {
            let n = self.mode_calls.get();
            self.mode_calls.set(n + 1);
            self.modes[n.min(self.modes.len() - 1)]
        }
        fn send_keys(&self, _p: &str, keys: &str) -> bool {
            self.sent.borrow_mut().push(keys.to_string());
            self.send_ok
        }
    }

    /// AB-COPYMODE-1：pane 停在 copy-mode 時 MUST NOT 送鍵。
    ///
    /// 「回 false」單獨不夠——**一個鍵都不能送出去**才是重點：copy-mode 中的
    /// send-keys 實測會永不返回，而且會把人正在看的捲動現場攪掉。
    #[test]
    fn copy_mode_pane_gets_no_keys_at_all() {
        let tmux = FakeTmux::new(Some(true), Some("$ ls\nfoo\n"));
        assert!(!notify_pane(&tmux, "%1", "agent-bridge receive t1"));
        assert!(tmux.sent.borrow().is_empty());
    }

    /// mode 讀不出來（tmux 查詢失敗）→ fail-closed，同 capture 失敗的方向：
    /// 無法確認 pane 狀態時放行送鍵，等於整條防線被略過。
    #[test]
    fn unknown_mode_fails_closed() {
        let tmux = FakeTmux::new(None, Some("$ ls\nfoo\n"));
        assert!(!notify_pane(&tmux, "%1", "agent-bridge receive t1"));
        assert!(tmux.sent.borrow().is_empty());
    }

    /// 三個關卡都乾淨才送鍵，且是「文字、Enter」兩次。
    #[test]
    fn clean_pane_gets_command_then_enter() {
        let tmux = FakeTmux::new(Some(false), Some("$ ls\nfoo\n"));
        assert!(notify_pane(&tmux, "%1", "agent-bridge receive t1"));
        assert_eq!(
            *tmux.sent.borrow(),
            vec!["agent-bridge receive t1".to_string(), "Enter".to_string()]
        );
    }

    /// 人在兩次送鍵之間的延遲裡捲動了 pane：第二道關卡必須攔下 Enter。
    ///
    /// 沒有這條，把送 Enter 前的 mode 檢查刪掉一樣全綠——而那個 Enter 會落進
    /// copy-mode 的按鍵表，還可能讓 send-keys 卡住。
    #[test]
    fn entering_copy_mode_during_the_delay_blocks_the_enter() {
        let tmux = FakeTmux::with_modes(vec![Some(false), Some(true)], Some("$ ls\nfoo\n"));
        assert!(!notify_pane(&tmux, "%1", "agent-bridge receive t1"));
        // 文字已送出（第一次掃描時畫面還乾淨），但 Enter 必須被擋下
        assert_eq!(*tmux.sent.borrow(), vec!["agent-bridge receive t1"]);
    }

    /// copy-mode 的關卡不得取代權限框的關卡：不在 mode、但畫面停在權限框時，
    /// 仍然一個鍵都不送。
    #[test]
    fn permission_box_still_blocks_when_not_in_mode() {
        let tmux = FakeTmux::new(Some(false), Some("Do you want to proceed? … Esc to cancel"));
        assert!(!notify_pane(&tmux, "%1", "agent-bridge receive t1"));
        assert!(tmux.sent.borrow().is_empty());
    }

    /// CC canary 的 Rust 側對應（分組 30）：套件那組把特徵字串拿去比對安裝中
    /// 的 claude 執行檔，抽取來源是 **bash 正本**（源碼耦合檢查，M4 才改綁
    /// Rust 源）。在那之前，這個測試守住「Rust 的 matcher 用的是同一組特徵」
    /// ——否則 canary 盯著 bash、Rust 這邊悄悄改掉特徵也不會有人發現。
    #[test]
    fn matcher_uses_the_canary_feature_strings() {
        assert!(screen_has_prompt(
            "… Do you want to proceed? … Esc to cancel …"
        ));
        assert!(screen_has_prompt(
            "Claude has written up a plan. Would you like to proceed?"
        ));
        // 兩組特徵各自成對才算命中：單邊出現不足以判定是權限框
        assert!(!screen_has_prompt("Do you want to"));
        assert!(!screen_has_prompt("Esc to cancel"));
        assert!(!screen_has_prompt("has written up a plan"));
        assert!(!screen_has_prompt("Would you like to proceed"));
    }

    /// agy 權限框（AGY-PROMPT-1）：實測畫面逐字，含小寫 `esc to cancel`
    /// footer——前兩組特徵一個都不命中，只有第三組錨救得回來。
    #[test]
    fn agy_permission_box_is_detected() {
        let screen = "● Bash(./bin/agent-bridge receive t1)\n\nCommand\n\
             ────────\n\nRequesting permission for:\n   ./bin/agent-bridge receive t1\n\n\
             Do you want to proceed?\n> 1. Yes\n  4. No\n\nesc to cancel";
        assert!(screen_has_prompt(screen));
        // **單錨降級**（2026-08-01）：header 自己不再成立，MUST 與同框的問句／
        // 選項行／footer 成對。這條原本是正向斷言——它正是 19/24 誤判的主因，
        // 任何「談論」權限框的畫面都會踩中它。
        assert!(!screen_has_prompt("Requesting permission for:"));
        // 成對即命中（header ＋ 同框選項行）
        assert!(screen_has_prompt(
            "Requesting permission for:\n   ./bin/ab receive t1\n> 1. Yes\n  4. No"
        ));
        // 矮 pane：header 被捲出一屏，只剩框的下緣——備援錨要接住
        assert!(screen_has_prompt(
            "Do you want to proceed?\n> 1. Yes\n  4. No\n\nesc to cancel"
        ));
        // agy 執行中的常駐 footer 不足以構成特徵
        assert!(!screen_has_prompt("⣽ Running...\nesc to cancel"));
        // 備援錨要求成對：單邊出現不算
        assert!(!screen_has_prompt("Do you want to proceed?"));
    }

    /// **snippet 與判定同源**（P5.4 blocker snippet）：命中才有內容、沒命中就
    /// 沒有；內容取的是命中窗的**原文尾行**（框在問什麼、有哪些選項都在那裡），
    /// 而且有界——整個鄰近窗塞進 DETAIL 會把等價 CLI 原文推出畫面。
    #[test]
    fn the_prompt_snippet_comes_from_the_very_window_that_matched() {
        let screen = "● Bash(./bin/agent-bridge receive t1)\n\nCommand\n\
             ────────\n\nRequesting permission for:\n   ./bin/agent-bridge receive t1\n\n\
             Do you want to proceed?\n> 1. Yes\n  4. No\n\nesc to cancel";
        let snip = prompt_snippet(screen).expect("命中的畫面 MUST 有 snippet");
        assert!(
            snip.len() <= PROMPT_SNIPPET_MAX_LINES,
            "有界：{} 行 > 上限 {PROMPT_SNIPPET_MAX_LINES}",
            snip.len()
        );
        assert_eq!(
            snip.last().map(String::as_str),
            Some("esc to cancel"),
            "尾行取的是框底（footer／選項），不是框頂"
        );
        assert!(
            snip.iter().any(|l| l.contains("Do you want to proceed?")),
            "框在問什麼 MUST 在 snippet 裡：{snip:?}"
        );
        // **原文**，不是比對用的摺疊正規化：縮排照留
        assert!(
            snip.iter().any(|l| l.starts_with("> 1. Yes")),
            "選項行照原文：{snip:?}"
        );
        assert!(
            snip.iter().all(|l| !l.is_empty()),
            "空行不佔 DETAIL 的行預算：{snip:?}"
        );

        // 沒命中就沒有內容——不得出現「標了 blocked 卻拿不到框」的矛盾
        assert!(prompt_snippet("$ ls\nfoo bar\n").is_none());
        assert!(prompt_snippet("Requesting permission for:").is_none());
        // 兩者永遠同進同出
        for s in [screen, "$ ls\nfoo bar\n", "Do you want to proceed?"] {
            assert_eq!(
                screen_has_prompt(s),
                prompt_snippet(s).is_some(),
                "判定與 snippet MUST 同源：{s:?}"
            );
        }
    }

    #[test]
    fn pane_re_rejects_injection_shapes() {
        assert!(is_valid_pane("%0"));
        assert!(is_valid_pane("%123"));
        assert!(!is_valid_pane("%1 ; kill-server"));
        assert!(!is_valid_pane("%"));
        assert!(!is_valid_pane(""));
        assert!(!is_valid_pane("1"));
    }

    /// 特徵片段被折行拆散時仍 MUST 判定為確認框（漏判＝放行誤批的 Enter）。
    #[test]
    fn prompt_detection_survives_wrapping() {
        assert!(screen_has_prompt(
            "Do you want to proceed?\n  1. Yes\n  Esc to cancel"
        ));
        // 窄 pane 硬折行：特徵片段被拆到兩行
        assert!(screen_has_prompt(
            "Do you want to\nproceed?\nEsc to\ncancel"
        ));
        // plan mode 退出框（不含第一組特徵）
        assert!(screen_has_prompt(
            "Claude has written up a plan and is ready to execute.\nWould you like to proceed?"
        ));
    }

    #[test]
    fn ordinary_output_is_not_a_prompt() {
        assert!(!screen_has_prompt("$ ls\nfoo bar\n"));
        // 單獨一個片段不構成特徵
        assert!(!screen_has_prompt("Do you want to know more?"));
        assert!(!screen_has_prompt("Esc to cancel"));
    }

    /// 一個 coordinator pane 的**下緣**長相：輸入框＋statusline，共 14 行。
    /// 誤判語料裡的命中片段全部落在這塊之上（最淺的距底 16 行）。
    fn coordinator_footer() -> String {
        [
            "✻ Brewed for 4s",
            "                                              41282 tokens",
            "────────────────────────────────────────────────────────",
            "❯ ",
            "────────────────────────────────────────────────────────",
            "   Opus 5 [1M] │ ⛁ ⛶⛶⛶⛶ 19% │  agent-bridge (rust/m0.5)",
            "   $27.57 / $451.50 │  8h 59m │ 󰓅 99%",
            "   5H ━━━───────────────────  11% ↻ 1:00pm",
            "   7D ━━━━━━━━━─────────────  31% ↻ aug 6 2:00am",
            "  -- INSERT -- ⏵⏵ auto mode on · ← 1 agent",
            "                                                     /rc",
            "",
            "  ◯ main",
            "  ● ✓ opus-5[1m]  [查通知失敗根因] 78.5kt 8h52m",
        ]
        .join("\n")
    }

    /// 誤判回歸（2026-08-01 語料，19/24≈79%）——**談論**權限框的畫面 MUST NOT
    /// 命中。兩類都從真實幀萃取最小合成行（原始截圖不進 repo）：
    ///
    /// - class B：一行 `rg` 指令回顯同時湊齊三組特徵（poll-133-8/9/10，距底 16）
    /// - class A：散文引用 header 單錨（poll-133-20..24，距底 27）
    ///
    /// 沒有這條，把 `TAIL_LINES` 改回整屏（或讓鄰近窗越過下緣區界）就會靜默
    /// 回到 79% 誤判——那正是 P4 之後 TUI 常駐假 `⛔blocked` 的來源。
    #[test]
    fn talking_about_a_permission_box_is_not_a_permission_box() {
        // class B：指令回顯，四個片段擠在同一行
        let echo_line = "● Bash(rg -n \"Requesting permission for:|Do you want to \
             proceed|esc to cancel|Esc to cancel\" $D/notify-captures/poll-133-1.txt)";
        let screen_b = format!(
            "{}\n{}\n{}",
            "  上半部的工作內容\n".repeat(14).trim_end(),
            echo_line,
            coordinator_footer()
        );
        assert!(!screen_has_prompt(&screen_b));

        // class A：散文引用 header 單錨
        let prose =
            "  ……；P3 的 Requesting permission for: 單錨應降級為必須與同框的選項行或 footer 成對。";
        let screen_a = format!(
            "{}\n{}\n{}",
            "  修法建議（一段話，未實作）\n".repeat(12).trim_end(),
            prose,
            coordinator_footer()
        );
        assert!(!screen_has_prompt(&screen_a));

        // class C：錨**落在下緣區內**但沒有同框伴隨特徵——位置錨擋不住它，
        // 只有單錨降級擋得住。語料裡的 class A 錨在距底 27 行，被位置錨先攔下，
        // 於是降級那條規則沒有獨立守衛；這條補上（突變驗證 e 的缺口）。
        let screen_c = "worker log: skipping Requesting permission for: probes\n\
             tail line 1\ntail line 2\ntail line 3";
        assert!(!screen_has_prompt(screen_c));
    }

    /// frag 代碼 → 字面值。**單一正本**：資料表只存代碼，字面值只在這裡，
    /// 兩邊不會各自漂移。
    fn frag_literal(code: &str) -> &'static str {
        match code {
            "P1a" => "Do you want to ",
            "P1b" => "Esc to cancel",
            "P2a" => "has written up a plan",
            "P2b" => "Would you like to proceed",
            "P3" => "Requesting permission for:",
            "P4a" => "Do you want to proceed?",
            "P4b" => "esc to cancel",
            other => panic!("資料表用了未知的 frag 代碼：{other}"),
        }
    }

    /// 收窄**前**的判定（整屏無錨 substring），逐字照 2081887 的實作。
    /// 只用來證明資料表每一列在收窄前真的會命中——沒有這個對照，一列
    /// 「本來就不命中」的資料混進表裡也會全綠，gate 等於沒設。
    fn whole_screen_hit(screen: &str) -> bool {
        let n = fold_whitespace(screen);
        (n.contains("Do you want to ") && n.contains("Esc to cancel"))
            || (n.contains("has written up a plan") && n.contains("Would you like to proceed"))
            || n.contains("Requesting permission for:")
            || (n.contains("Do you want to proceed?") && n.contains("esc to cancel"))
    }

    /// 19 幀誤判語料的**持久 gate**（tests/fixtures/matcher-false-positives.tsv）。
    ///
    /// 原始截圖不入庫（含工作內容）；表裡只留 matcher 實際依賴的兩件事——哪些
    /// 特徵片段出現、各自距底幾行。這裡逐幀重建等價畫面再斷言 no-hit。
    ///
    /// 每一列都先過 `whole_screen_hit`：那是收窄前的實作。收窄前 MUST 命中、
    /// 收窄後 MUST NOT 命中——雙向都鎖住，才叫回歸 gate 而不是一堆恆真斷言。
    #[test]
    fn false_positive_corpus_is_a_persistent_gate() {
        let table = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/matcher-false-positives.tsv"
        ));
        let mut checked = 0;
        for line in table.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let mut cols = line.split('\t');
            let frame = cols.next().expect("frame 欄");
            let total: usize = cols
                .next()
                .expect("lines 欄")
                .parse()
                .expect("lines 是數字");
            let placements = cols.next().expect("placements 欄");

            // 依「距底行數」把片段放回等價畫面，其餘填無關內容
            let mut lines: Vec<String> = vec!["  ordinary worker output".to_string(); total];
            for p in placements.split(';') {
                let (code, depth) = p.split_once('@').expect("frag@from_bottom");
                let depth: usize = depth.parse().expect("from_bottom 是數字");
                let idx = total - 1 - depth;
                let lit = frag_literal(code);
                // 同一距底＝同一行：既有內容後面接著放，重現「一行湊齊多個特徵」
                if lines[idx].starts_with("  ordinary") {
                    lines[idx] = format!("  {lit}");
                } else {
                    lines[idx] = format!("{} {lit}", lines[idx]);
                }
            }
            let screen = lines.join("\n");

            assert!(
                whole_screen_hit(&screen),
                "{frame}：重建畫面在收窄前不命中——這列資料沒有代表性，gate 是假的"
            );
            assert!(
                !screen_has_prompt(&screen),
                "{frame}：收窄後仍誤判（距底 {placements}）"
            );
            checked += 1;
        }
        assert_eq!(checked, 19, "語料表應為 19 幀，實得 {checked}");
    }

    /// 收窄的反面：真框在下緣時 MUST 照樣命中，**即使**畫面上半部塞滿雜訊。
    /// 幾何取自 2026-08-01 實測（Claude Code v2.1.220，33×143）：框出現時取代
    /// 輸入框與 statusline，`Esc to cancel` 距底 0 行、問句距底 5 行。
    #[test]
    fn a_real_box_at_the_bottom_still_hits_under_noise() {
        let screen = format!(
            "{}\n{}",
            "  上半部的工作內容\n".repeat(18).trim_end(),
            [
                "─────────────────────────────────────────",
                " Bash command",
                "",
                "   perl -e 'print qq{ok}'",
                "   Print ok via perl one-liner",
                "",
                " This command requires approval",
                "",
                " Do you want to proceed?",
                " ❯ 1. Yes",
                "   2. Yes, and don't ask again for: perl *",
                "   3. No",
                "",
                " Esc to cancel · Tab to amend · ctrl+e to explain",
            ]
            .join("\n")
        );
        assert!(screen_has_prompt(&screen));
    }

    /// `capture-pane` 會把 pane 底部沒寫到的列補成空行。不剝掉的話，「距底幾行」
    /// 會被整批推高、下緣錨等於白設——真框反而漏判（最壞方向）。
    #[test]
    fn trailing_blank_padding_does_not_push_the_box_out_of_the_tail() {
        let screen = format!(
            "{}\nDo you want to proceed?\n❯ 1. Yes\nEsc to cancel{}",
            "noise\n".repeat(20).trim_end(),
            "\n".repeat(12)
        );
        assert!(screen_has_prompt(&screen));
    }

    /// 鄰近條件：同一組的兩個片段隔得太遠（仍同在下緣區內）MUST NOT 湊成命中。
    #[test]
    fn fragments_too_far_apart_do_not_pair() {
        // 中間墊 n 行雜訊 → 兩個片段相距 n+1 行
        let build = |gap: usize| {
            let mut lines = vec!["Do you want to proceed?".to_string()];
            lines.extend(std::iter::repeat_n("……無關內容……".to_string(), gap));
            lines.push("Esc to cancel".to_string());
            lines.join("\n")
        };
        // 相距 PROXIMITY_LINES 行：超出鄰近窗，MUST NOT 成對
        assert!(!screen_has_prompt(&build(PROXIMITY_LINES - 1)));
        // 相距 PROXIMITY_LINES-1 行：剛好同窗，MUST 命中
        assert!(screen_has_prompt(&build(PROXIMITY_LINES - 2)));
    }

    /// notify-failed 的 reason 分類：四個關卡各自可辨。
    #[test]
    fn notify_failure_reasons_are_distinguishable() {
        let copy = FakeTmux::new(Some(true), Some("$ ls\n"));
        assert_eq!(
            notify_pane_reason(&copy, "%1", "x"),
            Err(NotifyFailReason::CopyMode)
        );

        let prompt = FakeTmux::new(Some(false), Some("Do you want to proceed?\nEsc to cancel"));
        assert_eq!(
            notify_pane_reason(&prompt, "%1", "x"),
            Err(NotifyFailReason::Prompt)
        );

        // pane 狀態查不到＝查詢層失敗（2026-08-03 拆桶：不再與 pane-gone 混同）
        let unknown = FakeTmux::new(None, Some("$ ls\n"));
        assert_eq!(
            notify_pane_reason(&unknown, "%1", "x"),
            Err(NotifyFailReason::QueryFailed)
        );
        // pane id 非法＝資料層壞掉，維持 pane-gone
        let bad = FakeTmux::new(Some(false), Some("$ ls\n"));
        assert_eq!(
            notify_pane_reason(&bad, "%1 ; kill-server", "x"),
            Err(NotifyFailReason::PaneGone)
        );

        // capture 讀不出來也是查詢層失敗
        let nocap = FakeTmux::new(Some(false), None);
        assert_eq!(
            notify_pane_reason(&nocap, "%1", "x"),
            Err(NotifyFailReason::QueryFailed)
        );

        // 前兩道關卡都過、卡在送鍵本身 → send-keys-failed（唯一走得到的路徑）
        let nosend = FakeTmux::with_failing_send_keys(Some("$ ls\n"));
        assert_eq!(
            notify_pane_reason(&nosend, "%1", "x"),
            Err(NotifyFailReason::SendKeysFailed)
        );
        // 失敗的是第一次送鍵（文字），Enter MUST NOT 補送
        assert_eq!(*nosend.sent.borrow(), vec!["x".to_string()]);

        let ok = FakeTmux::new(Some(false), Some("$ ls\n"));
        assert_eq!(notify_pane_reason(&ok, "%1", "x"), Ok(()));
    }

    /// 字面值是寫進 events.log 的穩定契約（spec/hooks.md HOOK-NOTIFY-4）。
    #[test]
    fn reason_wire_values_are_stable() {
        assert_eq!(NotifyFailReason::CopyMode.as_str(), "copy-mode");
        assert_eq!(NotifyFailReason::Prompt.as_str(), "prompt");
        assert_eq!(
            NotifyFailReason::SendKeysFailed.as_str(),
            "send-keys-failed"
        );
        assert_eq!(NotifyFailReason::PaneGone.as_str(), "pane-gone");
        assert_eq!(NotifyFailReason::QueryFailed.as_str(), "query-failed");
    }

    /// probe_blocker：單次觀測的分類正確性（去抖與 grace 在 task.rs 測）。
    #[test]
    fn probe_blocker_classifies_each_state() {
        let clear = FakeTmux::new(Some(false), Some("$ ls\n"));
        assert_eq!(probe_blocker(&clear, "%1"), PaneBlocker::Clear);

        let copy = FakeTmux::new(Some(true), Some("$ ls\n"));
        assert_eq!(probe_blocker(&copy, "%1"), PaneBlocker::CopyMode);

        let prompt = FakeTmux::new(Some(false), Some("Do you want to proceed?\nEsc to cancel"));
        assert_eq!(probe_blocker(&prompt, "%1"), PaneBlocker::Prompt);

        // 查詢層失敗（mode／capture 回 None、pane id 非法）→ Unknown，不得當死亡
        let nomode = FakeTmux::new(None, Some("$ ls\n"));
        assert_eq!(probe_blocker(&nomode, "%1"), PaneBlocker::Unknown);
        let nocap = FakeTmux::new(Some(false), None);
        assert_eq!(probe_blocker(&nocap, "%1"), PaneBlocker::Unknown);
        let bad_id = FakeTmux::new(Some(false), Some("$ ls\n"));
        assert_eq!(probe_blocker(&bad_id, "%1 ; kill"), PaneBlocker::Unknown);
    }
}

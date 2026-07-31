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
/// 自己寫出 Do you want to …」湊成誤判。誤判方向是漏送通知（任務仍在
/// mailbox），比誤批權限框輕，但沒有理由白收。
///
/// **header 單錨不夠**（跨廠複核 2026-07-31 的 blocker）：掃描只看可見一屏
/// （`capture-pane -pJ`，不取 scrollback），而預設 worker 會進共用 window 並
/// `tiled` 均分——pane 一多就矮，框的 header 會被捲出畫面、只剩下緣的
/// 選項與 footer。那時 header 錨失效、claude 那組又因大寫 `Esc` 不命中，
/// 送鍵的 Enter 就落在 `1. Yes`。故第四組是下緣備援：完整句
/// `Do you want to proceed?` ＋小寫 `esc to cancel` 成對。成對要求把誤判面
/// 壓回「助理輸出剛好整句寫出那個問句」的窄縫，而不是任何含 esc 的畫面。
///
/// 比對前先把整屏空白（含換行）摺疊成單一空格，對映 `tr -s '[:space:]' ' '`：
/// TUI 的 word-wrap 與 tmux 軟折行會把特徵片段拆到兩行，逐行比對必偽陰性
/// （漏判＝放行誤批的 Enter，是最壞方向）。Rust 的 `char::is_whitespace` 涵蓋
/// Unicode 空白、比 C locale 的 `[:space:]` 寬，差異只會讓更多片段被拼接
/// 起來——偽陽性方向，落在 bash 註解既定的 fail-closed 偏攔代價內。
pub fn screen_has_prompt(screen: &str) -> bool {
    let mut norm = String::with_capacity(screen.len());
    let mut prev_ws = false;
    for c in screen.chars() {
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
    (norm.contains("Do you want to ") && norm.contains("Esc to cancel"))
        || (norm.contains("has written up a plan") && norm.contains("Would you like to proceed"))
        || norm.contains("Requesting permission for:")
        || (norm.contains("Do you want to proceed?") && norm.contains("esc to cancel"))
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
fn pane_accepts_keys(tmux: &dyn TmuxClient, pane: &str) -> bool {
    if tmux.pane_in_mode(pane) != Some(false) {
        return false;
    }
    match tmux.capture_pane(pane) {
        Some(screen) => !screen_has_prompt(&screen),
        None => false,
    }
}

/// notify_pane:330 — 先驗 pane 存活、過畫面關卡，再分兩次送鍵（文字、Enter）。
/// 任一關卡失敗都回 `false`（呼叫端走 notify-failed 降級：訊息仍在 mailbox，
/// 可復原）。
pub fn notify_pane(tmux: &dyn TmuxClient, pane: &str, cmd: &str) -> bool {
    if !is_valid_pane(pane) {
        return false;
    }
    if !tmux.available() {
        return false;
    }
    if !tmux.pane_exists(pane) {
        return false;
    }
    if !pane_accepts_keys(tmux, pane) {
        return false;
    }
    if !tmux.send_keys(pane, cmd) {
        return false;
    }
    sleep_notify_delay();
    // 送 Enter 前再過一次同樣的關卡：worker 可能在這段延遲內才彈框，人也可能
    // 剛好在這段延遲內捲動 pane 進 copy-mode。殘留 race 與 bash 同（檢查與
    // send-keys 之間的微小空窗，tmux 給不了 pane-side 原子性）——那道空窗由
    // `send_keys` 自身的逾時兜底（tmux.rs `wait_with_timeout`），不會變成
    // 永久鎖死。
    if !pane_accepts_keys(tmux, pane) {
        return false;
    }
    tmux.send_keys(pane, "Enter")
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

    if notify_pane(tmux, pane, cmdline) {
        log_event(
            paths,
            task_id,
            "notified",
            &format!("pane={pane} cmd={tag}"),
        )?;
        Ok(NotifyOutcome::Notified)
    } else {
        log_event(
            paths,
            task_id,
            "notify-failed",
            &format!("pane={pane} cmd={tag}"),
        )?;
        Ok(NotifyOutcome::Failed)
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
            }
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
            true
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
        // header 錨單獨成立即可
        assert!(screen_has_prompt("Requesting permission for:"));
        // 矮 pane：header 被捲出一屏，只剩框的下緣——備援錨要接住
        assert!(screen_has_prompt(
            "Do you want to proceed?\n> 1. Yes\n  4. No\n\nesc to cancel"
        ));
        // agy 執行中的常駐 footer 不足以構成特徵
        assert!(!screen_has_prompt("⣽ Running...\nesc to cancel"));
        // 備援錨要求成對：單邊出現不算
        assert!(!screen_has_prompt("Do you want to proceed?"));
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
}

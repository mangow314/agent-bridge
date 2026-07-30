//! 送鍵通知：權限框雙掃、通知失敗語意、`notify_or_defer` 的 state TTL gate
//! （hooks.md HOOK-NOTIFY-*、env.md ENV-TTL-1/2、ENV-NOTIFY-1）。對映 bash
//! `notify_pane`:330、`screen_has_prompt`:318、`notify_or_defer`:371。
//!
//! 警告訊息直接 `eprintln!` 到 stderr（同 `lock::LockGuard::release` 的既有
//! 先例）：它們是流程中的提示而非指令失敗，回不到 `ab` 的 `Err` 收斂層——
//! 那條路徑會把整個指令變成非零退出，而 bash 這裡是 `err` + 繼續。

use crate::error::{Error, Result};
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
/// 兩組特徵（權限框／plan mode 退出框）依 bash 逐字。
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
}

/// notify_pane:330 — 先驗 pane 存活、掃描確認框，再分兩次送鍵（文字、Enter）。
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
    // 送鍵前確認 pane 沒停在權限確認對話框：那個 Enter 會被當成「確認預設
    // 選項」，替一個正等人類決策的 worker 按下批准。capture 失敗一律
    // fail-closed（`None` → 回 false），不放行送鍵。
    match tmux.capture_pane(pane) {
        Some(screen) if !screen_has_prompt(&screen) => {}
        _ => return false,
    }
    if !tmux.send_keys(pane, cmd) {
        return false;
    }
    sleep_notify_delay();
    // 送 Enter 前再掃一次（同樣 fail-closed）：worker 可能在這段延遲內才彈框。
    // 殘留 race 與 bash 同（capture 與 send-keys 之間的微小空窗，tmux 給不了
    // pane-side 原子性）。
    match tmux.capture_pane(pane) {
        Some(screen) if !screen_has_prompt(&screen) => {}
        _ => return false,
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
    let raw = std::env::var("AGENT_BRIDGE_NOTIFY_DELAY").unwrap_or_default();
    let secs = if raw.is_empty() {
        0.3
    } else {
        match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            _ => return,
        }
    };
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
}

/// notify_or_defer:371 — send／respond_task／cancel 三個通知呼叫點共用的 gate。
///
/// state/<agent>.json 的語意是「建議非權威」：讀不到、解析失敗、或 ts 已超過
/// `AGENT_BRIDGE_STATE_TTL` 秒都視為「未知」，直接落回 notify_pane 路徑。
/// 只有明確讀到 state=busy 且新鮮，才完全不送鍵。
pub fn notify_or_defer(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    agent: &str,
    pane: &str,
    cmdline: &str,
    task_id: &str,
    tag: &str,
) -> Result<()> {
    let ttl = resolve_state_ttl()?;
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
        eprintln!(
            "agent-bridge: 提示：{agent} 目前忙碌中，通知延後——訊息已在 mailbox，對方 turn 結束時會由 hook 自行取件"
        );
        return Ok(());
    }

    if notify_pane(tmux, pane, cmdline) {
        log_event(
            paths,
            task_id,
            "notified",
            &format!("pane={pane} cmd={tag}"),
        )?;
    } else {
        log_event(
            paths,
            task_id,
            "notify-failed",
            &format!("pane={pane} cmd={tag}"),
        )?;
        eprintln!(
            "agent-bridge: 警告：無法通知 {agent}（pane {pane}）；請手動在對方 session 執行：{cmdline}"
        );
    }
    Ok(())
}

/// ENV-TTL-1/2：`AGENT_BRIDGE_STATE_TTL` 只認非負整數（至多 9 位），其餘視為
/// 設定錯誤直接 die——**通知端的壞值 MUST 致命**，不得靜默退回預設。
fn resolve_state_ttl() -> Result<i64> {
    let raw = std::env::var("AGENT_BRIDGE_STATE_TTL").unwrap_or_default();
    if raw.is_empty() {
        return Ok(1800);
    }
    let ok = raw.len() <= 9 && raw.bytes().all(|b| b.is_ascii_digit());
    if !ok {
        return Err(Error::new(format!(
            "AGENT_BRIDGE_STATE_TTL 需為非負整數：{raw}"
        )));
    }
    // 前導零比照 bash `10#$ttl` 強制十進位
    raw.parse::<i64>()
        .map_err(|_| Error::new(format!("AGENT_BRIDGE_STATE_TTL 需為非負整數：{raw}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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

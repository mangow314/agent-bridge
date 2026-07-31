//! TUI 的動作層（tui-design.md §2／§3）：每個動作等於一條 `agent-bridge`
//! 命令。tmux 一律走 `ab-core` 的 `TmuxClient`（bounded），不直接 spawn。

use ab_core::error::{Error, Result};
use ab_core::tmux::TmuxClient;

use crate::model::{FocusPlan, LiveIndex, focus_plan};

/// `x` 的等價 CLI 原文（確認框 MUST 逐字顯示，§2 薄殼原則）。
pub fn cancel_cmdline(id: &str) -> String {
    format!("agent-bridge cancel {id}")
}

// `x` cancel 的實作正本是 `ab_core::task::cancel_task`（CLI 的 `cmd_cancel`
// 消費同一份）：TUI 這邊刻意**不留第二份拷貝**——鎖／轉態／事件／通知一旦
// 分家就會各自漂移（審查 F6）。呈現差異（footer vs stderr）由兩邊的外殼各
// 自處理，core 函式本身不印字（審查 F7）。

/// `Enter` focus（§2）。位置以**當下重查**的 liveness 為準（不是上一輪 2s
/// 快照——selection 到執行之間 pane 可能已搬家）；查不到＝降級成錯誤訊息，
/// 不猜、不凍結。
pub fn focus(tmux: &dyn TmuxClient, pane: &str, current_session: Option<&str>) -> Result<()> {
    if pane.is_empty() {
        return Err(Error::new(
            "此列沒有 pane id 可 focus（registry 缺 pane_id）",
        ));
    }
    let live = LiveIndex::query(tmux);
    let Some(plan) = focus_plan(
        live.panes.as_ref().and_then(|m| m.get(pane)),
        current_session,
    ) else {
        return Err(Error::new(format!(
            "找不到 pane {pane} 的位置（已死或 tmux 查詢失敗）"
        )));
    };
    exec_focus(tmux, &plan, pane)
}

/// 計畫的執行段（與計算分開，計算可單測）。每步失敗都回報，不靜默。
fn exec_focus(tmux: &dyn TmuxClient, plan: &FocusPlan, pane: &str) -> Result<()> {
    if let Some(sess) = &plan.switch_to {
        let ok = tmux
            .exec(&["switch-client", "-t", sess])
            .map(|o| o.status_ok)
            .unwrap_or(false);
        if !ok {
            return Err(Error::new(format!(
                "switch-client 失敗（目標 session：{sess}）"
            )));
        }
    }
    for args in [
        ["select-window", "-t", plan.window.as_str()],
        ["select-pane", "-t", pane],
    ] {
        let ok = tmux.exec(&args).map(|o| o.status_ok).unwrap_or(false);
        if !ok {
            return Err(Error::new(format!("{} 失敗（目標：{}）", args[0], args[2])));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ab_core::tmux::TmuxOutput;
    use std::sync::Mutex;

    /// 假 tmux：記錄呼叫、回放腳本。`None`＝模擬逾時／起不來（bounded 的
    /// 降級終態）。
    struct FakeTmux {
        calls: Mutex<Vec<Vec<String>>>,
        fail_all: bool,
    }

    impl FakeTmux {
        fn new(fail_all: bool) -> Self {
            FakeTmux {
                calls: Mutex::new(Vec::new()),
                fail_all,
            }
        }
    }

    impl TmuxClient for FakeTmux {
        fn exec(&self, args: &[&str]) -> Option<TmuxOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            if self.fail_all {
                return None;
            }
            Some(TmuxOutput {
                status_ok: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn available(&self) -> bool {
            true
        }
        fn resolve_pane(&self, _target: &str) -> Option<String> {
            None
        }
        fn pane_exists(&self, _pane: &str) -> bool {
            false
        }
        fn capture_pane(&self, _pane: &str) -> Option<String> {
            None
        }
        fn pane_in_mode(&self, _pane: &str) -> Option<bool> {
            None
        }
        fn send_keys(&self, _pane: &str, _keys: &str) -> bool {
            false
        }
    }

    /// 同 session：不得 switch-client，恰好 select-window＋select-pane。
    #[test]
    fn exec_focus_same_session_selects_without_switch() {
        let tmux = FakeTmux::new(false);
        let plan = FocusPlan {
            switch_to: None,
            window: "@3".to_string(),
        };
        exec_focus(&tmux, &plan, "%9").unwrap();
        let calls = tmux.calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![
                vec!["select-window".to_string(), "-t".into(), "@3".into()],
                vec!["select-pane".to_string(), "-t".into(), "%9".into()],
            ]
        );
    }

    /// 跨 session：先 switch-client 再 select（§2 語意順序）。
    #[test]
    fn exec_focus_cross_session_switches_first() {
        let tmux = FakeTmux::new(false);
        let plan = FocusPlan {
            switch_to: Some("other".to_string()),
            window: "@7".to_string(),
        };
        exec_focus(&tmux, &plan, "%2").unwrap();
        let calls = tmux.calls.lock().unwrap();
        assert_eq!(calls[0], vec!["switch-client", "-t", "other"]);
        assert_eq!(calls[1][0], "select-window");
        assert_eq!(calls[2][0], "select-pane");
    }

    /// tmux 整層失效（bounded 逾時→None）：focus 降級成 Err，不凍結不猜。
    #[test]
    fn focus_degrades_to_error_when_tmux_unavailable() {
        let tmux = FakeTmux::new(true);
        let err = focus(&tmux, "%1", Some("it")).unwrap_err();
        assert!(err.message.contains("%1"), "實際：{}", err.message);
    }

    /// 確認框顯示的等價 CLI 原文（§2 薄殼原則：TUI 動作＝CLI 命令）。
    #[test]
    fn cancel_cmdline_is_verbatim() {
        assert_eq!(
            cancel_cmdline("20260731T000001Z-aaaa"),
            "agent-bridge cancel 20260731T000001Z-aaaa"
        );
    }
}

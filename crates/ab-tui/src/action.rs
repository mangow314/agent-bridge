//! TUI 的動作層（tui-design.md §2／§3）：每個動作等於一條 `agent-bridge`
//! 命令。tmux 一律走 `ab-core` 的 `TmuxClient`（bounded），不直接 spawn。

use ab_core::error::{Error, Result};
use ab_core::tmux::TmuxClient;

use crate::app::Sel;
use crate::model::{FocusPlan, LiveIndex, Liveness, Model, focus_plan, pane_liveness};

/// `x` 的等價 CLI 原文（確認框 MUST 逐字顯示，§2 薄殼原則）。
pub fn cancel_cmdline(id: &str) -> String {
    format!("agent-bridge cancel {id}")
}

/// 選中項的**證據**行（§3 `c` 的白名單＋DETAIL 的 evidence 區共用同一份
/// 組裝——兩邊各寫一份就會有一邊悄悄長出 mutation 命令）。
///
/// 白名單只有四類：`task-id:`／`pane:`／`agent-bridge read`／
/// `agent-bridge status`（worker 列則是 `pane:`＋`agent-bridge list --long`）。
/// **MUST NOT** 出現任何 mutation 子指令（cancel／evict／despawn／send／
/// spawn／relay／unregister／register／kill／gc）：複製出去的東西會被人貼到
/// 別處直接執行，唯讀是這條路徑唯一的安全論證（§5）。
pub fn evidence(sel: &Sel) -> Vec<String> {
    let mut out = Vec::new();
    match sel {
        Sel::Task { task, worker } => {
            out.push(format!("task-id: {}", task.id));
            if let Some(w) = worker
                && !w.pane.is_empty()
            {
                out.push(format!("pane: {}", w.pane));
            }
            out.push(format!("agent-bridge read {}", task.id));
            out.push(format!("agent-bridge status {}", task.id));
        }
        Sel::Worker(w) => {
            out.push(format!(
                "pane: {}",
                if w.pane.is_empty() { "-" } else { &w.pane }
            ));
            out.push("agent-bridge list --long".to_string());
        }
        Sel::Owner(_) | Sel::None => {}
    }
    out
}

/// `c` 的 payload（純函式，不經 render 可測，§3）。
///
/// 第一行永遠以 `task-id:` 或 `pane:` 起首，不會以 `-` 開頭——`set-buffer`
/// 的位置參數若以 `-` 起首會被 tmux 當旗標解讀，這個前提是它安全的理由。
pub fn copy_payload(sel: &Sel) -> String {
    let lines = evidence(sel);
    if lines.is_empty() {
        return String::new();
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// 複製後端（§3 定案：tmux buffer，不引入 clipboard crate、不依賴 OSC52）。
/// trait 化只為了測試注入假件——正式實作只有 `TmuxClipboard` 一種。
pub trait Clipboard {
    fn set(&self, payload: &str) -> Result<()>;
}

pub struct TmuxClipboard<'t>(pub &'t dyn TmuxClient);

impl Clipboard for TmuxClipboard<'_> {
    fn set(&self, payload: &str) -> Result<()> {
        let ok = self
            .0
            .exec(&["set-buffer", payload])
            .map(|o| o.status_ok)
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(Error::new(
                "複製失敗：tmux set-buffer 不可用（tmux 不在或逾時）",
            ))
        }
    }
}

/// `c` 的執行段。tmux 不可用＝錯誤訊息，MUST NOT 凍結（呼叫端在背景 worker
/// 上執行這一段）。
pub fn copy(clip: &dyn Clipboard, payload: &str) -> Result<()> {
    clip.set(payload)
}

// `x` cancel 的實作正本是 `ab_core::task::cancel_task`（CLI 的 `cmd_cancel`
// 消費同一份）：TUI 這邊刻意**不留第二份拷貝**——鎖／轉態／事件／通知一旦
// 分家就會各自漂移（審查 F6）。呈現差異（footer vs stderr）由兩邊的外殼各
// 自處理，core 函式本身不印字（審查 F7）。

/// `i` 的 worker 摘要頁（§3：v1 以 status＋registry 摘要頁替代 `explain`，
/// 不新增協定子指令）。
///
/// 資料來源只有**已載入的 read model ＋已有的 liveness 快照**：按鍵路徑上
/// 不開任何檔、不查 tmux。`spawn_tag`／`registered_at` 隨 `AgentSnapshot` 在
/// `registry::snapshot` 的**同一次 parse** 取得——按鍵當下另外重讀會把不同
/// 世代的欄位拼成同一頁（registry 是 atomic replace，跨廠審查 major #3）。
/// liveness 維持三態，`unknown` MUST NOT 寫成 dead（§5 顯示紀律）。
pub fn info_page(model: &Model, live: &LiveIndex, name: &str) -> Vec<String> {
    let Some(w) = model.workers.iter().find(|w| w.name == name) else {
        return vec![format!("registry 已無 '{name}'")];
    };
    let dash = |s: &str| {
        if s.is_empty() {
            "-".to_string()
        } else {
            s.to_string()
        }
    };
    let mut lines = vec![
        format!("name         : {}", w.name),
        format!("pane         : {}", dash(&w.pane)),
        format!("runtime      : {}", dash(&w.runtime)),
        format!("owner        : {}", dash(&w.owner)),
        format!("ready        : {}", dash(&w.ready)),
        format!("spawn_tag    : {}", dash(&w.spawn_tag)),
        format!("registered_at: {}", dash(&w.registered_at)),
        format!(
            "liveness     : {}",
            match pane_liveness(live, &w.pane) {
                Liveness::Live => "live",
                Liveness::Dead => "dead",
                Liveness::Unknown => "unknown",
            }
        ),
        String::new(),
        "in-flight tasks:".to_string(),
    ];
    let mut any = false;
    for t in model.tasks.iter().filter(|t| t.to == w.name) {
        // status 一律權威字，不縮寫不造詞
        lines.push(format!("  {}  {}", t.id, t.status));
        any = true;
    }
    if !any {
        lines.push("  （無）".to_string());
    }
    lines.push(String::new());
    lines.push("evidence:".to_string());
    for e in evidence(&Sel::Worker(w)) {
        lines.push(format!("  {e}"));
    }
    lines.push(String::new());
    lines.push("按任意鍵關閉".to_string());
    lines
}

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

    /// mutation 子指令黑名單（§3／§5：`c` MUST NOT 複製任何會改狀態的命令）。
    const MUTATIONS: [&str; 10] = [
        "cancel",
        "evict",
        "despawn",
        "send",
        "spawn",
        "relay",
        "unregister",
        "kill",
        "gc",
        "register",
    ];

    fn snap(name: &str, pane: &str) -> ab_core::registry::AgentSnapshot {
        ab_core::registry::AgentSnapshot {
            name: name.to_string(),
            pane: pane.to_string(),
            runtime: "codex".to_string(),
            owner: "it:@1".to_string(),
            ready: "ready".to_string(),
            spawn_tag: "t-gen1".to_string(),
            registered_at: "2026-07-31T00:00:00Z".to_string(),
            spawned: true,
            corrupt: false,
        }
    }

    fn task(id: &str) -> ab_core::task::InFlight {
        ab_core::task::InFlight {
            id: id.to_string(),
            from: "alice".to_string(),
            to: "w1".to_string(),
            status: "completed".to_string(),
        }
    }

    /// tui-design §9 P2 gate (a) 的 action 層正本：每一種選中項組出的 payload
    /// 都 MUST NOT 含 mutation 子指令，且 MUST 含預期的唯讀原文。
    #[test]
    fn copy_payload_is_read_only_evidence_for_every_selection() {
        let w = snap("w1", "%5");
        let t = task("20260731T000009Z-dddd");

        let p = copy_payload(&Sel::Task {
            task: &t,
            worker: Some(&w),
        });
        assert!(p.contains("task-id: 20260731T000009Z-dddd"), "實際：{p}");
        assert!(p.contains("pane: %5"), "實際：{p}");
        assert!(
            p.contains("agent-bridge read 20260731T000009Z-dddd"),
            "實際：{p}"
        );
        assert!(
            p.contains("agent-bridge status 20260731T000009Z-dddd"),
            "實際：{p}"
        );

        let pw = copy_payload(&Sel::Worker(&w));
        assert!(pw.contains("pane: %5") && pw.contains("agent-bridge list --long"));

        for payload in [&p, &pw] {
            for m in MUTATIONS {
                assert!(
                    !payload.contains(m),
                    "payload MUST NOT 含 mutation 子指令 '{m}'：{payload}"
                );
            }
            // set-buffer 的位置參數若以 `-` 起首會被當旗標
            assert!(!payload.starts_with('-'), "payload 首字元不得是 -");
        }
        // owner 列／無選中項：沒有證據可複製（呼叫端據此提示無效）
        assert!(copy_payload(&Sel::Owner("it:@1")).is_empty());
        assert!(copy_payload(&Sel::None).is_empty());
    }

    /// `c` 的動作層確實把 `copy_payload` 的結果交給 clipboard（不經 render）。
    #[test]
    fn copy_hands_payload_to_clipboard() {
        struct FakeClip(Mutex<Vec<String>>);
        impl Clipboard for FakeClip {
            fn set(&self, payload: &str) -> Result<()> {
                self.0.lock().unwrap().push(payload.to_string());
                Ok(())
            }
        }
        let w = snap("w1", "%5");
        let t = task("20260731T000009Z-dddd");
        let sel = Sel::Task {
            task: &t,
            worker: Some(&w),
        };
        let clip = FakeClip(Mutex::new(Vec::new()));
        copy(&clip, &copy_payload(&sel)).unwrap();
        let got = clip.0.lock().unwrap();
        assert_eq!(*got, vec![copy_payload(&sel)]);
    }

    /// tmux 不可用（bounded 逾時→None）：複製失敗回錯誤訊息，不凍結。
    #[test]
    fn tmux_clipboard_degrades_to_error() {
        let tmux = FakeTmux::new(true);
        let err = TmuxClipboard(&tmux).set("task-id: x\n").unwrap_err();
        assert!(err.message.contains("set-buffer"), "實際：{}", err.message);
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

//! TUI 的動作層（tui-design.md §2／§3）：每個動作等於一條 `agent-bridge`
//! 命令。tmux 一律走 `ab-core` 的 `TmuxClient`（bounded），不直接 spawn。

use ab_core::error::{Error, Result};
use ab_core::evict::{self, EvictOutcome, EvictRequest};
use ab_core::paths::Paths;
use ab_core::spawn::DespawnResult;
use ab_core::tmux::TmuxClient;

use crate::app::{Pager, PeekTarget, PeekView, Sel};
use crate::model::{
    Blocker, BlockerIndex, FocusPlan, LiveIndex, Liveness, Model, focus_plan, pane_liveness,
};

/// `r` 的全螢幕 pager 攤平成的**完整內容列**（標頭＋內文＋鍵位提示）。
///
/// 為什麼組在這裡而不是 render 裡（同 `info_page`／`evict_confirm_lines` 的
/// 位置）：捲動上界要知道總列數，render 各算一份的話，兩邊遲早漂移——而漂移
/// 的症狀是「End 之後畫面上多／少一截空白」這種沒人會回報的小錯。
///
/// bytes → 字串的 lossy 轉換**只在這裡**做一次（action 層一律保留原始 bytes，
/// gate (b) 比的就是 bytes）。
pub fn pager_lines(p: &Pager) -> Vec<String> {
    let text = String::from_utf8_lossy(&p.bytes).into_owned();
    let mut lines = vec![
        format!("task-id: {}", p.id),
        format!("from: {}", p.from),
        format!("to: {}", p.to),
        "\u{2500}".repeat(8),
    ];
    lines.extend(text.lines().map(|l| l.to_string()));
    lines.push(String::new());
    // chrome 全英文（`every_chrome_surface_is_english_only` 掃得到這一行）
    lines.push(
        "j/k (\u{2193}\u{2191}) scroll \u{b7} PgUp/PgDn page \u{b7} Home/End ends \u{b7} Esc/q close"
            .to_string(),
    );
    lines
}

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
        Sel::None => {}
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
                "copy failed: tmux set-buffer unavailable (tmux missing or timed out)",
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
pub fn info_page(
    model: &Model,
    live: &LiveIndex,
    blockers: &BlockerIndex,
    name: &str,
) -> Vec<String> {
    let Some(w) = model.workers.iter().find(|w| w.name == name) else {
        return vec![format!("'{name}' is gone from the registry")];
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
        // BLOCKER 軸（§4 雙軸）：與 liveness 各自一行，三態不得壓成兩態
        format!(
            "blocker      : {}",
            match blockers.get(&w.pane) {
                Blocker::None => "none",
                Blocker::Prompt => "permission/plan prompt (blocked)",
                Blocker::Occluded => "occluded (copy-mode)",
                Blocker::Unknown => "unknown",
            }
        ),
        String::new(),
        "in-flight tasks:".to_string(),
    ];
    let mut any = false;
    // **與 WORKERS 欄同一條判準**（修正輪 R2／F3）：純名字比對是第四處手抄，
    // 它會讓 w1 於 20:00 respawn 之後，11:00 建立的舊 task 在 WORKERS 欄正確
    // 地不掛、按 `i` 卻列得出來——同一張畫面兩個答案
    for t in model.tasks.iter().filter(|t| crate::model::attached(t, w)) {
        // status 一律權威字，不縮寫不造詞
        lines.push(format!("  {}  {}", t.id, t.status));
        any = true;
    }
    if !any {
        lines.push("  (none)".to_string());
    }
    lines.push(String::new());
    lines.push("evidence:".to_string());
    for e in evidence(&Sel::Worker(w)) {
        lines.push(format!("  {e}"));
    }
    lines.push(String::new());
    lines.push("press any key to close".to_string());
    lines
}

/// `L` 的尾行預覽頁（P4.7 切片 D；純函式，不經 render 可測）。
///
/// **這裡不再截一次**：行／byte／時間三個界只有 `ab_core::config` 那一份定義，
/// 且都成立於取得路徑上（`tmux::capture_pane_tail`）。這裡只做呈現層的事——
/// 去掉 capture-pane 在畫面下緣補出來的空行（那是 pane 高度的產物，不是內容），
/// 並在截斷過時加一行標記。
pub fn peek_page(target: &PeekTarget, cap: &ab_core::tmux::TailCapture) -> PeekView {
    let mut lines: Vec<String> = cap.text.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push("(the pane has produced no output)".to_string());
    }
    PeekView {
        title: format!("tail preview — {} ({})", target.name, target.pane),
        lines,
        truncated: cap.truncated,
    }
}

/// `e` 證據框上顯示過的世代識別（tui-design §5 compare-and-act 的「compare」
/// 那一半）。人是看著這組值按下 y 的，執行時的值 MUST 與它一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvictShown {
    pub name: String,
    pub pane: String,
    pub spawn_tag: String,
}

/// `e` 的證據框（§5 顯示紀律）。
#[derive(Debug)]
pub struct EvictPrompt {
    pub shown: EvictShown,
    /// 框內逐行內容（已含等價 CLI 原文，§2 薄殼原則）
    pub lines: Vec<String>,
}

/// 按 `e` 當下：讀 registry 做出身判定並取當下 pane／世代，組出證據框。
///
/// 出身規則**不在 TUI 重寫**：判定與訊息都來自 `ab_core::evict::precheck_registry`
/// （CLI 的 evict 走同一份）。非 spawn 出身／registry 無法解析／缺 spawn_tag
/// 一律在這裡就被拒，錯誤訊息原樣進 footer。
pub fn evict_prompt(paths: &Paths, name: &str) -> Result<EvictPrompt> {
    let f = paths.agents_dir.join(format!("{name}.json"));
    let (pane, spawn_tag) = evict::precheck_registry(&f, name)?;
    let shown = EvictShown {
        name: name.to_string(),
        pane,
        spawn_tag,
    };
    let lines = evict_confirm_lines(&evict_request_from(&shown));
    Ok(EvictPrompt { shown, lines })
}

/// 確認（y）當下：**重讀 registry**，以當下值組出 evict 參數。
///
/// 重讀是本條款的重點（tui-design §5）：沿用 500ms 前那份輪詢快照，等於把
/// selection 到確認之間的換代一路帶進 mutation。重讀到的值與證據框上顯示的
/// 不同時 MUST 中止——人是看著框上那組值按下 y 的，悄悄換一個目標去執行比
/// 拒絕更糟；此時尚未產生任何副作用。
///
/// 相符時 expect 參數帶的是**重讀值**（與框上相等，但語意上執行的永遠是當下
/// 讀到的那一代）；core 會在 registry 鎖內再比對一次，堵住這裡到鎖之間的
/// 第二個窗口（CLI-EVICT-4）。
pub fn evict_request(paths: &Paths, shown: &EvictShown) -> Result<EvictRequest> {
    let f = paths.agents_dir.join(format!("{}.json", shown.name));
    // 確認期**任何**取不到「與框上同一個 identity」的情況都是 selection stale
    // ——registry 被刪、被改壞、變成人工註冊、spawn_tag 消失，都代表框上那一代
    // 已經不在了。原始 precheck 錯誤（「未註冊的 agent」「非 spawn 出身」）在
    // 這一刻會誤導成「這個 worker 本來就不能 evict」，而真相是「你看到的那個
    // 已經不存在」（codex 複核 major #1）。原因保留在括號裡，不吞。
    // 初次開框（`evict_prompt`）不套用這層：那時原始理由才是對的。
    let (pane, spawn_tag) = evict::precheck_registry(&f, &shown.name).map_err(|e| {
        Error::new(format!(
            "evict 中止（selection stale）：確認當下重讀 registry，已取不到 agent '{}' 框上那一代（{}）；未建立任何任務、未通知、未回收",
            shown.name, e.message
        ))
    })?;
    if pane != shown.pane || spawn_tag != shown.spawn_tag {
        return Err(Error::new(format!(
            "evict 中止（selection stale）：確認當下重讀 registry，agent '{}' 現為 pane {pane}／世代 {spawn_tag}，與證據框顯示的 pane {}／世代 {} 不同；未建立任何任務、未通知、未回收",
            shown.name, shown.pane, shown.spawn_tag
        )));
    }
    Ok(evict_request_from(&EvictShown {
        name: shown.name.clone(),
        pane,
        spawn_tag,
    }))
}

/// `(name, pane, tag)` → evict 參數。TUI 一律帶滿兩個 expect：dashboard 的
/// selection 天生落後於磁碟，不帶 expect 就是把 TOCTOU 留在原地（§5）。
fn evict_request_from(shown: &EvictShown) -> EvictRequest {
    let mut req = EvictRequest::new(&shown.name);
    req.expect_pane = Some(shown.pane.clone());
    req.expect_generation = Some(shown.spawn_tag.clone());
    req
}

/// 證據框的內容（純函式，不經 render 可測）。
///
/// §5 顯示紀律：措辭 MUST 是「派收尾任務後回收」（P4.6 題 9 英文化之後即
/// `wrap-up task, then reclaim`），MUST NOT 出現任何「安全刪除」語彙
/// （`safe to delete`／`safe to remove`…）——這一框的職責是把證據攤開讓人
/// 自己判斷，不是替人下判斷。框內 MUST 逐字帶上等價 CLI 原文（§2 薄殼原則）。
pub fn evict_confirm_lines(req: &EvictRequest) -> Vec<String> {
    vec![
        format!("worker '{}': wrap-up task, then reclaim", req.name),
        "  first dispatch one wrap-up task so it can write down the facts".to_string(),
        "  that exist only in its context; the pane is reclaimed only after".to_string(),
        "  those notes land (or time out). Notes stay in tasks/ to re-read.".to_string(),
        format!("pane      : {}", req.expect_pane.as_deref().unwrap_or("-")),
        format!(
            "generation: {}",
            req.expect_generation.as_deref().unwrap_or("-")
        ),
        "Confirm to run the equivalent CLI:".to_string(),
        format!("$ {}", req.cmdline()),
        "[y/Enter] run \u{b7} [n/Esc] abort".to_string(),
    ]
}

/// evict 終局 → footer 一句話（呈現層與 core 分離，審查 F7）。
pub fn evict_message(name: &str, res: &Result<EvictOutcome>) -> String {
    match res {
        Ok(o) if o.despawn == DespawnResult::Stale => format!(
            "registration of agent '{name}' was cleared, but pane {} was NOT reclaimed; judge the wrap-up task {} ({}) yourself",
            o.pane, o.task_id, o.final_status
        ),
        Ok(o) => match o.audit {
            "evicted" => format!(
                "evicted '{name}'; wrap-up notes are available: agent-bridge read {}",
                o.task_id
            ),
            "evicted-unfinished" => format!(
                "reclaimed '{name}', but the wrap-up task {} ended as {}; notes did not land",
                o.task_id, o.final_status
            ),
            _ => format!(
                "reclaimed '{name}', but the wrap-up task {} timed out; notes did not land",
                o.task_id
            ),
        },
        Err(e) => e.message.clone(),
    }
}

/// `Enter` focus（§2）。位置以**當下重查**的 liveness 為準（不是上一輪 2s
/// 快照——selection 到執行之間 pane 可能已搬家）；查不到＝降級成錯誤訊息，
/// 不猜、不凍結。
pub fn focus(tmux: &dyn TmuxClient, pane: &str, current_session: Option<&str>) -> Result<()> {
    if pane.is_empty() {
        return Err(Error::new(
            "this row has no pane id to focus (registry has no pane_id)",
        ));
    }
    let live = LiveIndex::query(tmux);
    let Some(plan) = focus_plan(
        live.panes.as_ref().and_then(|m| m.get(pane)),
        current_session,
    ) else {
        return Err(Error::new(format!(
            "cannot locate pane {pane} (it is gone, or the tmux query failed)"
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
                "switch-client failed (target session: {sess})"
            )));
        }
    }
    for args in [
        ["select-window", "-t", plan.window.as_str()],
        ["select-pane", "-t", pane],
    ] {
        let ok = tmux.exec(&args).map(|o| o.status_ok).unwrap_or(false);
        if !ok {
            return Err(Error::new(format!(
                "{} failed (target: {})",
                args[0], args[2]
            )));
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
            // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
            lineage_root: None,
            parent_agent: None,
        }
    }

    fn task(id: &str) -> ab_core::task::InFlight {
        ab_core::task::InFlight {
            created_at: "2026-08-01T00:00:00Z".to_string(),
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
        // 無選中項：沒有證據可複製（呼叫端據此提示無效）
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

    /// 臨時資料目錄（registry 檔要真的落在磁碟上，才驗得到「重讀」）。
    struct TmpPaths {
        paths: Paths,
        root: std::path::PathBuf,
    }

    impl TmpPaths {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "ab-tui-evict-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("agents")).unwrap();
            let mut paths = Paths::resolve();
            paths.data_dir = root.clone();
            paths.agents_dir = root.join("agents");
            paths.tasks_dir = root.join("tasks");
            paths.state_dir = root.join("state");
            paths.locks_dir = root.join("locks");
            TmpPaths { paths, root }
        }
        fn write_agent(&self, name: &str, pane: &str, tag: &str) {
            std::fs::write(
                self.paths.agents_dir.join(format!("{name}.json")),
                format!(
                    r#"{{"name":"{name}","pane_id":"{pane}","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"{tag}"}}"#
                ),
            )
            .unwrap();
        }
    }

    impl Drop for TmpPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// §5 顯示紀律：證據框措辭 MUST 是「派收尾任務後回收」，MUST NOT 出現
    /// 任何「安全刪除」語彙；且 MUST 逐字帶上等價 CLI 原文（§2 薄殼原則）。
    #[test]
    fn evict_confirm_lines_state_wrap_up_never_safe_to_delete() {
        let t = TmpPaths::new("prompt");
        t.write_agent("w1", "%5", "t-gen1");
        let p = evict_prompt(&t.paths, "w1").unwrap();
        let text = p.lines.join("\n");

        assert!(text.contains("wrap-up task, then reclaim"), "實際：{text}");
        // 措辭紅線（§5）：中英兩套「安全刪除」語彙都不得出現——英文化之後只擋
        // 中文等於把這條紀律翻掉了
        for banned in [
            "安全刪除",
            "可安全",
            "安全回收",
            "可刪",
            "無殘值",
            "safe to delete",
            "safe to remove",
            "safe to reclaim",
            "no longer needed",
            "disposable",
        ] {
            assert!(!text.contains(banned), "MUST NOT 出現 '{banned}'：{text}");
        }
        assert!(
            text.contains("$ agent-bridge evict w1 --expect-pane %5 --expect-generation t-gen1"),
            "MUST 逐字顯示等價 CLI：{text}"
        );
        assert_eq!(p.shown.pane, "%5");
        assert_eq!(p.shown.spawn_tag, "t-gen1");
    }

    /// 出身非 spawn（人工註冊）／缺 spawn_tag：`e` MUST 在證據框之前就被拒，
    /// 且理由沿用 core 的判定（TUI 不自己重寫規則）。
    #[test]
    fn evict_prompt_rejects_non_spawned_origin() {
        let t = TmpPaths::new("origin");
        std::fs::write(
            t.paths.agents_dir.join("manual.json"),
            r#"{"name":"manual","pane_id":"%9"}"#,
        )
        .unwrap();
        let e = evict_prompt(&t.paths, "manual").unwrap_err();
        assert!(e.message.contains("非 spawn 出身"), "實際：{}", e.message);

        std::fs::write(
            t.paths.agents_dir.join("notag.json"),
            r#"{"name":"notag","pane_id":"%9","spawned":true}"#,
        )
        .unwrap();
        let e = evict_prompt(&t.paths, "notag").unwrap_err();
        assert!(e.message.contains("spawn_tag"), "實際：{}", e.message);
    }

    /// **本組的不變量測試**（tui-design §5）：確認當下 MUST 重讀 registry。
    ///
    /// 證據框顯示的是「按 e 當下」讀到的值；確認之間 registry 換了代，重讀就
    /// 會看見新值 → 與框上不符 → selection stale 中止。把重讀改成沿用框上的
    /// 快照值，這個測試就會轉綠地放行（mutant 存活），故它同時錨住兩件事：
    /// 有沒有重讀、以及不符時有沒有中止。
    #[test]
    fn evict_request_rereads_registry_at_confirm_time() {
        let t = TmpPaths::new("reread");
        t.write_agent("w1", "%5", "t-gen1");
        let shown = evict_prompt(&t.paths, "w1").unwrap().shown;

        // 沒換代：重讀值 == 框上值 → 帶滿兩個 expect
        let req = evict_request(&t.paths, &shown).unwrap();
        assert_eq!(req.expect_pane.as_deref(), Some("%5"));
        assert_eq!(req.expect_generation.as_deref(), Some("t-gen1"));
        assert_eq!(req.name, "w1");

        // 確認之前換代（respawn）：重讀 MUST 看見新值 → selection stale
        t.write_agent("w1", "%77", "t-gen2");
        let e = evict_request(&t.paths, &shown).unwrap_err();
        assert!(
            e.message.contains("selection stale"),
            "MUST 中止並點名 selection stale：{}",
            e.message
        );
        assert!(e.message.contains("%77"), "MUST 說出當下值：{}", e.message);
        assert!(
            e.message.contains("未建立任何任務"),
            "MUST 說清楚沒有副作用：{}",
            e.message
        );

        // registry 整個消失（worker 已被別人回收）／被改壞／變成人工註冊：
        // 確認期一律是 **selection stale**，不得回「未註冊」「非 spawn 出身」
        // 這種會被讀成「本來就不能 evict」的原始理由（codex 複核 major #1）
        let f = t.paths.agents_dir.join("w1.json");
        for (case, body) in [
            ("registry 消失", None),
            ("registry 損壞", Some("not json")),
            ("改成人工註冊", Some(r#"{"name":"w1","pane_id":"%5"}"#)),
            (
                "spawn_tag 消失",
                Some(r#"{"name":"w1","pane_id":"%5","spawned":true}"#),
            ),
        ] {
            match body {
                None => {
                    let _ = std::fs::remove_file(&f);
                }
                Some(b) => std::fs::write(&f, b).unwrap(),
            }
            let e = evict_request(&t.paths, &shown).unwrap_err();
            assert!(
                e.message.contains("selection stale"),
                "{case} MUST 映射成 selection stale：{}",
                e.message
            );
            assert!(
                e.message.contains("未建立任何任務"),
                "{case} MUST 說清楚沒有副作用：{}",
                e.message
            );
        }
    }

    /// 初次開框（`e`）**不**套用 stale 映射：那時原始理由（非 spawn 出身／
    /// 缺 spawn_tag）才是對的，包成 stale 反而讓人以為只要重按一次就好。
    #[test]
    fn evict_prompt_keeps_original_precheck_reason() {
        let t = TmpPaths::new("promptreason");
        std::fs::write(
            t.paths.agents_dir.join("manual.json"),
            r#"{"name":"manual","pane_id":"%9"}"#,
        )
        .unwrap();
        let e = evict_prompt(&t.paths, "manual").unwrap_err();
        assert!(e.message.contains("非 spawn 出身"), "實際：{}", e.message);
        assert!(
            !e.message.contains("selection stale"),
            "開框期 MUST NOT 說成 stale：{}",
            e.message
        );
    }

    /// evict 終局 → footer 一句話：三種審計分流各自可辨識，stale 不得說成
    /// 「已回收」（§5：不得替人下判斷，也不得謊報發生過的事）。
    #[test]
    fn evict_message_distinguishes_every_outcome() {
        let out = |audit: &'static str, despawn: DespawnResult| EvictOutcome {
            task_id: "20260731T000009Z-dddd".into(),
            final_status: "failed".into(),
            audit,
            despawn,
            pane: "%5".into(),
        };
        let m = evict_message("w1", &Ok(out("evicted", DespawnResult::Killed)));
        assert!(m.contains("evicted 'w1'") && m.contains("agent-bridge read"));
        let m = evict_message("w1", &Ok(out("evicted-unfinished", DespawnResult::Killed)));
        assert!(
            m.contains("notes did not land") && m.contains("failed"),
            "實際：{m}"
        );
        let m = evict_message("w1", &Ok(out("evicted-timeout", DespawnResult::Killed)));
        assert!(
            m.contains("timed out") && m.contains("notes did not land"),
            "實際：{m}"
        );
        // stale：pane 未被回收，訊息不得宣稱回收成功
        let m = evict_message("w1", &Ok(out("evicted", DespawnResult::Stale)));
        assert!(m.contains("NOT reclaimed"), "實際：{m}");
        assert!(
            !m.contains("evicted 'w1'"),
            "stale MUST NOT 說成 evicted：{m}"
        );
        // 錯誤（含 selection stale）原樣進 footer
        let m = evict_message("w1", &Err(Error::new("evict 中止（selection stale）：…")));
        assert!(m.contains("selection stale"), "實際：{m}");
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

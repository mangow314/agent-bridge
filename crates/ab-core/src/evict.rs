//! evict 的三段式編排（send 收尾任務 → await → despawn），CLI 與 TUI 的
//! **單一正本**（審查 F6：編排只活在 CLI 層時，TUI 只能自己再抄一份，兩邊
//! 必然漂移）。
//!
//! **函式內不印任何字**（審查 F7）：進度與警告經 `EvictEvent` 交回呼叫端
//! ——CLI 印 stderr（`Warn` 走 `agent-bridge: ` 前綴、`Info` 走裸行），TUI 進
//! footer（alternate screen 下 stderr 會畫花畫面）。事件是**串流**而不是最後
//! 一次回傳：「收尾任務已派出、等待中」那行必須在 await 開始**之前**就看得到。

use std::path::Path;

use crate::error::{Error, Result};
use crate::lock::acquire_lock;
use crate::paths::Paths;
use crate::registry;
use crate::send;
use crate::spawn::{self, DespawnCtx, DespawnResult};
use crate::task::{self, AwaitOutcome, MessageSource};
use crate::tmux::TmuxClient;
use crate::validate::is_valid_name;

/// evict 的收尾任務文案。**硬編在這裡而不是抽到 share/**：它是機制的一部分
/// （「把只存在於你 context 裡的事實寫下來」），不是可調策略。抽成檔案會多一條
/// 「檔案不存在怎麼辦」的失敗路徑，而那條路徑一旦失敗，等於整個筆記機制悄悄消失。
pub const EVICT_MSG: &str = r#"[Wrap-up task — your final round before this pane is reclaimed]

This pane is about to be reclaimed and your context vanishes with it.
Use reply to hand back a note with the key facts that exist only in your
context and never made it into earlier responses.

Write:
- facts you found but never put into a reply (file:line, commands, measured numbers)
- dead ends you walked and why they failed (so the next runner skips them)
- open questions, the assumptions you held, and which conclusions were
  actually conjecture rather than verified

Do not write:
- restatements of what is already in your responses
- new work: this round is consolidation only — start no new investigation

When done, run agent-bridge reply <task-id> --message-file <path> (or --message).
Even with nothing worth keeping, reply anyway with the single line
"no residual value" — a missing reply is recorded as notes-never-landed."#;

/// `--timeout` 預設（秒）。
pub const DEFAULT_TIMEOUT: u64 = 300;
/// `--from` 預設。
pub const DEFAULT_FROM: &str = "orchestrator";

/// 一次 evict 呼叫的完整參數（＝一條 `agent-bridge evict …` 命令）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvictRequest {
    pub name: String,
    pub from: String,
    pub timeout: u64,
    /// CLI-EVICT-4：只驗帶到的那一項；兩項都不帶＝行為與現行完全相同
    pub expect_pane: Option<String>,
    pub expect_generation: Option<String>,
}

impl EvictRequest {
    pub fn new(name: &str) -> Self {
        EvictRequest {
            name: name.to_string(),
            from: DEFAULT_FROM.to_string(),
            timeout: DEFAULT_TIMEOUT,
            expect_pane: None,
            expect_generation: None,
        }
    }

    /// 等價 CLI 原文（薄殼原則，tui-design §2：TUI 的確認框 MUST 逐字顯示
    /// 它將要執行的那條命令）。只印**與預設不同**的旗標，人貼上去跑得到同一
    /// 件事。
    ///
    /// 動態值一律經 `shell_quote`（bash `printf %q` 語意）：`pane_id` 與
    /// `spawn_tag` 讀自 registry，而 registry 是 **worker 可寫面**——含空白就
    /// 破壞 argv 等價（貼上去是兩個參數），含 `;`／`$(…)` 更是把一句「可貼上
    /// 重跑」的承諾變成注入通道（codex 複核 major #4）。正常值全落在 `%q` 的
    /// 安全字元集內（`%5`／`AGENT_BRIDGE_SPAWN_TAG=…` 逐字不變），所以既有
    /// 畫面斷言不受影響。
    pub fn cmdline(&self) -> String {
        let q = |s: &str| crate::spawn::shell_quote(s.as_bytes());
        let mut s = format!("agent-bridge evict {}", q(&self.name));
        if self.from != DEFAULT_FROM {
            s.push_str(&format!(" --from {}", q(&self.from)));
        }
        if self.timeout != DEFAULT_TIMEOUT {
            // u64，沒有引用的必要（也沒有引用的餘地）
            s.push_str(&format!(" --timeout {}", self.timeout));
        }
        if let Some(p) = &self.expect_pane {
            s.push_str(&format!(" --expect-pane {}", q(p)));
        }
        if let Some(g) = &self.expect_generation {
            s.push_str(&format!(" --expect-generation {}", q(g)));
        }
        s
    }
}

/// 編排過程中要給人看的訊息。
///
/// 兩類的差別只在 CLI 的呈現慣例（`Warn` 加 `agent-bridge: ` 前綴、`Info` 不
/// 加），逐字對齊現行 `cmd_evict`；TUI 兩者都進 footer。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvictEvent {
    Warn(String),
    Info(String),
}

/// evict 一次執行的終態。拿到它代表收尾任務已建立（`task_id` 恆非空）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvictOutcome {
    pub task_id: String,
    /// await 的終態字；逾時為空字串
    pub final_status: String,
    /// 審計事件名：`evicted`／`evicted-unfinished`／`evicted-timeout`
    pub audit: &'static str,
    /// despawn 的終局。`Stale`＝registry 清掉了但 pane 不屬於它、未被回收
    pub despawn: DespawnResult,
    pub pane: String,
}

/// registry 側的出身／世代快檢，回傳當下的 `(pane_id, spawn_tag)`。
///
/// 純 fail-fast：避免對人工註冊的 agent 送出一個之後必定被 despawn 拒絕、
/// 沒人回收的孤兒收尾任務。權威判定仍在 despawn 的鎖內。
///
/// TUI 的 `e` 也走這一份（出身規則不得在 TUI 另寫一套），並且**在確認當下
/// 再呼叫一次**取當下值當 expect 參數（tui-design §5 compare-and-act）。
pub fn precheck_registry(f: &Path, name: &str) -> Result<(String, String)> {
    if !f.is_file() {
        return Err(Error::new(format!("未註冊的 agent：{name}")));
    }
    match registry::read_provenance(f) {
        registry::Provenance::Manual => {
            return Err(Error::new(format!(
                "agent '{name}' 非 spawn 出身，evict 拒絕（人工 pane 的生命週期不歸 bridge 管，請用 unregister）"
            )));
        }
        registry::Provenance::Undetermined => {
            return Err(Error::new(format!(
                "agent '{name}' 的 registry 無法解析，出身不明，evict 拒絕；請確認 {} 後手動處理",
                f.display()
            )));
        }
        registry::Provenance::Spawned => {}
    }

    // pane/runtime 在 despawn 前取：despawn 會刪掉 registry，之後就讀不到了
    let pane = registry::read_field(f, "pane_id", "-");
    // 記下這一代的 spawn_tag，最後 despawn 時綁定比對：收尾任務是派給「這一代」
    // 的，回收也只能收這一代。tag 空的話綁定等於沒有——正常 spawn 一定寫得出
    // tag，取不到代表 registry 被動過，這時拒絕動作
    let gen_tag = registry::read_field(f, "spawn_tag", "");
    if gen_tag.is_empty() {
        return Err(Error::new(format!(
            "agent '{name}' 的 registry 沒有 spawn_tag，無法鎖定世代，evict 拒絕；請確認 {} 後手動處理",
            f.display()
        )));
    }
    Ok((pane, gen_tag))
}

/// CLI-EVICT-4 的 compare-and-act 比對（純函式，可單測）。
///
/// 只驗**帶到的**那一項：兩項都不帶時一律通過，evict 行為與現行完全相同
/// （設計正本 §1 非目標：既有 invocation 語意零改變）。
/// 不符一律回含 `selection stale` 的錯誤——呼叫端據此非 0 退出，且此時
/// 尚未產生任何副作用。
pub fn check_expect(
    name: &str,
    actual_pane: &str,
    actual_gen: &str,
    expect_pane: Option<&str>,
    expect_gen: Option<&str>,
) -> Result<()> {
    for (label, actual, expect) in [
        ("pane", actual_pane, expect_pane),
        ("世代（spawn_tag）", actual_gen, expect_gen),
    ] {
        if let Some(want) = expect
            && want != actual
        {
            return Err(Error::new(format!(
                "evict 中止（selection stale）：agent '{name}' 的{label}實際為 {actual}（期望 {want}）；未建立任何任務、未通知、未回收"
            )));
        }
    }
    Ok(())
}

/// await 終態 → 審計事件名（CLI-EVICT-2，純函式可單測）。
///
/// 空字串＝逾時（`await_task` 的 `Timeout` 分支）。三支 MUST 分開記：全記成
/// `evicted-timeout` 會讓審計線說謊——「筆記沒落地」的原因不同。
fn audit_event(final_status: &str) -> &'static str {
    match final_status {
        "completed" => "evicted",
        // failed/cancelled 也是 await 的正常返回，不是逾時
        "failed" | "cancelled" => "evicted-unfinished",
        _ => "evicted-timeout",
    }
}

/// cmd_evict:1866 — 撞 cap 時的驅逐，但**不是直接殺**：先派一輪收尾任務，讓
/// worker 把只存在於它 context 裡的關鍵事實寫下來，落地之後才 despawn。
///
/// 三步（send → await → despawn）刻意**不包在一把鎖裡**：鎖是單值，同時持有
/// 兩把時只會放掉一把。分段的失效方向分別是「多一個沒人收的收尾 task」與
/// 「筆記已落地、pane 沒收掉（多佔一個 cap）」——都不會刪掉還沒落地的脈絡。
///
/// **逾時仍然 despawn**：否則一個不回話的 worker 會把 cap 永久卡死。代價是
/// 筆記沒落地，所以審計線一定要看得出來（evicted-timeout）。
pub fn evict(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    req: &EvictRequest,
    ev: &mut dyn FnMut(EvictEvent),
) -> Result<EvictOutcome> {
    let name = req.name.as_str();
    if !is_valid_name(name) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{name}"
        )));
    }
    if !is_valid_name(&req.from) {
        return Err(Error::new(format!(
            "sender 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{}",
            req.from
        )));
    }

    let f = paths.agents_dir.join(format!("{name}.json"));

    // CLI-EVICT-4：出身檢查、世代讀取、**expect 比對**與收尾任務的建立全部
    // 收進**同一把 registry 鎖**內。
    //
    // 為什麼一定要同一把鎖：expect 比對若在鎖外做，「驗完 → 建 task」之間仍
    // 有換代窗口，收尾任務就會派給新世代並回收它——那正是本條款要堵的第一個
    // race window（設計正本 §5）。既有的 despawn 綁定（CLI-EVICT-3）只保護
    // 「送出後 → despawn」那第二個窗口，兩者各有各的防線。
    //
    // 通知**不在**鎖內（`send::notify_send` 在鎖外）：它會送 tmux 鍵並帶延遲，
    // 圈進鎖裡會與 spawn／despawn 互撞。
    //
    // **鎖只在帶了 expect 參數時才取**：無條件取鎖會讓「不帶 expect＝行為與
    // 現行完全相同」變成假話——別的 registry 操作持鎖時，舊碼直接以「未註冊」
    // 之類的既有錯誤結束，無條件取鎖版卻會先等滿重試上限再以鎖逾時失敗，
    // 連錯誤優先序都變了（跨廠審查 P3a major #1，實測 elapsed≈4.8s）。
    let mut prepare = || -> Result<(String, String, String, String)> {
        let (pane, gen_tag) = precheck_registry(&f, name)?;
        let runtime = registry::read_field(&f, "runtime", "-");

        // MUST 在任何副作用之前：不符就直接出去，此時尚未建任何 task、
        // 未通知、未動 registry、未碰 pane
        check_expect(
            name,
            &pane,
            &gen_tag,
            req.expect_pane.as_deref(),
            req.expect_generation.as_deref(),
        )?;

        let task_id = send::create_send_task(
            paths,
            name,
            &req.from,
            &MessageSource::Text(EVICT_MSG.as_bytes().to_vec()),
            true,
            &mut |w| ev(EvictEvent::Warn(w)),
        )
        .map_err(|e| {
            // 內層錯誤先出聲再蓋上 evict 的中止訊息：bash 的 `cmd_send` 跑在命令
            // 替換的 subshell 裡，它自己的 die 早就印上 stderr 了，外層 die 是
            // 第二行（codex 複核 2026-07-31）
            ev(EvictEvent::Warn(e.message));
            Error::new(format!(
                "evict 中止：收尾任務送不出去，未動 pane（agent '{name}' 仍在）"
            ))
        })?;
        Ok((pane, runtime, gen_tag, task_id))
    };

    // 帶了 expect＝要 CAS，才需要把比對與建 task 圈在同一把鎖內；沒帶就照
    // 既有的無鎖路徑走（含它原本的錯誤優先序）
    let outcome = if req.expect_pane.is_some() || req.expect_generation.is_some() {
        let guard = acquire_lock(paths, "agents-registry")?;
        let r = prepare();
        guard.release();
        r
    } else {
        prepare()
    };
    let (pane, runtime, gen_tag, task_id) = outcome?;

    // 鎖外通知：task 已落地，通知失敗的處置與拆分前的 `do_send` 相同
    let report = send::notify_send(paths, tmux, name, &task_id).map_err(|e| {
        ev(EvictEvent::Warn(e.message));
        Error::new(format!(
            "evict 中止：收尾任務送不出去，未動 pane（agent '{name}' 仍在）"
        ))
    })?;
    if let Some(m) = report.message() {
        ev(EvictEvent::Warn(m));
    }
    ev(EvictEvent::Info(format!(
        "evict：收尾任務 {task_id} 已派給 '{name}'，等待筆記落地（timeout {}s）",
        req.timeout
    )));

    // 只有真正的逾時才走「筆記沒落地仍回收」；await 自己的操作性失敗（壞輪詢
    // 間隔、status 檔消失等）代表 worker 可能還活著、根本沒等到期限——這時
    // despawn 等於把活的 context 當逾時殺掉，審計還記成 timeout
    let final_st = match task::await_task(paths, &task_id, req.timeout) {
        Ok(AwaitOutcome::Terminal(st)) => st,
        // evict 走不帶 watch 的 await_task，Blocked 不可能出現（CLI-AWAIT-4：
        // 只有顯式帶 BlockerPolicy::Return 的 watched await 會回它）。真出現
        // 代表 await 契約被改壞——當操作性失敗中止，不得當逾時去回收 pane
        Ok(AwaitOutcome::Blocked { status, .. }) => {
            ev(EvictEvent::Warn(format!(
                "await 回報 Blocked（狀態 {status}）——evict 未帶 blocker watch，不應發生"
            )));
            return Err(Error::new(format!(
                "evict 中止：await 回報非預期的 Blocked，pane 未動（agent '{name}' 仍在）；收尾任務 {task_id} 留存可查"
            )));
        }
        Ok(AwaitOutcome::Timeout(st)) => {
            // bash 的 cmd_await 在 subshell 內先印自己的逾時行才 exit 124；
            // 那行是呼叫端追查「等到什麼狀態」的唯一線索，不能吞
            ev(EvictEvent::Warn(format!(
                "await 逾時（{}s）：task {task_id} 目前狀態 {st}",
                req.timeout
            )));
            String::new()
        }
        Err(e) => {
            ev(EvictEvent::Warn(e.message));
            return Err(Error::new(format!(
                "evict 中止：await 操作性失敗（rc=1，非逾時），pane 未動（agent '{name}' 仍在）；收尾任務 {task_id} 留存可查"
            )));
        }
    };
    let audit = audit_event(&final_st);

    let result = spawn::despawn(
        paths,
        tmux,
        name,
        &DespawnCtx {
            expect_tag: Some(gen_tag),
            notes_handled: true,
        },
    )?;

    // stale＝registry 清掉了，但那個 pane 還活著、已經不屬於這個 agent。它沒有
    // 被回收，所以不能記 evicted*——despawn 自己已經記過 despawn-stale，再補一筆
    // 只會讓審計線宣稱發生過一次沒發生的回收
    if result == DespawnResult::Stale {
        ev(EvictEvent::Warn(format!(
            "警告：agent '{name}' 的註冊已清除，但 pane {pane} 已不屬於它、未被回收；收尾任務 {task_id}（{final_st}）請自行判讀"
        )));
        return Ok(EvictOutcome {
            task_id,
            final_status: final_st,
            audit,
            despawn: result,
            pane,
        });
    }
    // 記在 despawn 成功之後：despawn 失敗代表 pane 還在、根本沒被驅逐
    if registry::log_agent_event(paths, tmux, audit, name, &pane, &runtime, None).is_err() {
        ev(EvictEvent::Warn(
            "警告：evict 已完成，但審計未落地（agents.log append 失敗）".to_string(),
        ));
    }
    match audit {
        "evicted" => ev(EvictEvent::Info(format!(
            "已 evict agent '{name}'；收尾筆記可用：agent-bridge read {task_id}"
        ))),
        "evicted-unfinished" => ev(EvictEvent::Warn(format!(
            "警告：收尾任務 {task_id} 以 {final_st} 結束，筆記未落地；agent '{name}' 仍已回收"
        ))),
        _ => ev(EvictEvent::Warn(format!(
            "警告：收尾任務 {task_id} 逾時（{}s）未回覆，筆記未落地；agent '{name}' 仍已回收（避免 cap 卡死）",
            req.timeout
        ))),
    }
    Ok(EvictOutcome {
        task_id,
        final_status: final_st,
        audit,
        despawn: result,
        pane,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI-EVICT-4 的 compare-and-act 比對：只驗帶到的那一項，兩項都不帶時
    /// MUST 通過（不帶 expect 參數＝行為與現行完全相同）。
    #[test]
    fn check_expect_only_validates_supplied_fields() {
        let ok = |ep, eg| check_expect("w1", "%5", "tag-1", ep, eg);
        // 都不帶／各自相符／兩者皆符 → 通過
        assert!(ok(None, None).is_ok(), "不帶 expect MUST 與現行行為相同");
        assert!(ok(Some("%5"), None).is_ok());
        assert!(ok(None, Some("tag-1")).is_ok());
        assert!(ok(Some("%5"), Some("tag-1")).is_ok());

        // 任一不符 → selection stale，且訊息要指出實際值與期望值
        for (ep, eg, want) in [
            (Some("%9"), None, "%9"),
            (None, Some("tag-2"), "tag-2"),
            (Some("%5"), Some("tag-2"), "tag-2"),
            (Some("%9"), Some("tag-1"), "%9"),
        ] {
            let e = ok(ep, eg).unwrap_err();
            assert!(
                e.message.contains("selection stale"),
                "MUST 含 selection stale：{}",
                e.message
            );
            assert!(e.message.contains(want), "MUST 帶期望值：{}", e.message);
            assert!(
                e.message.contains("未建立任何任務"),
                "MUST 說清楚沒有副作用：{}",
                e.message
            );
        }
    }

    /// 等價 CLI 原文（薄殼原則）：預設值不入命令列，帶了 expect 就要逐字帶上
    /// ——TUI 的證據框顯示的就是這一行，人貼出去要跑得到同一件事。
    #[test]
    fn cmdline_prints_only_non_default_flags() {
        let mut req = EvictRequest::new("w1");
        assert_eq!(req.cmdline(), "agent-bridge evict w1");
        req.expect_pane = Some("%5".into());
        req.expect_generation = Some("t-gen1".into());
        assert_eq!(
            req.cmdline(),
            "agent-bridge evict w1 --expect-pane %5 --expect-generation t-gen1"
        );
        req.from = "tui".into();
        req.timeout = 30;
        assert_eq!(
            req.cmdline(),
            "agent-bridge evict w1 --from tui --timeout 30 --expect-pane %5 --expect-generation t-gen1"
        );
    }

    /// registry 是 worker 可寫面：`cmdline()` 承諾「可貼上重跑」，動態值就
    /// MUST 引用（codex 複核 major #4）。空白破壞 argv 等價，`;`／`$(…)`／
    /// 單引號則是可執行的注入。
    #[test]
    fn cmdline_quotes_hostile_registry_values() {
        let mut req = EvictRequest::new("w1");
        req.expect_pane = Some("%5 extra".into());
        req.expect_generation = Some("t; rm -rf /tmp/x".into());
        let s = req.cmdline();
        // 引用後：危險字元前一定有反斜線（`printf %q` 形式），不再是裸的
        // 分隔符／命令起點
        assert!(s.contains(r"%5\ extra"), "空白 MUST 被引用：{s}");
        assert!(!s.contains("t; rm"), "分號 MUST NOT 裸露：{s}");
        assert!(s.contains(r"\;"), "分號 MUST 被引用：{s}");

        req.expect_pane = Some("$(touch /tmp/pwned)".into());
        req.expect_generation = Some("it's".into());
        let s = req.cmdline();
        assert!(!s.contains("$(touch"), "命令替換 MUST NOT 裸露：{s}");
        assert!(s.contains(r"\$\(touch"), "實際：{s}");
        assert!(s.contains(r"it\'s"), "單引號 MUST 被引用：{s}");

        // 名稱與 sender 同樣走引用（TUI 的 name 來自 registry 檔名）
        let mut req2 = EvictRequest::new("w 1");
        req2.from = "a;b".into();
        let s2 = req2.cmdline();
        assert!(s2.contains(r"w\ 1") && s2.contains(r"a\;b"), "實際：{s2}");

        // 正常值逐字不變（既有畫面斷言的前提）
        let mut req3 = EvictRequest::new("ev43");
        req3.expect_pane = Some("%12".into());
        req3.expect_generation = Some("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-ev43-1-abc".into());
        assert_eq!(
            req3.cmdline(),
            "agent-bridge evict ev43 --expect-pane %12 --expect-generation AGENT_BRIDGE_SPAWN_TAG=ab-spawn-ev43-1-abc"
        );
    }

    /// CLI-EVICT-2 的三支分流不得合併：completed→`evicted`；failed／cancelled
    /// →`evicted-unfinished`；逾時（await 回 `Timeout`，終態字為空）→
    /// `evicted-timeout`。
    #[test]
    fn audit_event_keeps_the_three_outcomes_apart() {
        assert_eq!(audit_event("completed"), "evicted");
        assert_eq!(audit_event("failed"), "evicted-unfinished");
        assert_eq!(audit_event("cancelled"), "evicted-unfinished");
        assert_eq!(audit_event(""), "evicted-timeout");
        // 未知字一律走最保守的那支（筆記沒落地）
        assert_eq!(audit_event("running"), "evicted-timeout");
    }

    /// 出身判定的單一正本（TUI 的 `e` 直接消費這一份）：人工註冊、無法解析、
    /// 缺 spawn_tag 三條都 MUST 拒絕，且訊息各自可辨識。
    #[test]
    fn precheck_rejects_manual_corrupt_and_tagless() {
        let dir = std::env::temp_dir().join(format!("ab-evict-precheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |n: &str, body: &str| {
            let p = dir.join(n);
            std::fs::write(&p, body).unwrap();
            p
        };

        let missing = dir.join("nope.json");
        assert!(
            precheck_registry(&missing, "nope")
                .unwrap_err()
                .message
                .contains("未註冊")
        );

        let manual = write("manual.json", r#"{"name":"manual","pane_id":"%1"}"#);
        assert!(
            precheck_registry(&manual, "manual")
                .unwrap_err()
                .message
                .contains("非 spawn 出身")
        );

        let broken = write("broken.json", "not json");
        assert!(
            precheck_registry(&broken, "broken")
                .unwrap_err()
                .message
                .contains("出身不明")
        );

        let tagless = write(
            "tagless.json",
            r#"{"name":"tagless","pane_id":"%2","spawned":true}"#,
        );
        assert!(
            precheck_registry(&tagless, "tagless")
                .unwrap_err()
                .message
                .contains("沒有 spawn_tag")
        );

        let good = write(
            "good.json",
            r#"{"name":"good","pane_id":"%3","spawned":true,"spawn_tag":"t-1"}"#,
        );
        assert_eq!(
            precheck_registry(&good, "good").unwrap(),
            ("%3".to_string(), "t-1".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

//! send 的兩段（建 task／通知），CLI 與 evict 編排的**單一正本**。
//!
//! 為什麼拆兩段：evict 的入口 CAS（CLI-EVICT-4）要求 expect 比對與收尾任務的
//! 建立在**同一把 registry 鎖內**完成，而通知 MUST 在鎖外（送鍵帶延遲，圈進
//! 鎖裡會與 spawn／despawn 互撞）。前半自己不取鎖，呼叫端因此可以在持鎖狀態
//! 下呼叫它。
//!
//! **函式內不印任何字**（審查 F7）：`create_send_task` 的 not-ready 警告經
//! `warn` callback 交回呼叫端——CLI 印 stderr、TUI 進 footer。用 callback 而
//! 不是回傳值，是因為那句警告 MUST 在建 task **之前**發出：建 task 失敗時
//! （`Err` 路徑）舊行為照樣印得出它。

use crate::error::{Error, Result};
use crate::notify::{self, NotifyOutcome};
use crate::paths::Paths;
use crate::registry;
use crate::task::{self, MessageSource};
use crate::tmux::TmuxClient;
use crate::validate::is_valid_name;

/// `do_send` 的前半：名稱文法 → 收件者已註冊 → not-ready 警告 → 建 task。
pub fn create_send_task(
    paths: &Paths,
    to: &str,
    from: &str,
    src: &MessageSource,
    pinned: bool,
    warn: &mut dyn FnMut(String),
) -> Result<String> {
    if !is_valid_name(to) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{to}"
        )));
    }
    if !is_valid_name(from) {
        return Err(Error::new(format!(
            "sender 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{from}"
        )));
    }
    let agent_file = paths.agents_dir.join(format!("{to}.json"));
    if !agent_file.is_file() {
        return Err(Error::new(format!(
            "未註冊的 agent：{to}（先用 agent-bridge register）"
        )));
    }
    if registry::is_spawned_not_ready(&agent_file) {
        warn(format!(
            "警告：agent '{to}' 尚未回報就緒（starting），通知可能延後；訊息已入 mailbox 不會遺失"
        ));
    }

    task::create_task(paths, from, to, src, pinned)
}

/// 一次收件通知的終態＋呈現所需的欄位（呼叫端據此組訊息）。
pub struct NotifyReport {
    pub outcome: NotifyOutcome,
    pub agent: String,
    pub pane: String,
    pub cmdline: String,
}

impl NotifyReport {
    /// 給人看的一句話（`None`＝送達成功，沒有話要說）。
    pub fn message(&self) -> Option<String> {
        notify::outcome_message(self.outcome, &self.agent, &self.pane, &self.cmdline)
    }
}

/// `do_send` 的後半：通知收件者。**MUST 在釋放 registry 鎖之後呼叫**
/// ——`notify_or_defer_outcome` 會送 tmux 鍵並帶延遲，圈進鎖裡會讓 registry
/// 鎖被持有數百毫秒以上，與 spawn／despawn 互撞（`acquire_lock` 只重試 25 次
/// × 0.2s 就放棄）。
///
/// 不印字版（審查 F7）：終態交回呼叫端呈現。
pub fn notify_send(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    to: &str,
    task_id: &str,
) -> Result<NotifyReport> {
    // 通知前重讀 pane（cmd_send:613-619）：從參數檢查到這裡隔著建目錄＋三次
    // 寫檔，期間同名 agent 可能被 unregister＋register 換到別的 pane——舊 pane
    // 若已屬別人的 session，這行 command＋Enter 就打進無辜視窗。重讀把窗口縮到
    // 次毫秒級；徹底關閉需要「讀 registry 與 send-keys」原子化，tmux 給不了。
    let agent_file = paths.agents_dir.join(format!("{to}.json"));
    let pane = registry::read_pane(&agent_file);
    let cmdline = format!("agent-bridge receive {task_id}");
    let outcome =
        notify::notify_or_defer_outcome(paths, tmux, to, &pane, &cmdline, task_id, "receive")?;
    Ok(NotifyReport {
        outcome,
        agent: to.to_string(),
        pane,
        cmdline,
    })
}

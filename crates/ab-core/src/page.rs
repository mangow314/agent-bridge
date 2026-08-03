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
//! 事件本身一定落盤（rubric v2 page 層條 4），所以事後回頭查得到。

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

impl PageEvent {
    /// 去重軸。**同一個 key 只會推一次**，跨行程、跨重啟都算數（key 落在磁碟
    /// 上，不在記憶體裡——行程重啟不重推是 rubric v2 page 層條 2 的一半）。
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

    /// 通知標題。**帶 agent 名**——rubric v2 的 human judgment 要求受測者只看
    /// 通知就說得出「哪個 agent」，名字不在標題裡就等著人去開面板查。
    pub fn title(&self) -> String {
        match self {
            PageEvent::TaskFailed { agent, .. } => format!("agent-bridge: {agent} 任務失敗"),
            PageEvent::WorkerDied { agent, .. } => format!("agent-bridge: {agent} 死了"),
        }
    }

    /// 通知內文。**說出「為何現在需要我」**，不只是報一個狀態字。
    pub fn body(&self) -> String {
        match self {
            PageEvent::TaskFailed { task, .. } => {
                format!("task {task} 進 failed 終態，回覆已在信箱等你讀")
            }
            PageEvent::WorkerDied { task, .. } => {
                format!("pane 已不存在，task {task} 還掛在它身上——沒有人會回這一則")
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

/// 真的去跑。stdout／stderr 一律丟掉：推播是副作用，它的雜訊不得混進呼叫端的
/// 輸出契約（§1 非目標）。
pub struct SubprocessRunner;

impl CmdRunner for SubprocessRunner {
    fn run(&self, argv: &[String]) -> bool {
        let Some((prog, rest)) = argv.split_first() else {
            return false;
        };
        std::process::Command::new(prog)
            .args(rest)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
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
            return self.runner.run(&[
                "notify-send".to_string(),
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
        let msg = format!("{title} — {body}");
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
    /// 事件是新的：已落盤，且推播已嘗試（`paged` 說明有沒有送出去）。
    Emitted { paged: bool },
    /// 這個 key 先前已處理過——事件流不再多一筆，也不再推一次。
    AlreadySeen,
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
pub fn emit(paths: &Paths, event: &PageEvent, pager: &dyn Pager) -> Result<EmitOutcome> {
    let key = event.key();
    if is_seen(paths, &key)? {
        return Ok(EmitOutcome::AlreadySeen);
    }
    {
        let guard = acquire_lock(paths, LOCK_ID)?;
        if is_seen(paths, &key)? {
            guard.release();
            return Ok(EmitOutcome::AlreadySeen);
        }
        let r = append_event(paths, event).and_then(|()| mark_seen(paths, &key));
        guard.release();
        r?;
    }
    let paged = pager.page(&event.title(), &event.body());
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

fn append_event(paths: &Paths, event: &PageEvent) -> Result<()> {
    // 欄位序＝人讀 jsonl 時的掃描序：時間、類別、誰、哪一筆、去重軸、給人看的話
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), now_iso().into());
    obj.insert("kind".into(), event.kind().into());
    obj.insert("agent".into(), event.agent().into());
    obj.insert("task".into(), event.task().into());
    obj.insert("key".into(), event.key().into());
    obj.insert("message".into(), event.body().into());
    let line = format!("{}\n", serde_json::Value::Object(obj));
    append_line(&events_path(paths), &line)
}

fn mark_seen(paths: &Paths, key: &str) -> Result<()> {
    append_line(&seen_path(paths), &format!("{key}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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
        calls: RefCell<Vec<Vec<String>>>,
        available: bool,
    }

    impl FakeTmux {
        fn with_clients(clients: &'static str) -> Self {
            FakeTmux {
                clients: Some(clients),
                calls: RefCell::new(Vec::new()),
                available: true,
            }
        }
        fn unavailable() -> Self {
            FakeTmux {
                clients: None,
                calls: RefCell::new(Vec::new()),
                available: false,
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
            let stdout = if args.first() == Some(&"list-clients") {
                self.clients.unwrap_or("").to_string()
            } else {
                String::new()
            };
            Some(crate::tmux::TmuxOutput {
                status_ok: true,
                stdout,
                stderr: String::new(),
            })
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
            emit(&paths, &failed("t1"), &pager).unwrap(),
            EmitOutcome::Emitted { paged: true }
        );
        assert_eq!(
            emit(&paths, &failed("t1"), &pager).unwrap(),
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
        emit(&paths, &failed("t1"), &first).unwrap();
        let second = RecordingPager::new(true);
        assert_eq!(
            emit(&paths, &failed("t1"), &second).unwrap(),
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
            emit(&paths, &failed("t1"), &pager).unwrap(),
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
        emit(&paths, &failed("t1"), &pager).unwrap();
        emit(&paths, &failed("t2"), &pager).unwrap();
        emit(
            &paths,
            &PageEvent::WorkerDied {
                agent: "w1".into(),
                spawn_tag: "ab-spawn-w1-1-aaaaaaaaaaaa".into(),
                task: "t1".into(),
            },
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
        emit(&paths, &died("ab-spawn-w1-1-aaaaaaaaaaaa"), &pager).unwrap();
        emit(&paths, &died("ab-spawn-w1-2-bbbbbbbbbbbb"), &pager).unwrap();
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
            emit(&paths, &failed("t1"), &pager).unwrap(),
            EmitOutcome::Emitted { paged: false }
        );
        assert_eq!(event_lines(&paths).len(), 1);
    }

    /// 通知文字本身要答得出「哪個 agent、為何現在需要我」——不必打開任何面板
    /// （rubric v2 page 層 human judgment 的機器可判定那一半）。
    #[test]
    fn the_page_text_names_the_agent_and_the_task() {
        let e = failed("20260803T000000Z-abcd");
        assert!(e.title().contains("w1"), "標題要有 agent 名：{}", e.title());
        assert!(
            e.body().contains("20260803T000000Z-abcd"),
            "內文要有 task id：{}",
            e.body()
        );
        let d = PageEvent::WorkerDied {
            agent: "w9".into(),
            spawn_tag: "ab-spawn-w9-1-cccccccccccc".into(),
            task: "t7".into(),
        };
        assert!(d.title().contains("w9"), "標題要有 agent 名：{}", d.title());
        assert!(d.body().contains("t7"), "內文要有 task id：{}", d.body());
    }
}

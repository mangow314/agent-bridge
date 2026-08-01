//! ab-tui：`agent-bridge ui` 的 alternate-screen dashboard（設計正本
//! `docs/tui-design.md`；本 crate 為第一縱切 §8：OWNERS｜WORKERS 兩欄＋
//! `Enter` focus＋`x` cancel）。
//!
//! 形狀（§2）：lib crate，由 `ab` 的 `ui` 子指令呼叫 `run()`——部署邊界是
//! `cp target/release/ab bin/ab` 單一 binary。依賴上限（§6）：ratatui＋
//! crossterm＋ab-core，不上 async runtime／daemon／clipboard crate。
//! tmux 一律走 `ab-core::tmux::TmuxClient`（bounded，ENV-TMUX-1），且**全部
//! 在背景 worker thread 上跑**（`worker.rs`）：UI thread 只 non-blocking 收
//! 信，逾時／卡住的終態是該欄 stale，MUST NOT 凍結 UI（§4 硬條款）。

mod action;
mod app;
mod model;
mod theme;
mod view;
mod worker;

use std::time::{Duration, Instant};

use ab_core::error::{Error, Result};
use ab_core::paths::Paths;
use ab_core::tmux::SubprocessTmux;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use app::{App, Effect, Key};
use model::{BlockerDebounce, BlockerIndex, LiveIndex, Model};
use worker::{Msg, Req};

/// 磁碟 read model 的輪詢節奏（§4：500ms；tmux liveness 另以 2s 節流）。
const DISK_POLL: Duration = Duration::from_millis(500);
const LIVE_POLL: Duration = Duration::from_secs(2);
/// 鍵盤事件的等待窗：這是 UI 的心跳上限，不是輪詢節奏本身。
const EVENT_TICK: Duration = Duration::from_millis(100);

/// 啟動器協定（CLI-UI-1／ENV-UI-1）：`AGENT_BRIDGE_UI_POPUP=1` 由 tmux binding
/// 設定（`display-popup -E`），**程式不自行偵測 popup**（§2）。設了它，
/// `Enter` focus 成功後直接正常退出——`-E` 的行程結束即關 popup，人於是落在
/// 目標 pane 上；沒設就維持現行（focus 後繼續跑）。
const ENV_UI_POPUP: &str = "AGENT_BRIDGE_UI_POPUP";

/// `agent-bridge ui` 的入口。terminal 生命週期全部在這裡收攏：
/// `ratatui::try_init` 啟 raw mode＋alternate screen 並**裝 panic hook**
/// （panic 時先還原 terminal 再走預設 hook）；正常返回、錯誤返回、**以及
/// 初始化自己半途失敗**都經 `ratatui::restore()`（raw mode off＋離開 alt
/// screen）——P1 gate (b)。
pub fn run() -> Result<()> {
    let paths = Paths::resolve();
    // tmux 一律走背景 worker：連開場的呼叫者定位都不在 UI thread 上做
    let worker = worker::spawn(SubprocessTmux, paths.clone());

    let mut terminal = init_terminal(ratatui::try_init, ratatui::restore)?;
    let outcome = event_loop(&mut terminal, &paths, &worker);
    ratatui::restore();
    outcome
}

/// init 的 cleanup 包裝：`try_init` 是「裝 panic hook → 開 raw mode → 進 alt
/// screen」三步，後兩步失敗會以 `Err` 返回而**不觸發 panic hook**，直接
/// `?` 出去就會把 raw mode 留給使用者的 shell（審查 F8）。故 Err 路徑
/// best-effort 還原（失敗也吞掉——此時已無畫面可報，能還原多少算多少）。
fn init_terminal<T, I, R>(init: I, restore: R) -> Result<T>
where
    I: FnOnce() -> std::io::Result<T>,
    R: FnOnce(),
{
    match init() {
        Ok(t) => Ok(t),
        Err(e) => {
            restore();
            Err(Error::new(format!("cannot initialise terminal: {e}")))
        }
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    paths: &Paths,
    worker: &worker::Handle,
) -> Result<()> {
    // 測試注入點（分組 40 gate (b) 的錯誤退出案例）：畫過第一幀後 panic／回
    // Err，驗證兩條錯誤退出路徑都還原 terminal。刻意**不用** AGENT_BRIDGE_
    // 前綴：那個命名空間是使用者設定面（spec/env.md 逐一列管、check-contract
    // 第 1 項機器比對），這是測試 harness 的注入點，不是設定旋鈕。
    let selftest_panic = std::env::var_os("AB_TUI_SELFTEST_PANIC").is_some_and(|v| v == "1");
    let selftest_err = std::env::var_os("AB_TUI_SELFTEST_ERR").is_some_and(|v| v == "1");
    let popup = std::env::var_os(ENV_UI_POPUP).is_some_and(|v| v == "1");

    let mut model = Model::load(paths);
    // liveness 起始為 unknown，等 worker 第一則回報——不在 UI thread 上查
    let mut live = LiveIndex::unknown();
    // blocker 軸同樣起始 unknown：unknown MUST NOT 顯示成「沒有 blocker」（§5）
    let mut blockers = BlockerIndex::unknown();
    // screen-matcher 來源的 blocker 需連續命中才升旗（§4 去抖）；狀態跨輪，
    // 故活在主迴圈而不是每輪重建的 BlockerIndex 裡
    let mut debounce = BlockerDebounce::new();
    let mut app = App::new();
    let mut last_disk = Instant::now();
    let mut last_live = Instant::now();
    // 同時只讓一個 liveness 請求在途：worker 卡住時不得堆積無界佇列
    let mut live_inflight = true;

    loop {
        terminal
            .draw(|f| view::render(f, &model, &live, &blockers, &app))
            .map_err(|e| Error::new(format!("draw failed: {e}")))?;
        if selftest_panic {
            panic!("AB_TUI_SELFTEST_PANIC: test-triggered panic exit (terminal restore check)");
        }
        if selftest_err {
            return Err(Error::new(
                "AB_TUI_SELFTEST_ERR: test-triggered error return",
            ));
        }

        // worker 回信：non-blocking drain，一則都不等
        while let Some(msg) = worker.try_recv() {
            match msg {
                Msg::Origin { owner, pane } => {
                    app.caller_origin = owner;
                    app.caller_pane = pane;
                    app.apply_origin(&model);
                }
                Msg::Live(l, b) => {
                    apply_live(&mut live, &mut blockers, &mut debounce, l, b);
                    live_inflight = false;
                }
                Msg::Focus { label, pane, res } => match res {
                    Ok(()) => {
                        // popup 模式：focus 成功即正常退出，由 `-E` 收掉 popup
                        if popup {
                            return Ok(());
                        }
                        app.message = format!("focused '{label}' ({pane})");
                    }
                    Err(e) => app.message = e.message,
                },
                Msg::Cancel { id, res } => {
                    app.message = match res {
                        Ok(o) => format!("cancelled task {id}{}", notify_suffix(&o)),
                        Err(e) => e.message,
                    };
                    // 取消結果下一幀就要看得到
                    model = Model::load(paths);
                    app.relocate(&model);
                    last_disk = Instant::now();
                }
                // `r`：成功開全螢幕 pager（保留原始 bytes，render 才 lossy）；
                // 失敗（未回覆／已取消／損壞）逐字沿用 core 的訊息進 footer
                Msg::Read { id, res } => match res {
                    Ok(o) => {
                        app.pager = Some(app::Pager {
                            id,
                            from: o.from,
                            to: o.to,
                            bytes: o.bytes,
                            scroll: 0,
                        });
                        app.message.clear();
                    }
                    Err(e) => app.message = e.message,
                },
                Msg::Copy { res } => app.message = match res {
                    Ok(()) => {
                        "evidence copied to the tmux buffer (read it back with tmux show-buffer)"
                            .to_string()
                    }
                    Err(e) => e.message,
                },
                // evict 的編排事件（core 不印字）：串流進 footer，人才看得到
                // 「已派收尾任務、等待中」這段長達數分鐘的過程。
                // **警告走 sticky 區、進度才覆寫單行 message**：兩者混用一個
                // 欄位時，`notify-failed` 會被下一行「等待筆記落地」蓋掉
                // （codex 複核 major #2）
                Msg::EvictProgress { name, line, warn } => {
                    // `line` 是 **core 的編排事件原文**，不是 TUI chrome
                    // （題 9 只管 chrome）——照抄不譯，兩邊各譯一份會漂移
                    let text = format!("evict '{name}': {line}");
                    if warn {
                        app.push_warning(text);
                    } else {
                        app.message = text;
                    }
                }
                Msg::Evict { name, res } => {
                    let text = action::evict_message(&name, &res);
                    // 終局本身若是壞消息（失敗／stale／筆記未落地）同樣 sticky
                    // ——它是這一輪最該被看到的一行，不能被下一次刷新蓋掉
                    if evict_outcome_is_clean(&res) {
                        app.message = text;
                    } else {
                        app.push_warning(text);
                    }
                    app.evict_inflight.remove(&name);
                    // 回收結果下一幀就要看得到（registry／task 都動過了）
                    model = Model::load(paths);
                    app.relocate(&model);
                    last_disk = Instant::now();
                }
            }
        }

        let has_event =
            event::poll(EVENT_TICK).map_err(|e| Error::new(format!("event poll failed: {e}")))?;
        if has_event {
            let ev = event::read().map_err(|e| Error::new(format!("event read failed: {e}")))?;
            if let Event::Key(k) = ev
                && k.kind == KeyEventKind::Press
                && let Some(key) = translate(k.code)
            {
                match app::handle_key(&mut app, &model, key) {
                    Effect::Quit => return Ok(()),
                    Effect::Focus { pane, label } => {
                        // 動作也走 worker：跨 session 的 switch-client 一樣是
                        // tmux 子行程，卡住不得凍住畫面與鍵盤
                        app.message = format!("focus '{label}' in progress…");
                        dispatch(&mut app, worker, Req::Focus { pane, label });
                    }
                    Effect::Cancel { id } => {
                        app.message = format!("cancel {id} in progress…");
                        dispatch(&mut app, worker, Req::Cancel { id });
                    }
                    // `r` 走背景 worker：read 會取 task 鎖，鎖被握著時在 UI
                    // thread 上就是凍結（§4 硬條款）
                    Effect::Read { id } => {
                        app.message = format!("read {id} in progress…");
                        dispatch(&mut app, worker, Req::Read { id });
                    }
                    // `i` 只消費已載入的 read model／liveness 快照，不開檔也
                    // 不查 tmux，故就地組頁（同一份快照＝同一個世代）
                    Effect::Info { worker: name } => {
                        app.info = Some(action::info_page(&model, &live, &blockers, &name));
                    }
                    Effect::Copy { payload } => {
                        app.message = "copying evidence…".to_string();
                        dispatch(&mut app, worker, Req::Copy { payload });
                    }
                    // `e` 第一段：讀 registry（小 JSON、不取鎖）做出身判定並
                    // 取當下世代。非 spawn 出身／缺 spawn_tag 就地拒絕，訊息
                    // 原樣沿用 core 的判定
                    Effect::EvictPrompt { worker: name } => {
                        match action::evict_prompt(paths, &name) {
                            Ok(p) => {
                                app.evict_prompt = Some(p);
                                app.message.clear();
                            }
                            Err(e) => app.message = e.message,
                        }
                    }
                    // `e` 第二段：確認當下**重讀** registry（§5），再交給一次性
                    // thread 跑完整編排——evict 的 await 段預設等 300s，搭常駐
                    // worker 那條 thread 等於五分鐘不再有 liveness
                    Effect::Evict { shown } => {
                        start_evict(&mut app, worker, paths, shown);
                    }
                    Effect::None => {}
                }
            }
        }

        if last_disk.elapsed() >= DISK_POLL {
            model = Model::load(paths);
            app.relocate(&model);
            app.apply_origin(&model);
            last_disk = Instant::now();
        }
        if !live_inflight && last_live.elapsed() >= LIVE_POLL {
            live_inflight = worker.send(Req::Live);
            last_live = Instant::now();
        }
    }
}

/// evict 終局是否「乾淨到可以只當一則普通訊息」：唯一算乾淨的是「筆記落地
/// 且 pane 真的被回收」。其餘（錯誤、stale、unfinished、timeout）都是人要
/// 追的事，走 sticky 警告。
fn evict_outcome_is_clean(res: &ab_core::error::Result<ab_core::evict::EvictOutcome>) -> bool {
    matches!(res, Ok(o)
        if o.audit == "evicted" && o.despawn != ab_core::spawn::DespawnResult::Stale)
}

/// panic payload → 給人看的一句話（`&str`／`String` 兩種常見形狀，其餘給
/// 一個固定字面）。
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// 把一次性工人的 unwind 轉成 terminal error。
///
/// 沒有這一層，工人 panic 時就**沒有終局訊息**：畫面永遠停在「進行中…」、
/// in-flight 閘永遠不放開，那個 worker 在這個 session 裡再也 evict 不了
/// （codex 複核 major #3）。錯誤訊息指向 CLI，人才有下一步可走。
fn guard_panic<T>(f: impl FnOnce() -> ab_core::error::Result<T>) -> ab_core::error::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        Err(ab_core::error::Error::new(format!(
            "the evict background worker died unexpectedly ({}): check the real state with agent-bridge list --long",
            panic_text(p.as_ref())
        )))
    })
}

/// `e` 確認後的執行段：重讀 → in-flight 閘 → 一次性 thread。
///
/// UI thread 在這裡只做兩件不會阻塞的事（讀一份小 JSON、起 thread），編排本身
/// 全在一次性 thread 上；進度與終局都經同一個 mpsc 回來（§4：UI thread 永不
/// 阻塞）。
fn start_evict(app: &mut App, worker: &worker::Handle, paths: &Paths, shown: action::EvictShown) {
    if app.evict_inflight.contains(&shown.name) {
        app.message = format!("evict of '{}' is already in progress", shown.name);
        return;
    }
    let req = match action::evict_request(paths, &shown) {
        Ok(r) => r,
        Err(e) => {
            // 確認期的 stale 是人要看到的事實（他選的那一代已經不在了），
            // 不能被下一輪刷新蓋掉
            app.push_warning(e.message);
            return;
        }
    };
    let name = shown.name.clone();
    let paths = paths.clone();
    let progress_name = name.clone();
    let done_name = name.clone();
    // 命令原文先留一份：req 之後會被 move 進工人，而起不來的那條路徑要靠它
    // 告訴人「改用 CLI 跑哪一條」
    let cmdline = req.cmdline();
    let started = worker.spawn_oneshot(move |tx| {
        let progress_tx = tx.clone();
        // **unwind 一律轉成 terminal error**：工人 panic 時不會有終局訊息，
        // 畫面永遠停在「進行中…」、in-flight 閘也永遠不放開，那個 worker 在
        // 這個 session 裡就再也 evict 不了（codex 複核 major #3）
        let res = guard_panic(|| {
            let tmux = SubprocessTmux;
            ab_core::evict::evict(&paths, &tmux, &req, &mut |e| {
                let (line, warn) = match e {
                    ab_core::evict::EvictEvent::Warn(m) => (m, true),
                    ab_core::evict::EvictEvent::Info(m) => (m, false),
                };
                let _ = progress_tx.send(Msg::EvictProgress {
                    name: progress_name.clone(),
                    line,
                    warn,
                });
            })
        });
        let _ = tx.send(Msg::Evict {
            name: done_name,
            res,
        });
    });
    if started {
        app.evict_inflight.insert(name.clone());
        app.message = format!("evict '{name}' in progress… (wrap-up task, then reclaim)");
    } else {
        // thread 起不來（`Builder::spawn` 失敗）：當場給終態，別讓畫面停在
        // 一個不會結束的「進行中」
        app.push_warning(format!(
            "evict '{name}' could not start (background worker failed to spawn); run it from the CLI instead: {cmdline}"
        ));
    }
}

/// 把請求丟給背景 worker；channel 已斷就**當場給終態**。
///
/// `Handle::send` 用 bool 表達 disconnected，丟掉它的話畫面會永遠停在
/// 「…進行中…」——一個不會結束的進行中比一個明確的失敗更糟，人會一直等
/// （跨廠審查 minor #5）。
fn dispatch(app: &mut App, worker: &worker::Handle, req: Req) {
    if !worker.send(req) {
        app.message =
            "background worker is gone (tmux actions cannot run); quit and reopen".to_string();
    }
}

/// cancel 的通知終態進 footer（不印 stderr——alt screen 下會畫花畫面，
/// 審查 F7）。`None`＝對方未註冊，依 CLI 正本不通知也不算失敗。
fn notify_suffix(o: &ab_core::task::CancelOutcome) -> String {
    use ab_core::notify::NotifyOutcome;
    match o.notify {
        None => String::new(),
        Some(NotifyOutcome::Notified) => format!("; notified {}", o.to),
        Some(NotifyOutcome::Deferred) => {
            format!(
                "; {} is busy, notification deferred (its hook picks it up at turn end)",
                o.to
            )
        }
        Some(NotifyOutcome::Failed) => format!(
            "; could not notify {} (pane {}): run {} by hand",
            o.to, o.pane, o.cmdline
        ),
    }
}

fn translate(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Tab => Some(Key::Tab),
        // `Shift+Tab`：crossterm 回的是獨立的 BackTab，不是帶 SHIFT 修飾的 Tab
        KeyCode::BackTab => Some(Key::BackTab),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Up => Some(Key::Up),
        _ => None,
    }
}

/// 一則 `Msg::Live` 落進 read model 的**唯一路徑**。
///
/// 抽成函式而不是內嵌在主迴圈裡，是為了讓「blocker 有沒有真的過去抖」可被
/// 測試殺掉：內嵌時把 `debounce.apply(b)` 改回 `b` 全套照樣綠——去抖等於裸奔。
/// liveness 直接覆蓋（它沒有單幀誤判面），blocker MUST 過 `debounce`。
fn apply_live(
    live: &mut LiveIndex,
    blockers: &mut BlockerIndex,
    debounce: &mut BlockerDebounce,
    l: LiveIndex,
    b: BlockerIndex,
) {
    *live = l;
    *blockers = debounce.apply(b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::Blocker;
    use std::cell::Cell;
    use std::collections::HashMap;

    /// init 半途失敗（raw mode 已開、進 alt screen 失敗）：MUST best-effort
    /// 還原，不得把 raw mode 留給呼叫者的 shell（審查 F8）。
    #[test]
    fn init_failure_restores_terminal() {
        let restored = Cell::new(false);
        let r: Result<()> = init_terminal(
            || Err(std::io::Error::other("alt screen 進不去")),
            || restored.set(true),
        );
        let err = r.unwrap_err();
        assert!(restored.get(), "init 失敗路徑 MUST 還原 terminal");
        assert!(err.message.contains("alt screen"), "實際：{}", err.message);
    }

    /// 工人 panic MUST 變成 terminal error（而不是「沒有終局訊息」）：
    /// 呼叫端據此送出 `Msg::Evict`，in-flight 閘才放得開（major #3）。
    #[test]
    fn guard_panic_turns_unwind_into_terminal_error() {
        // 正常路徑原樣通過
        assert_eq!(guard_panic(|| Ok(7u8)).unwrap(), 7);
        let e = guard_panic::<u8>(|| Err(Error::new("一般錯誤"))).unwrap_err();
        assert_eq!(e.message, "一般錯誤");

        // panic（&str／String 兩種 payload）都收斂成可讀的終態訊息
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // 測試輸出不要被 panic 訊息洗版
        let e = guard_panic::<u8>(|| panic!("boom-str")).unwrap_err();
        let e2 = guard_panic::<u8>(|| panic!("{}", String::from("boom-string"))).unwrap_err();
        std::panic::set_hook(prev);

        for (e, needle) in [(e, "boom-str"), (e2, "boom-string")] {
            assert!(
                e.message.contains("died unexpectedly"),
                "實際：{}",
                e.message
            );
            assert!(
                e.message.contains(needle),
                "MUST 帶 panic 原因：{}",
                e.message
            );
            assert!(
                e.message.contains("agent-bridge list --long"),
                "MUST 指出下一步：{}",
                e.message
            );
        }
    }

    /// 非 `&str`／`String` 的 panic payload（`panic_any`）：fallback 那一支
    /// 也是 chrome，MUST 是英文，且訊息其餘部分照樣完整（原因說不出來時，
    /// 「下一步怎麼查」更不能少）。
    #[test]
    fn guard_panic_falls_back_to_english_for_opaque_payloads() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let e = guard_panic::<u8>(|| std::panic::panic_any(42u32)).unwrap_err();
        std::panic::set_hook(prev);
        assert_eq!(
            e.message,
            "the evict background worker died unexpectedly (unknown panic): \
             check the real state with agent-bridge list --long",
            "fallback 訊息 MUST 逐字如此（英文、且保留下一步）"
        );
    }

    /// panic 之後**終局訊息仍要回到 UI 的收信口**（in-flight 閘靠它清除）。
    /// 這條錨的是 `start_evict` 的結構：guard 在 send 之前、沒有 early return。
    #[test]
    fn panicking_worker_still_delivers_terminal_message() {
        let (h, _req_rx) = worker::Handle::detached();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        assert!(h.spawn_oneshot(move |tx| {
            let res = guard_panic::<ab_core::evict::EvictOutcome>(|| panic!("worker 爆了"));
            let _ = tx.send(Msg::Evict {
                name: "w1".into(),
                res,
            });
            let _ = done_tx.send(());
        }));
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("一次性 thread 應完成");
        std::panic::set_hook(prev);

        match h.try_recv() {
            Some(Msg::Evict { name, res }) => {
                assert_eq!(name, "w1");
                let e = res.unwrap_err();
                assert!(
                    e.message.contains("died unexpectedly"),
                    "實際：{}",
                    e.message
                );
            }
            _ => panic!("panic 之後 MUST 仍送出 Msg::Evict 終局"),
        }
    }

    /// footer 的嚴重度分流（major #2）：只有「筆記落地＋真的回收」算乾淨，
    /// 其餘一律走 sticky 警告區，不得被下一則訊息覆寫。
    #[test]
    fn only_a_fully_clean_evict_avoids_the_warning_area() {
        use ab_core::evict::EvictOutcome;
        use ab_core::spawn::DespawnResult;
        let out = |audit: &'static str, despawn: DespawnResult| {
            Ok(EvictOutcome {
                task_id: "t".into(),
                final_status: "completed".into(),
                audit,
                despawn,
                pane: "%5".into(),
            })
        };
        assert!(evict_outcome_is_clean(&out(
            "evicted",
            DespawnResult::Killed
        )));
        assert!(!evict_outcome_is_clean(&out(
            "evicted",
            DespawnResult::Stale
        )));
        assert!(!evict_outcome_is_clean(&out(
            "evicted-unfinished",
            DespawnResult::Killed
        )));
        assert!(!evict_outcome_is_clean(&out(
            "evicted-timeout",
            DespawnResult::Killed
        )));
        assert!(!evict_outcome_is_clean(&Err(Error::new("selection stale"))));
    }

    /// 成功路徑不得誤觸還原（否則第一幀就沒有 alt screen 可畫）。
    #[test]
    fn init_success_does_not_restore() {
        let restored = Cell::new(false);
        let v = init_terminal(|| Ok(7u8), || restored.set(true)).unwrap();
        assert_eq!(v, 7);
        assert!(!restored.get());
    }

    fn prompt_round() -> BlockerIndex {
        let mut m = HashMap::new();
        m.insert("%1".to_string(), Blocker::Prompt);
        BlockerIndex { panes: Some(m) }
    }

    /// **wiring 測試**：`Msg::Live` 的 blocker MUST 經過去抖才進畫面。
    ///
    /// 斷言落在 `view::blocker_mark` 的輸出（人真正看到的那串），而不是只看
    /// 索引值：第一輪畫面上 MUST 沒有 `⛔blocked`，第二輪才有。
    /// 沒有這條，把 `apply_live` 裡的 `debounce.apply(b)` 改回 `b` 全套照樣綠。
    #[test]
    fn live_message_routes_blockers_through_the_debounce() {
        let mut live = LiveIndex::unknown();
        let mut blockers = BlockerIndex::unknown();
        let mut debounce = BlockerDebounce::new();

        apply_live(
            &mut live,
            &mut blockers,
            &mut debounce,
            LiveIndex::unknown(),
            prompt_round(),
        );
        assert_eq!(
            view::blocker_mark(blockers.get("%1")),
            "",
            "第一輪命中就升旗＝單幀回顯會變成常駐假警報"
        );

        apply_live(
            &mut live,
            &mut blockers,
            &mut debounce,
            LiveIndex::unknown(),
            prompt_round(),
        );
        assert_eq!(
            view::blocker_mark(blockers.get("%1")),
            "  ⛔blocked",
            "連續第二輪 MUST 升旗"
        );
    }
}

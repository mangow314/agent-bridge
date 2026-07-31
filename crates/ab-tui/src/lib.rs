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
mod view;
mod worker;

use std::time::{Duration, Instant};

use ab_core::error::{Error, Result};
use ab_core::paths::Paths;
use ab_core::tmux::SubprocessTmux;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use app::{App, Effect, Key};
use model::{LiveIndex, Model};
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
            Err(Error::new(format!("無法初始化 terminal：{e}")))
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
    let mut app = App::new();
    let mut last_disk = Instant::now();
    let mut last_live = Instant::now();
    // 同時只讓一個 liveness 請求在途：worker 卡住時不得堆積無界佇列
    let mut live_inflight = true;

    loop {
        terminal
            .draw(|f| view::render(f, &model, &live, &app))
            .map_err(|e| Error::new(format!("畫面繪製失敗：{e}")))?;
        if selftest_panic {
            panic!("AB_TUI_SELFTEST_PANIC：測試觸發的錯誤退出（驗 terminal 還原）");
        }
        if selftest_err {
            return Err(Error::new("AB_TUI_SELFTEST_ERR：測試觸發的錯誤返回"));
        }

        // worker 回信：non-blocking drain，一則都不等
        while let Some(msg) = worker.try_recv() {
            match msg {
                Msg::Origin { owner, pane } => {
                    app.origin_owner = owner;
                    app.origin_pane = pane;
                    app.apply_origin(&model);
                }
                Msg::Live(l) => {
                    live = l;
                    live_inflight = false;
                }
                Msg::Focus { label, pane, res } => match res {
                    Ok(()) => {
                        // popup 模式：focus 成功即正常退出，由 `-E` 收掉 popup
                        if popup {
                            return Ok(());
                        }
                        app.message = format!("已 focus '{label}'（{pane}）");
                    }
                    Err(e) => app.message = e.message,
                },
                Msg::Cancel { id, res } => {
                    app.message = match res {
                        Ok(o) => format!("已取消 task {id}（cancelled）{}", notify_suffix(&o)),
                        Err(e) => e.message,
                    };
                    // 取消結果下一幀就要看得到
                    model = Model::load(paths);
                    app.clamp(&model);
                    last_disk = Instant::now();
                }
            }
        }

        let has_event =
            event::poll(EVENT_TICK).map_err(|e| Error::new(format!("事件輪詢失敗：{e}")))?;
        if has_event {
            let ev = event::read().map_err(|e| Error::new(format!("事件讀取失敗：{e}")))?;
            if let Event::Key(k) = ev
                && k.kind == KeyEventKind::Press
                && let Some(key) = translate(k.code)
            {
                match app::handle_key(&mut app, &model, key) {
                    Effect::Quit => return Ok(()),
                    Effect::Focus { pane, label } => {
                        // 動作也走 worker：跨 session 的 switch-client 一樣是
                        // tmux 子行程，卡住不得凍住畫面與鍵盤
                        app.message = format!("focus '{label}' 進行中…");
                        worker.send(Req::Focus { pane, label });
                    }
                    Effect::Cancel { id } => {
                        app.message = format!("cancel {id} 進行中…");
                        worker.send(Req::Cancel { id });
                    }
                    Effect::None => {}
                }
            }
        }

        if last_disk.elapsed() >= DISK_POLL {
            model = Model::load(paths);
            app.clamp(&model);
            app.apply_origin(&model);
            last_disk = Instant::now();
        }
        if !live_inflight && last_live.elapsed() >= LIVE_POLL {
            live_inflight = worker.send(Req::Live);
            last_live = Instant::now();
        }
    }
}

/// cancel 的通知終態進 footer（不印 stderr——alt screen 下會畫花畫面，
/// 審查 F7）。`None`＝對方未註冊，依 CLI 正本不通知也不算失敗。
fn notify_suffix(o: &ab_core::task::CancelOutcome) -> String {
    use ab_core::notify::NotifyOutcome;
    match o.notify {
        None => String::new(),
        Some(NotifyOutcome::Notified) => format!("；已通知 {}", o.to),
        Some(NotifyOutcome::Deferred) => {
            format!(
                "；{} 忙碌中，通知延後（對方 turn 結束時由 hook 取件）",
                o.to
            )
        }
        Some(NotifyOutcome::Failed) => format!(
            "；無法通知 {}（pane {}）：請手動執行 {}",
            o.to, o.pane, o.cmdline
        ),
    }
}

fn translate(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Up => Some(Key::Up),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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

    /// 成功路徑不得誤觸還原（否則第一幀就沒有 alt screen 可畫）。
    #[test]
    fn init_success_does_not_restore() {
        let restored = Cell::new(false);
        let v = init_terminal(|| Ok(7u8), || restored.set(true)).unwrap();
        assert_eq!(v, 7);
        assert!(!restored.get());
    }
}

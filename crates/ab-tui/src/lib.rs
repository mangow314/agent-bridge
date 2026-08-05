//! ab-tui：`agent-bridge ui` 的 alternate-screen dashboard（設計正本
//! `docs/tui-design.md`）。版面到 P4.7 切片 B1 為止是
//! WORKERS｜TASKS｜DETAIL 三欄（可聚焦的只有前兩個），WORKERS 的唯一軸是
//! lineage 分組；ORIGINS 面板已退場。
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
use ab_core::tmux::{SubprocessTmux, TmuxClient};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use app::{App, Effect, Key};
use model::{BlockerDebounce, BlockerIndex, LiveIndex, Model, Snippets, Summaries};
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
    // 色盤在**進 alternate screen 之前**定案（P5.5）：`COLORTERM` 說得出
    // 24-bit 才升級，說不出就是原本那份 ANSI 16。進畫面之後再換色盤等於
    // 同一次執行裡畫面前後不一致
    theme::init_from_env();
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
    // P5.3：兩行列第二行的摘要快取（首行永不失效、events.log 檔長變更才重讀）
    let mut sums = Summaries::default();
    sums.sync(paths, &model);
    // liveness 起始為 unknown，等 worker 第一則回報——不在 UI thread 上查
    let mut live = LiveIndex::unknown();
    // blocker 軸同樣起始 unknown：unknown MUST NOT 顯示成「沒有 blocker」（§5）
    let mut blockers = BlockerIndex::unknown();
    // P5.4：blocker 框的畫面內容（只在記憶體、降旗即清，見 `model::Snippets`）
    let mut snippets = Snippets::default();
    // screen-matcher 來源的 blocker 需連續命中才升旗（§4 去抖）；狀態跨輪，
    // 故活在主迴圈而不是每輪重建的 BlockerIndex 裡
    let mut debounce = BlockerDebounce::new();
    let mut app = App::new();
    // **嘗試**時間戳（決定何時再輪詢一次）
    let mut last_disk = Instant::now();
    let mut last_live = Instant::now();
    // **成功**時間戳（決定畫面上那份資料有多新，P4.6 切片 C）。兩者必須分開：
    // 用嘗試時間算 freshness 的話，worker 每 2s 發一次查詢就足以讓畫面永遠
    // 顯示「剛更新」——即使那些查詢一次都沒回來過。
    //
    // disk 軸沒有失敗訊號可用（`Model::load` 讀不到就是空快照，不回錯），所以
    // 它量的是「上次真的**完成**一輪重讀」——抓得到迴圈被拖住，抓不到部分讀
    // 失敗。這個限制寫在這裡，畫面不假裝它抓得到。
    let mut stamps = Stamps::new(Instant::now());
    // 同時只讓一個 liveness 請求在途：worker 卡住時不得堆積無界佇列
    let mut live_inflight = true;

    // 兩軸的 stale 降級都落在同一份 unknown 上（借用，不每幀複製 HashMap）
    let unknown_live = LiveIndex::unknown();
    let unknown_blockers = BlockerIndex::unknown();

    // stale 降級是**邊緣**觸發 triage 的第三個來源：畫面上的兩個排序鍵在
    // 降級／回復的那一幀整批換了一份事實
    let mut degraded_prev = false;

    loop {
        let fresh = stamps.freshness(Instant::now());
        // **stale＝降級為 unknown，不是繼續畫舊的**（§4）：背景 worker 卡死時
        // 舊死活／舊 blocker 冒充新鮮，人會據此下判斷（例如以為某個 pane 還
        // 活著而不去看它）。unknown 說的是「現在不知道」，那是真的
        let degraded = degrade_on_stale(fresh, &mut debounce);
        let (live_view, blockers_view) = if degraded {
            (&unknown_live, &unknown_blockers)
        } else {
            (&live, &blockers)
        };
        // 只在**翻面那一幀**重排：stale 期間全體降回中性序（unknown 不浮頂，
        // 見 `worker_severity`），回復時再依真資料浮一次。每幀重排會讓
        // selection 追著排序跑
        if degraded != degraded_prev {
            degraded_prev = degraded;
            retriage(&mut model, live_view, blockers_view, &mut app);
            // 框內容跟著同一份顯示層索引走（跨廠複核 2026-08-05 finding 4）：
            // 整層降級為 unknown 的那一幀，畫面說的是「現在不知道」，一份舊
            // 框留在記憶體裡等回復就是替一個沒有證據的狀態留著證物。
            // `apply` 以顯示層索引 retain，unknown 下等於全清
            snippets.apply(Default::default(), blockers_view);
        }
        let mut frame = ratatui::layout::Rect::default();
        terminal
            .draw(|f| {
                frame = f.area();
                view::render(
                    f,
                    &model,
                    live_view,
                    blockers_view,
                    &app,
                    fresh,
                    &sums,
                    &snippets,
                    ab_core::time::now_epoch(),
                );
            })
            .map_err(|e| Error::new(format!("draw failed: {e}")))?;
        // 一頁多長只有版面知道：量到之後回填給狀態機（PgUp／PgDn 用）
        // footer 的額外行（filter 提示列／copy-mode banner）也吃版面：一頁
        // 多長要與 render 算的是同一份，否則翻頁會差一行
        app.pages = view::panel_heights(
            frame,
            app.warnings.len(),
            view::footer_extra_rows(&model, &app, blockers_view),
        );
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
                Msg::Live(l, b, s) => {
                    // **只有查得到東西才算成功，而且算的是快照的觀測時間**：
                    // bounded 查詢逾時／tmux 不在時 worker 回的是整層降級的
                    // unknown（那一輪沒有新資料）；查詢卡了十幾秒才回來的那一
                    // 輪則帶著一份早就過期的快照——兩者都不得刷新 freshness，
                    // 否則「查詢愈慢，畫面看起來愈新」（切片 C 核心修正＋F1）
                    stamps.note_live(&l);
                    apply_live(
                        &mut live,
                        &mut blockers,
                        &mut debounce,
                        &mut snippets,
                        l,
                        b,
                        s,
                    );
                    // triage 的資料事件邊緣之二：兩個排序鍵（blocker／死活）
                    // 都剛換過一份事實
                    retriage(&mut model, &live, &blockers, &mut app);
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
                    refresh_disk(
                        paths,
                        &mut model,
                        &mut sums,
                        &mut app,
                        shown_live(degraded, &live, &unknown_live),
                        shown_blockers(degraded, &blockers, &unknown_blockers),
                        &mut last_disk,
                        &mut stamps,
                    );
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
                    refresh_disk(
                        paths,
                        &mut model,
                        &mut sums,
                        &mut app,
                        shown_live(degraded, &live, &unknown_live),
                        shown_blockers(degraded, &blockers, &unknown_blockers),
                        &mut last_disk,
                        &mut stamps,
                    );
                }
                // `L` 的回信。判斷（放閘、比對目標、丟棄晚到結果）全在
                // `app::peek_apply` 這個純函式裡；run loop 的責任只有一件：
                // **先做一次權威重讀再比對**（跨廠複核 M2）
                Msg::Peek { target, res } => {
                    peek_arrival(&mut app, &mut model, || Model::load(paths), &target, res);
                    // peek_arrival 內部已做權威重讀；摘要快取跟上同一份快照
                    sums.sync(paths, &model);
                    last_disk = Instant::now();
                    stamps.note_disk(last_disk);
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
                        // 摘要頁與 dashboard 看的必須是同一份快照——包含
                        // stale 降級：畫面上標了 unknown、`i` 卻還印著三十秒前
                        // 的 live，那是同一個世代說兩種話
                        let stale = stamps.freshness(Instant::now()).tmux_stale();
                        let (lv, bl) = if stale {
                            (&unknown_live, &unknown_blockers)
                        } else {
                            (&live, &blockers)
                        };
                        app.info = Some(action::info_page(&model, lv, bl, &name));
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
                    // `L`：一次性 thread 上跑一次三重有界的 capture。**不搭常駐
                    // worker**——它同時負責 liveness 輪詢，一次卡住的 capture
                    // 會讓兩軸一起停擺（同 evict 的理由）
                    Effect::Peek { target } => {
                        start_peek(&mut app, worker, target);
                    }
                    Effect::None => {}
                }
            }
        }

        if last_disk.elapsed() >= DISK_POLL {
            // 這一輪真的完成了一次重讀＝disk 軸的「上次成功」（stamps 語意）
            refresh_disk(
                        paths,
                        &mut model,
                        &mut sums,
                        &mut app,
                        shown_live(degraded, &live, &unknown_live),
                        shown_blockers(degraded, &blockers, &unknown_blockers),
                        &mut last_disk,
                        &mut stamps,
                    );
        }
        if !live_inflight && last_live.elapsed() >= LIVE_POLL {
            live_inflight = worker.send(Req::Live);
            last_live = Instant::now();
        }
    }
}

/// 磁碟軸的一次完整刷新（P5.3 收攏四處手抄）：權威重讀 → 摘要快取同步 →
/// triage 重排 → selection 跟人不跟位 → 兩個時間戳。順序不可換——sums 讀的
/// 集合來自新 model，triage 排的是新 model 的組，relocate 讀的列來自重排後
/// 的列序。
#[allow(clippy::too_many_arguments)]
fn refresh_disk(
    paths: &Paths,
    model: &mut Model,
    sums: &mut Summaries,
    app: &mut App,
    live: &LiveIndex,
    blockers: &BlockerIndex,
    last_disk: &mut Instant,
    stamps: &mut Stamps,
) {
    *model = Model::load(paths);
    sums.sync(paths, model);
    retriage(model, live, blockers, app);
    *last_disk = Instant::now();
    stamps.note_disk(*last_disk);
}

/// 「畫面現在採信的那一份」死活／blocker（跨廠複核 2026-08-05 finding 5）。
///
/// triage 的排序鍵 MUST 與同一幀畫出來的兩軸同源。stale 期間畫面顯示 unknown
/// 卻仍依快取中的舊 Prompt／Dead 浮頂，等於排序在替一份畫面上已經撤回的事實
/// 背書——那正是 §4「stale＝降級為 unknown，不是繼續畫舊的」要擋的事。
fn shown_live<'a>(degraded: bool, live: &'a LiveIndex, unknown: &'a LiveIndex) -> &'a LiveIndex {
    if degraded { unknown } else { live }
}

fn shown_blockers<'a>(
    degraded: bool,
    blockers: &'a BlockerIndex,
    unknown: &'a BlockerIndex,
) -> &'a BlockerIndex {
    if degraded { unknown } else { blockers }
}

/// triage 的**唯一**入口：重排 → `relocate`。
///
/// 兩者綁在一起是硬條件——組序一動，同一個 `row_idx` 就指向別人了。分開呼叫
/// 的版本會在「剛好排序有變的那一幀」把選取靜默移到另一個 worker 身上，而
/// `x`／`e` 的目標就是選取（§5）。
fn retriage(model: &mut Model, live: &LiveIndex, blockers: &BlockerIndex, app: &mut App) {
    model::apply_triage(model, live, blockers);
    app.relocate(model);
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

/// 晚到的預覽結果落地：**先做一次權威重讀，再比對目標**（跨廠複核 M2）。
///
/// 為什麼不能直接拿記憶體裡那份 model 比：磁碟軸每 500ms 才重讀一次，回信落在
/// 兩次 poll 之間時，`model` 最舊可以是 500ms 前的。具體會出事的次序是——
/// disk poll 剛結束 → w1 respawn（registry 的 pane／`spawn_tag` 都換了）→ 舊
/// capture 的回信到了 → 對**舊** model 比對「還是同一列」→ 舊世代的畫面內容
/// 就這樣貼上新一代的列。而那正是 gate (d) 指名要擋掉的情境。
///
/// loader 由呼叫端注入，這一條才驗得到（真 run loop 傳 `Model::load`）。
fn peek_arrival(
    app: &mut App,
    model: &mut Model,
    load: impl FnOnce() -> Model,
    target: &app::PeekTarget,
    res: Option<ab_core::tmux::TailCapture>,
) {
    *model = load();
    app.relocate(model);
    app::peek_apply(app, model, target, res);
}

/// 尾行預覽的三個界。**只從 `ab_core::config` 讀**——那是唯一一份定義，UI 端
/// 不再截第二次（切片 D 契約）。
fn peek_bounds() -> ab_core::tmux::TailBounds {
    ab_core::tmux::TailBounds {
        history_lines: ab_core::config::TAIL_HISTORY_LINES,
        max_bytes: ab_core::config::TAIL_MAX_BYTES,
        timeout: ab_core::config::TAIL_TIMEOUT,
    }
}

/// 一次性 thread 上真正做的那一件事（與 thread 的起法分開，才注得進假件）。
///
/// panic 也回一則終局：閘由回信放開，沒有回信就永遠按不動下一次 `L`。
fn peek_once(tmux: &dyn TmuxClient, target: app::PeekTarget) -> Msg {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tmux.capture_pane_tail(&target.pane, peek_bounds())
    }))
    .unwrap_or(None);
    Msg::Peek { target, res }
}

/// `L` 的執行段：in-flight 閘 → 一次性 thread → 回信（P4.7 切片 D）。
///
/// UI thread 在這裡只起一條 thread，capture 本身連同它的三個界都在那一條上；
/// 界的值只從 `ab_core::config` 讀一次，UI 端不再截第二次。
fn start_peek(app: &mut App, worker: &worker::Handle, target: app::PeekTarget) {
    let name = target.name.clone();
    let req_target = target.clone();
    let started = worker.spawn_oneshot(move |tx| {
        let _ = tx.send(peek_once(&SubprocessTmux, req_target));
    });
    if started {
        app.peek_inflight = Some(target);
        app.message = format!("preview of '{name}' in progress…");
    } else {
        // thread 起不來：當場給終態，別讓閘卡在一個不會回來的請求上
        app.peek_inflight = None;
        app.message =
            format!("preview of '{name}' could not start (background worker failed to spawn)");
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
        // 翻頁／到頂到底（P4.6 切片 C）：新增鍵，既有鍵語意一個都不動
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        // filter 輸入模式的退格（P4.7 切片 C）。命令模式下 `dispatch_key`
        // 沒有對應分支，等同無效鍵
        KeyCode::Backspace => Some(Key::Backspace),
        _ => None,
    }
}

/// 兩軸的資料年紀時間戳（P4.6 切片 C）。
///
/// 與輪詢用的「上次嘗試」時間戳分開，而且抽成型別而不是內嵌在主迴圈裡：
/// 內嵌的話，把 `note_live` 改成無條件刷新（＝退回嘗試時間戳）沒有任何測試
/// 會紅——而那正是這次要修掉的缺陷本身。
struct Stamps {
    /// 上次**完成**一輪磁碟掃描。不是 `Option`：`Stamps` 是在第一次
    /// `Model::load` 之後才建的，那一輪掃描確實完成過。
    disk: Instant,
    /// 上次**成功**的 tmux 查詢輪的**觀測時間**（不是收信時間，審查 F1）。
    /// `None`＝至今沒有任何成功樣本（審查 F2）——啟動時間 MUST NOT 被拿來
    /// 充當這一格。
    tmux: Option<Instant>,
}

impl Stamps {
    fn new(now: Instant) -> Self {
        Stamps {
            disk: now,
            tmux: None,
        }
    }

    fn freshness(&self, now: Instant) -> model::Freshness {
        model::Freshness {
            disk: now.saturating_duration_since(self.disk),
            tmux: self.tmux.map(|t| now.saturating_duration_since(t)),
        }
    }

    /// 完成一輪磁碟掃描。
    fn note_disk(&mut self, now: Instant) {
        self.disk = now;
    }

    /// 收到一則 liveness 回報。
    ///
    /// **只有兩個必要子查詢都成功的那一輪才算數**，而且算的是那份快照的
    /// **觀測時間**，不是這則訊息抵達 UI 的時間（`LiveIndex::success_at`）。
    /// 用收信時間的話，一輪查詢卡到 15 秒才回來，會把 15 秒前的 pane 快照
    /// 重新標成「剛更新」再冒充新鮮 10 秒——查詢愈慢，畫面看起來愈新。
    ///
    /// 取 `max`：晚到的舊 round MUST NOT 把較新的樣本往回推。至於「晚到且
    /// 已超門檻」的那一輪，寫進去之後 age 立刻就 ≥ `TMUX_STALE`，於是照樣
    /// 是 stale／unknown——這裡不必再攔一次。
    fn note_live(&mut self, l: &LiveIndex) {
        let Some(observed) = l.success_at() else {
            return;
        };
        if self.tmux.is_none_or(|t| observed > t) {
            self.tmux = Some(observed);
        }
    }
}

/// stale 時的降級判定（審查 F7）：**回傳「要不要降級」，順手作廢去抖連勝**。
///
/// 兩件事綁在同一個函式裡是刻意的：降級與清 streak 必須同時發生，分開寫
/// 就會有一條路徑忘記清（症狀是停擺半分鐘後的第一則回報立刻升旗，而畫面
/// 上看起來完全合理）。抽成函式也讓這條線可被測試殺掉——內嵌在主迴圈裡的
/// 話，把 `reset()` 刪掉全套照樣綠。
fn degrade_on_stale(fresh: model::Freshness, debounce: &mut BlockerDebounce) -> bool {
    let stale = fresh.tmux_stale();
    if stale {
        debounce.reset();
    }
    stale
}

/// 一則 `Msg::Live` 落進 read model 的**唯一路徑**。
///
/// 抽成函式而不是內嵌在主迴圈裡，是為了讓「blocker 有沒有真的過去抖」可被
/// 測試殺掉：內嵌時把 `debounce.apply(b)` 改回 `b` 全套照樣綠——去抖等於裸奔。
/// liveness 直接覆蓋（它沒有單幀誤判面），blocker MUST 過 `debounce`。
///
/// snippet 落地**排在去抖之後**（P5.4）：它吃的是顯示層那份索引，去抖期間
/// 畫面說「沒有可見 blocker」時就不該有框內容陪著。
fn apply_live(
    live: &mut LiveIndex,
    blockers: &mut BlockerIndex,
    debounce: &mut BlockerDebounce,
    snippets: &mut Snippets,
    l: LiveIndex,
    b: BlockerIndex,
    s: std::collections::HashMap<String, Vec<String>>,
) {
    *live = l;
    *blockers = debounce.apply(b);
    snippets.apply(s, blockers);
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

    /// **wiring 測試**：畫面採信哪一份兩軸，triage 與 snippet 就吃哪一份
    /// （跨廠複核 2026-08-05 finding 4／5）。
    ///
    /// 這兩條原本各差一步：stale 期間 `refresh_disk` 仍拿快取中的舊 Prompt／
    /// Dead 去排序（畫面已改說 unknown），而 snippet 要等下一則 `Msg::Live`
    /// 才被 prune。差別在畫面上是「排名沒有事實支撐」與「舊框留在記憶體」。
    #[test]
    fn the_degraded_view_is_what_triage_and_snippets_consume() {
        let unknown_live = LiveIndex::unknown();
        let unknown_blockers = BlockerIndex::unknown();
        let live = LiveIndex {
            panes: Some(HashMap::from([(
                "%1".to_string(),
                vec![("s".to_string(), "@1".to_string())],
            )])),
            ..LiveIndex::unknown()
        };
        let blockers = prompt_round();

        // 未降級：拿到的是真資料
        assert_eq!(
            shown_blockers(false, &blockers, &unknown_blockers).get("%1"),
            Blocker::Prompt
        );
        assert!(shown_live(false, &live, &unknown_live).panes.is_some());
        // 降級：拿到的是 unknown——與同一幀 render 吃的是同一份
        assert_eq!(
            shown_blockers(true, &blockers, &unknown_blockers).get("%1"),
            Blocker::Unknown
        );
        assert!(shown_live(true, &live, &unknown_live).panes.is_none());

        // snippet：以顯示層索引 retain，unknown 那一幀等於全清
        let mut snips = Snippets::default();
        snips.apply(
            HashMap::from([("%1".to_string(), vec!["Do you want to proceed?".to_string()])]),
            &blockers,
        );
        assert!(snips.get("%1").is_some(), "前提：升旗時留著");
        snips.apply(Default::default(), shown_blockers(true, &blockers, &unknown_blockers));
        assert!(
            snips.get("%1").is_none(),
            "降級那一幀 MUST 清掉舊框（不得等下一則 Msg::Live）"
        );
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
            &mut Snippets::default(),
            LiveIndex::unknown(),
            prompt_round(),
            Default::default(),
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
            &mut Snippets::default(),
            LiveIndex::unknown(),
            prompt_round(),
            Default::default(),
            );
        assert_eq!(
            view::blocker_mark(blockers.get("%1")),
            "  ⛔blocked",
            "連續第二輪 MUST 升旗"
        );
    }

    /// 一輪**成功**的 liveness，觀測時間由呼叫端指定。
    fn round_at(at: Instant) -> LiveIndex {
        LiveIndex {
            panes: Some(HashMap::new()),
            windows: Some(HashMap::new()),
            panes_at: Some(at),
        }
    }

    /// **freshness 的 wiring 測試**（P4.6 切片 C）：tmux 軸的「上次成功」
    /// MUST 只被**查得到東西**的那一輪刷新。
    ///
    /// 沒有這條，把 `note_live` 改回無條件刷新（＝退回嘗試時間戳）全套照樣
    /// 綠——而那正是這次要修掉的缺陷：查詢一路逾時，畫面卻一路顯示「剛更新」。
    #[test]
    fn only_a_successful_tmux_round_refreshes_the_freshness_stamp() {
        let t0 = Instant::now();
        let mut s = Stamps::new(t0);
        let later = t0 + Duration::from_secs(30);

        // 降級（unknown）的那一輪：時間戳不動 → 30 秒後照樣是 stale
        s.note_live(&LiveIndex::unknown());
        let f = s.freshness(later);
        assert!(
            f.tmux_stale(),
            "unknown 回報不算成功（實際 age {:?}）",
            f.tmux
        );

        // 真的查到東西：時間戳前進 → 不再 stale
        s.note_live(&round_at(later));
        assert!(!s.freshness(later).tmux_stale(), "成功回報 MUST 刷新");

        // disk 軸各自獨立（tmux 成功不會順手把 disk 說成新鮮）
        assert!(s.freshness(later).disk >= Duration::from_secs(30));
    }

    /// **F1 regression：晚到的成功 round MUST NOT 把過期快照重新標新鮮。**
    ///
    /// 上面那條只驗「結果型別」（unknown vs 查得到），round **延遲**它一點都
    /// 抓不到：把 stamp 改回收信時間，上面那條照樣綠。這條驗的是時間軸——
    /// 一輪在 t0 取到 pane 快照、卡在後續 bounded 查詢上，直到 t0+15s 才送
    /// 抵 UI。那份快照當下已經 15 秒大，畫面 MUST 維持 unknown。
    #[test]
    fn a_late_arriving_round_cannot_relabel_an_expired_snapshot_as_fresh() {
        let t0 = Instant::now();
        let mut s = Stamps::new(t0);
        // list-panes 在 t0 就到手，整輪卻拖到 t0+15s 才回到 UI
        let arrived = t0 + Duration::from_secs(15);
        s.note_live(&round_at(t0));

        let f = s.freshness(arrived);
        assert_eq!(
            f.tmux,
            Some(Duration::from_secs(15)),
            "age MUST 從**觀測時間**起算，不是從收信時間"
        );
        assert!(
            f.tmux_stale(),
            "晚到且已超門檻的 round MUST 維持 unknown，不得洗新"
        );

        // 對照組：同一輪若沒有延遲（觀測時間就是現在），當然是新鮮的
        let mut s2 = Stamps::new(t0);
        s2.note_live(&round_at(arrived));
        assert!(
            !s2.freshness(arrived).tmux_stale(),
            "正向對照：不延遲就新鮮"
        );

        // 晚到的**舊** round 也不得把較新的樣本往回推
        let mut s3 = Stamps::new(t0);
        s3.note_live(&round_at(arrived));
        s3.note_live(&round_at(t0));
        assert!(
            !s3.freshness(arrived).tmux_stale(),
            "亂序抵達 MUST NOT 讓時間戳倒退"
        );
    }

    /// **F2：部分成功不得替整軸背書；第一輪回來前不得有時間戳。**
    #[test]
    fn a_partial_tmux_round_is_not_a_successful_sample() {
        let t0 = Instant::now();
        let s = Stamps::new(t0);
        // 啟動時尚無任何成功樣本：age 是 None（不是「距啟動 0 秒」）
        assert_eq!(s.freshness(t0).tmux, None, "第一輪回來前 MUST 沒有時間戳");
        assert!(s.freshness(t0).tmux_stale(), "沒有樣本＝unknown＝stale");

        // list-panes 成功、list-windows 失敗：不算一輪成功
        let mut s2 = Stamps::new(t0);
        s2.note_live(&LiveIndex {
            panes: Some(HashMap::new()),
            windows: None,
            panes_at: Some(t0),
        });
        assert_eq!(
            s2.freshness(t0).tmux,
            None,
            "只有 panes 成功 MUST NOT 替 windows 背書"
        );

        // 反向：兩者都成功但**空集合**仍是合法觀測（機器上真的沒有 pane）
        let mut s3 = Stamps::new(t0);
        s3.note_live(&round_at(t0));
        assert_eq!(s3.freshness(t0).tmux, Some(Duration::ZERO));
    }

    /// **F7：停擺缺口 MUST 作廢去抖連勝。**
    ///
    /// 「連續兩輪」隱含相鄰兩輪的時間鄰近性（2s 一輪、最壞未滿 4s）。停擺前
    /// 命中一次的 pane，若 streak 跨過缺口留著，30 秒停擺之後的**第一則**
    /// 回報就會立刻升旗——那兩次命中之間隔了半分鐘。
    #[test]
    fn a_stale_gap_voids_the_blocker_debounce_streak() {
        let mut live = LiveIndex::unknown();
        let mut blockers = BlockerIndex::unknown();
        let mut debounce = BlockerDebounce::new();

        // 停擺前：命中一次（streak=1，還沒升旗）
        apply_live(
            &mut live,
            &mut blockers,
            &mut debounce,
            &mut Snippets::default(),
            LiveIndex::unknown(),
            prompt_round(),
            Default::default(),
            );
        assert_eq!(view::blocker_mark(blockers.get("%1")), "");

        // 停擺 30 秒：畫面降級為 unknown，連勝同時作廢
        let stale = model::Freshness {
            disk: Duration::ZERO,
            tmux: Some(Duration::from_secs(30)),
        };
        assert!(degrade_on_stale(stale, &mut debounce), "前提：這是 stale");

        // 恢復後的第一則回報：MUST NOT 立刻升旗（它是缺口後的**第一**輪）
        apply_live(
            &mut live,
            &mut blockers,
            &mut debounce,
            &mut Snippets::default(),
            LiveIndex::unknown(),
            prompt_round(),
            Default::default(),
            );
        assert_eq!(
            view::blocker_mark(blockers.get("%1")),
            "",
            "停擺後的第一則回報升旗＝連續兩輪的時間鄰近性假設已經破了"
        );

        // 再連上一輪才算數（去抖本身沒有被關掉）
        apply_live(
            &mut live,
            &mut blockers,
            &mut debounce,
            &mut Snippets::default(),
            LiveIndex::unknown(),
            prompt_round(),
            Default::default(),
            );
        assert_eq!(view::blocker_mark(blockers.get("%1")), "  ⛔blocked");

        // 沒 stale 時不得清（否則等於把去抖永久關掉）
        let ok = model::Freshness {
            disk: Duration::ZERO,
            tmux: Some(Duration::ZERO),
        };
        assert!(!degrade_on_stale(ok, &mut debounce));
    }

    // ── P4.7 切片 D：`L` 尾行預覽的 wiring ────────────────────────────

    fn peek_target(pane: &str, name: &str) -> app::PeekTarget {
        app::PeekTarget {
            pane: pane.to_string(),
            name: name.to_string(),
            key: model::RowKey::Worker {
                name: name.to_string(),
                spawn_tag: "t-gen1".to_string(),
            },
        }
    }

    /// 假 tmux：記下 `capture_pane_tail` 收到的 `(pane, bounds)`，其餘方法都是
    /// fail-closed 的空殼（這一條路徑用不到）。
    struct TailTmux {
        seen: std::sync::Mutex<Vec<(String, ab_core::tmux::TailBounds)>>,
        /// 每次呼叫先睡這麼久（模擬卡住的 tmux）
        delay: Duration,
    }
    impl TmuxClient for TailTmux {
        fn exec(&self, _a: &[&str]) -> Option<ab_core::tmux::TmuxOutput> {
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
            None
        }
        fn pane_in_mode(&self, _p: &str) -> Option<bool> {
            None
        }
        fn send_keys(&self, _p: &str, _k: &str) -> bool {
            false
        }
        fn capture_pane_tail(
            &self,
            pane: &str,
            bounds: ab_core::tmux::TailBounds,
        ) -> Option<ab_core::tmux::TailCapture> {
            self.seen.lock().unwrap().push((pane.to_string(), bounds));
            std::thread::sleep(self.delay);
            Some(ab_core::tmux::TailCapture {
                text: "tail\n".to_string(),
                truncated: false,
            })
        }
    }

    fn tail_tmux(delay: Duration) -> TailTmux {
        TailTmux {
            seen: std::sync::Mutex::new(Vec::new()),
            delay,
        }
    }

    /// 只有一個 worker 的 read model（世代由呼叫端指定）。
    fn one_worker(tag: &str) -> Model {
        let mut m = Model {
            groups: Vec::new(),
            workers: vec![ab_core::registry::AgentSnapshot {
                name: "w1".into(),
                pane: "%5".into(),
                runtime: "codex".into(),
                owner: "it:@1".into(),
                ready: "ready".into(),
                spawn_tag: tag.into(),
                registered_at: "2026-07-31T00:00:00Z".into(),
                spawned_at: String::new(),
                spawned: true,
                corrupt: false,
                lineage_root: None,
                parent_agent: None,
            }],
            tasks: Vec::new(),
            recent: Vec::new(),
            recent_truncated: false,
        };
        m.groups = model::group_by_lineage(&m.workers);
        m
    }

    /// **晚到的結果 MUST 對權威資料比對**（跨廠複核 M2）。
    ///
    /// 重現真 run loop 的次序：disk poll 之後 w1 respawn → 舊 capture 的回信先
    ///到 → 下一次 periodic poll 還沒發生。此時記憶體裡的 model 仍是舊世代，
    /// 直接拿它比對會判成「還是同一列」而把舊內容貼上去。
    #[test]
    fn a_peek_result_is_matched_against_a_freshly_loaded_model() {
        let mut model = one_worker("t-gen1");
        let mut app = App::new();
        let target = app::peek_target(&app, &model).expect("有 pane 的 worker 列");
        app.peek_inflight = Some(target.clone());

        // 磁碟上已經換代，記憶體裡的 `model` 還是舊的那一份
        let cap = ab_core::tmux::TailCapture {
            text: "stale\n".to_string(),
            truncated: false,
        };
        peek_arrival(
            &mut app,
            &mut model,
            || one_worker("t-gen2"),
            &target,
            Some(cap.clone()),
        );
        assert!(
            app.peek.is_none(),
            "換代之後 MUST NOT 把舊世代的畫面貼上新一代的列"
        );
        assert!(app.message.contains("discarded"), "{}", app.message);
        assert_eq!(
            model.workers[0].spawn_tag, "t-gen2",
            "重讀進來的 MUST 是權威那一份"
        );

        // 對照組：世代沒變 → 照常貼上（重讀不是把功能關掉）
        let mut app = App::new();
        let target = app::peek_target(&app, &model).expect("仍有選中列");
        app.peek_inflight = Some(target.clone());
        peek_arrival(
            &mut app,
            &mut model,
            || one_worker("t-gen2"),
            &target,
            Some(cap),
        );
        assert!(app.peek.is_some(), "同一代 MUST 正常開出預覽");
    }

    /// 取得路徑收到的界**就是 `config` 那一份**（UI 端不另立一套），而且回信
    /// 原樣帶回目標——晚到比對靠的就是它。
    #[test]
    fn a_peek_request_carries_the_config_bounds_and_its_target() {
        let tmux = tail_tmux(Duration::ZERO);
        let t = peek_target("%5", "w1");
        let Msg::Peek { target, res } = peek_once(&tmux, t.clone()) else {
            panic!("MUST 回 Msg::Peek");
        };
        assert_eq!(target, t, "回信 MUST 原樣帶回目標");
        assert_eq!(res.expect("有結果").text, "tail\n");

        let seen = tmux.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "一次按鍵只查一次");
        assert_eq!(seen[0].0, "%5");
        assert_eq!(
            seen[0].1,
            ab_core::tmux::TailBounds {
                history_lines: ab_core::config::TAIL_HISTORY_LINES,
                max_bytes: ab_core::config::TAIL_MAX_BYTES,
                timeout: ab_core::config::TAIL_TIMEOUT,
            },
            "三個界 MUST 全部來自 config 那一份定義"
        );
    }

    /// **UI thread 不因 tmux 卡住而凍結**（§4 硬條款）：capture 跑在一次性
    /// thread 上，UI 端的 drain 是 non-blocking——查詢還沒回來時它立刻回空手，
    /// 不是等在那裡。
    #[test]
    fn a_hanging_peek_does_not_block_the_ui_drain() {
        let (h, _req_rx) = worker::Handle::detached();
        let t = peek_target("%5", "w1");
        assert!(h.spawn_oneshot(move |tx| {
            let tmux = tail_tmux(Duration::from_millis(400));
            let _ = tx.send(peek_once(&tmux, t));
        }));
        // 這一輪 drain 發生在查詢還卡著的時候：MUST 立刻回來
        let started = Instant::now();
        let first = h.try_recv();
        let drain = started.elapsed();
        assert!(first.is_none(), "還沒回來的請求 MUST NOT 有訊息");
        assert!(
            drain < Duration::from_millis(100),
            "drain MUST 不等查詢（實測 {drain:?}）"
        );

        // 查詢完成後訊息才進來（證明卡住的是工人，不是 UI）
        let deadline = Instant::now() + Duration::from_secs(5);
        let got = loop {
            if let Some(m) = h.try_recv() {
                break Some(m);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(matches!(got, Some(Msg::Peek { .. })), "終局 MUST 送達");
    }
}

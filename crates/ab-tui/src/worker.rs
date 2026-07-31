//! tmux／mutation 的背景工人（tui-design.md §4 bounded-read 硬條款的消費端
//! 防線）。
//!
//! 為什麼要有這一層：`ab-core` 的 tmux 呼叫雖然 bounded，`AGENT_BRIDGE_TMUX_TIMEOUT`
//! 預設 5 秒、`0` 等同不設限——只要 UI thread 直接呼叫，一輪 liveness 就能把
//! 畫面與鍵盤凍住數秒甚至永久（審查 F1）。因此**所有** tmux 往來（含使用者
//! 觸發的 focus 與 cancel）都下放到一條 `std::thread`，UI thread 只
//! non-blocking 收信。工人卡住的終態是「該欄 stale ＋ 動作顯示進行中」，
//! 不是凍結。
//!
//! 依賴紀律（§6）：只用 std 的 thread＋mpsc，不引入 async runtime。

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use ab_core::error::Result;
use ab_core::paths::Paths;
use ab_core::registry;
use ab_core::task::{self, CancelOutcome};
use ab_core::tmux::TmuxClient;

use crate::model::LiveIndex;

/// UI → worker 的請求。
pub enum Req {
    /// 重查 tmux liveness（節流由 UI 端決定）
    Live,
    /// 執行 focus（§2 語意，含跨 session 的 switch-client）
    Focus { pane: String, label: String },
    /// 執行 cancel（正本在 `ab_core::task::cancel_task`）
    Cancel { id: String },
}

/// worker → UI 的訊息。
pub enum Msg {
    /// 啟動定位：呼叫者所在的 owner 標籤與 pane id（初始 owner 用，審查 F2）
    Origin {
        owner: Option<String>,
        pane: Option<String>,
    },
    Live(LiveIndex),
    Focus {
        label: String,
        pane: String,
        res: Result<()>,
    },
    Cancel {
        id: String,
        res: Result<CancelOutcome>,
    },
}

/// UI 端持有的把手。`Drop` 時關掉請求端，worker 做完手上那件事就自行結束
/// ——**不 join**：工人可能正卡在一次無界 tmux 呼叫上，join 等於把凍結搬到
/// 退出路徑。
pub struct Handle {
    tx: Sender<Req>,
    rx: Receiver<Msg>,
}

impl Handle {
    /// 送出請求。worker 已死（channel 關閉）＝降級成不做，不 panic：
    /// dashboard 少一次刷新可以，垮掉不行。
    pub fn send(&self, req: Req) -> bool {
        self.tx.send(req).is_ok()
    }

    /// non-blocking 取一則訊息（UI 每幀 drain）。
    pub fn try_recv(&self) -> Option<Msg> {
        match self.rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

/// 起一條 worker thread。開場先回報 origin 與第一輪 liveness，之後照請求做事。
pub fn spawn<T: TmuxClient + Send + 'static>(tmux: T, paths: Paths) -> Handle {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();

    thread::spawn(move || {
        let owner = current_owner(&tmux);
        let pane = current_pane(&tmux);
        // current session 只在 worker 內部用（focus 的 switch-client 目標）
        let session = owner
            .as_ref()
            .and_then(|o| o.rsplit_once(':').map(|(s, _)| s.to_string()));
        if msg_tx
            .send(Msg::Origin {
                owner,
                pane: pane.clone(),
            })
            .is_err()
        {
            return;
        }
        if msg_tx.send(Msg::Live(LiveIndex::query(&tmux))).is_err() {
            return;
        }
        while let Ok(req) = req_rx.recv() {
            let msg = match req {
                Req::Live => Msg::Live(LiveIndex::query(&tmux)),
                Req::Focus { pane, label } => {
                    let res = crate::action::focus(&tmux, &pane, session.as_deref());
                    Msg::Focus { label, pane, res }
                }
                Req::Cancel { id } => {
                    let res = task::cancel_task(&paths, &tmux, &id);
                    Msg::Cancel { id, res }
                }
            };
            if msg_tx.send(msg).is_err() {
                break;
            }
        }
    });

    Handle {
        tx: req_tx,
        rx: msg_rx,
    }
}

/// 呼叫者所在的 owner 標籤 `session:@window`。先走 `caller_owner`
/// （`TMUX_PANE` env ＋ display-message 覆核，與 spawn 的歸屬邏輯同一條）；
/// 該路徑要求 `TMUX`／`TMUX_PANE` 同時存在，缺一就退而直接問 tmux 當前
/// client 的定位（審查 F2 指定的兩段式）。
fn current_owner(tmux: &dyn TmuxClient) -> Option<String> {
    if let Some(o) = registry::caller_owner(tmux) {
        return Some(o);
    }
    let out = tmux
        .exec(&["display-message", "-p", "#{session_name}:#{window_id}"])?
        .ok_stdout()?;
    if out.contains(":@") { Some(out) } else { None }
}

/// 呼叫者所在的 pane id。`TMUX_PANE` 優先（不必開子行程），缺失才問 tmux。
/// 用途是「TUI 就開在某個 worker 的 pane 裡」時仍能反查其 owner。
fn current_pane(tmux: &dyn TmuxClient) -> Option<String> {
    if let Ok(p) = std::env::var("TMUX_PANE")
        && !p.is_empty()
    {
        return Some(p);
    }
    tmux.exec(&["display-message", "-p", "#{pane_id}"])?
        .ok_stdout()
}

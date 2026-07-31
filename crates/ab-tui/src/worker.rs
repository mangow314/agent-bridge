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
use ab_core::evict::EvictOutcome;
use ab_core::paths::Paths;
use ab_core::registry;
use ab_core::task::{self, CancelOutcome, ReadOutcome};
use ab_core::tmux::TmuxClient;

use crate::model::{BlockerIndex, LiveIndex};

/// UI → worker 的請求。
pub enum Req {
    /// 重查 tmux liveness（節流由 UI 端決定）
    Live,
    /// 執行 focus（§2 語意，含跨 session 的 switch-client）
    Focus { pane: String, label: String },
    /// 執行 cancel（正本在 `ab_core::task::cancel_task`）
    Cancel { id: String },
    /// 讀 task 全文（正本在 `ab_core::task::read_response`）。**必須在這裡跑**
    /// ——它會取 task 鎖，鎖被別人握著時 UI thread 上就是凍結
    Read { id: String },
    /// 複製證據到 tmux buffer（§3）
    Copy { payload: String },
}

/// worker → UI 的訊息。
pub enum Msg {
    /// 啟動定位：呼叫者所在的 owner 標籤與 pane id（初始 owner 用，審查 F2）
    Origin {
        owner: Option<String>,
        pane: Option<String>,
    },
    /// 一輪 tmux 查詢：死活（§4 liveness）＋ blocker 軸（§4 v1 matcher 契約）。
    /// 兩者同一輪回報：分兩則會讓畫面出現「死活已更新、blocker 還是上一輪」
    /// 的混搭狀態
    Live(LiveIndex, BlockerIndex),
    Focus {
        label: String,
        pane: String,
        res: Result<()>,
    },
    Cancel {
        id: String,
        res: Result<CancelOutcome>,
    },
    Read {
        id: String,
        res: Result<ReadOutcome>,
    },
    Copy {
        res: Result<()>,
    },
    /// evict 編排的進度行（core 的 `EvictEvent`，一次性 thread 串流回來）。
    /// `warn` 保留 core 的嚴重度：警告 MUST NOT 被後續進度／終局蓋掉
    /// （codex 複核 major #2）
    EvictProgress {
        name: String,
        line: String,
        warn: bool,
    },
    /// evict 編排的終局（一次性 thread 的最後一則）
    Evict {
        name: String,
        res: Result<EvictOutcome>,
    },
}

/// UI 端持有的把手。`Drop` 時關掉請求端，worker 做完手上那件事就自行結束
/// ——**不 join**：工人可能正卡在一次無界 tmux 呼叫上，join 等於把凍結搬到
/// 退出路徑。
pub struct Handle {
    tx: Sender<Req>,
    rx: Receiver<Msg>,
    /// 一次性 thread 用的回信端（`spawn_oneshot`）。UI 只有一個收信口，
    /// 長工與一次性工人都往這裡送。
    msg_tx: Sender<Msg>,
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

    /// 起一條**一次性** thread，結果送回同一個 mpsc。
    ///
    /// 為什麼 evict 不能搭常駐 worker 那條 thread：那條同時負責 liveness 輪詢，
    /// 而 evict 預設 `--timeout 300` 會在 await 段一路等下去——搭上去等於整整
    /// 五分鐘不再有 liveness 更新，且期間任何 focus／cancel／read 都排在它後面。
    /// 一次性 thread 各自獨立，UI thread 兩邊都只是 non-blocking 收信。
    ///
    /// **不 join**（同 `Handle` 的 `Drop` 註解）：工人可能正卡在 await 上。
    ///
    /// 回傳 `false`＝**thread 根本沒起來**（OS 資源耗盡）。用
    /// `thread::Builder::spawn` 而不是 `thread::spawn`：後者失敗時是 panic，
    /// 而且無條件回 `true` 會讓呼叫端的失敗分支變成死碼——畫面於是永遠停在
    /// 「進行中…」，in-flight 閘也永遠不會放開（codex 複核 major #3）。
    ///
    /// 工人自己 panic 的處置**不在這裡**：只有呼叫端知道該回哪一則終局訊息，
    /// 故由它以 `catch_unwind` 轉成 terminal error（見 `crate::start_evict`）。
    pub fn spawn_oneshot<F>(&self, f: F) -> bool
    where
        F: FnOnce(Sender<Msg>) + Send + 'static,
    {
        let tx = self.msg_tx.clone();
        thread::Builder::new()
            .name("ab-tui-oneshot".to_string())
            .spawn(move || f(tx))
            .is_ok()
    }

    /// 只有 channel、不起常駐 worker 的把手（測試用：驗一次性 thread 的訊息
    /// 確實回流到 UI 的收信口）。`Receiver<Req>` 一併回傳，否則 `send` 會因
    /// 對端已 drop 而失敗，測不到真實行為。
    #[cfg(test)]
    pub fn detached() -> (Handle, Receiver<Req>) {
        let (req_tx, req_rx) = mpsc::channel::<Req>();
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        (
            Handle {
                tx: req_tx,
                rx: msg_rx,
                msg_tx,
            },
            req_rx,
        )
    }
}

/// 起一條 worker thread。開場先回報 origin 與第一輪 liveness，之後照請求做事。
pub fn spawn<T: TmuxClient + Send + 'static>(tmux: T, paths: Paths) -> Handle {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let oneshot_tx = msg_tx.clone();

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
        if msg_tx.send(live_round(&tmux, &paths)).is_err() {
            return;
        }
        while let Ok(req) = req_rx.recv() {
            let msg = match req {
                Req::Live => live_round(&tmux, &paths),
                Req::Focus { pane, label } => {
                    let res = crate::action::focus(&tmux, &pane, session.as_deref());
                    Msg::Focus { label, pane, res }
                }
                Req::Cancel { id } => {
                    let res = task::cancel_task(&paths, &tmux, &id);
                    Msg::Cancel { id, res }
                }
                Req::Read { id } => {
                    let res = task::read_response(&paths, &id);
                    Msg::Read { id, res }
                }
                Req::Copy { payload } => {
                    let res = crate::action::copy(&crate::action::TmuxClipboard(&tmux), &payload);
                    Msg::Copy { res }
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
        msg_tx: oneshot_tx,
    }
}

/// 一輪 tmux 查詢：死活索引 ＋ blocker 索引。
///
/// blocker 只對 registry 快照裡的 pane 查（`snapshot` 讀的是小 JSON，不取鎖），
/// 每個 pane 兩次 bounded 呼叫（`pane_in_mode`＋`capture_pane`）。整輪跑在
/// 背景 worker 上，卡住的終態是「該欄 stale」，不是凍結（§4 硬條款）。
fn live_round(tmux: &dyn TmuxClient, paths: &Paths) -> Msg {
    let live = LiveIndex::query(tmux);
    let panes: Vec<String> = registry::snapshot(paths)
        .into_iter()
        .map(|w| w.pane)
        .filter(|p| !p.is_empty())
        .collect();
    Msg::Live(live, BlockerIndex::query(tmux, &panes))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次性 thread 的訊息 MUST 回流到 UI 的同一個收信口，且**與常駐 worker
    /// 那條 thread 無關**（evict 的 await 段一等 300s，搭上長工就是五分鐘沒有
    /// liveness）。這裡連常駐 worker 都沒起，訊息照樣收得到。
    #[test]
    fn oneshot_thread_messages_reach_the_ui_channel() {
        let (h, _req_rx) = Handle::detached();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        assert!(h.spawn_oneshot(move |tx| {
            tx.send(Msg::EvictProgress {
                name: "w1".into(),
                line: "evict：收尾任務已派出".into(),
                warn: false,
            })
            .unwrap();
            tx.send(Msg::Evict {
                name: "w1".into(),
                res: Err(ab_core::error::Error::new("boom")),
            })
            .unwrap();
            done_tx.send(()).unwrap();
        }));
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("一次性 thread 應完成");

        let mut got = Vec::new();
        while let Some(m) = h.try_recv() {
            got.push(m);
        }
        assert_eq!(got.len(), 2, "兩則訊息都要回到 UI 的收信口");
        assert!(matches!(got[0], Msg::EvictProgress { .. }));
        assert!(matches!(got[1], Msg::Evict { .. }));
    }
}

//! UI 狀態機（selection＋鍵位語意，tui-design.md §2／§3）。純狀態轉移，
//! 不碰 terminal 與副作用——副作用以 `Effect` 交回 run loop 執行。

use crate::model::{Model, Row, worker_rows};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Owners,
    Workers,
}

/// 與 crossterm 解耦的鍵表示：狀態機測試不需要 terminal 事件型別。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Tab,
    Enter,
    Esc,
    Down,
    Up,
}

/// 待 run loop 執行的副作用。
#[derive(PartialEq, Eq, Debug)]
pub enum Effect {
    None,
    Quit,
    /// focus 該 pane（§2 focus 語意由 action 層落地）
    Focus {
        pane: String,
        label: String,
    },
    /// 確認後的 cancel（id immutable，天然 CAS，§5）
    Cancel {
        id: String,
    },
}

pub struct App {
    pub panel: Panel,
    pub owner_idx: usize,
    pub row_idx: usize,
    /// `x` 的單確認框：`Some(task-id)`＝等待 y/n。綁 id 不綁列位——確認期間
    /// 列表刷新也不會改變取消目標（§5 CAS）。
    pub confirm: Option<String>,
    pub help: bool,
    pub message: String,
    /// 呼叫者定位（worker 開場回報）：初始 owner 以此為根（§2「以 current
    /// owner 為根」）。晚到才落地，故每次磁碟重讀都重試一次。
    pub origin_owner: Option<String>,
    pub origin_pane: Option<String>,
    /// 使用者是否已自行動過 OWNERS 欄：動過之後 origin 不得再搶 selection
    /// （晚到的定位把人拉走比選錯還糟）。
    pub owner_touched: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            panel: Panel::Workers,
            owner_idx: 0,
            row_idx: 0,
            confirm: None,
            help: false,
            message: String::new(),
            origin_owner: None,
            origin_pane: None,
            owner_touched: false,
        }
    }

    /// 把 selection 落到 current owner（§2）。人動過 OWNERS 欄之後就不再套用；
    /// 找不到對應 owner 時**維持字典序第 0 筆**（不猜、不新增假列）。
    pub fn apply_origin(&mut self, model: &Model) {
        if self.owner_touched {
            return;
        }
        if let Some(i) = origin_owner_idx(
            model,
            self.origin_owner.as_deref(),
            self.origin_pane.as_deref(),
        ) && i != self.owner_idx
        {
            self.owner_idx = i;
            self.row_idx = 0;
        }
    }

    /// 磁碟重讀後把 selection 夾回合法範圍（列數可能縮短）。
    pub fn clamp(&mut self, model: &Model) {
        if self.owner_idx >= model.owners.len() {
            self.owner_idx = model.owners.len().saturating_sub(1);
        }
        let n = self.rows(model).len();
        if self.row_idx >= n {
            self.row_idx = n.saturating_sub(1);
        }
    }

    pub fn selected_owner<'m>(&self, model: &'m Model) -> Option<&'m str> {
        model.owners.get(self.owner_idx).map(|s| s.as_str())
    }

    pub fn rows(&self, model: &Model) -> Vec<Row> {
        match self.selected_owner(model) {
            Some(owner) => worker_rows(model, owner),
            None => Vec::new(),
        }
    }

    pub fn selected_row(&self, model: &Model) -> Option<Row> {
        self.rows(model).get(self.row_idx).copied()
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// current owner 在 OWNERS 欄的索引（純函式，可單測）。兩段式反查：
/// 1. 呼叫者所在 window 的 owner 標籤 `session:@window` 直接對上一列
/// 2. 對不上時（例如 TUI 開在某個 worker 自己的 pane 裡）以 current pane
///    在 registry 快照中反查該 worker 的 owner
///
/// 都對不上→`None`，呼叫端維持既有 selection（字典序第 0 筆）。
pub fn origin_owner_idx(model: &Model, owner: Option<&str>, pane: Option<&str>) -> Option<usize> {
    if let Some(o) = owner
        && let Some(i) = model.owners.iter().position(|x| x == o)
    {
        return Some(i);
    }
    let p = pane?;
    if p.is_empty() {
        return None;
    }
    let w = model.workers.iter().find(|w| w.pane == p)?;
    let label = crate::model::owner_label(w);
    model.owners.iter().position(|x| *x == label)
}

/// 鍵位表（§3 中屬第一縱切的子集）。回傳待執行的副作用。
pub fn handle_key(app: &mut App, model: &Model, key: Key) -> Effect {
    // 確認框開著：只認 y／Enter（執行）與 n／Esc（放棄），其餘鍵一律吞掉
    // ——破壞性動作的模態下不得讓導航鍵改變 selection 造成誤解
    if let Some(id) = app.confirm.clone() {
        return match key {
            Key::Char('y') | Key::Enter => {
                app.confirm = None;
                Effect::Cancel { id }
            }
            Key::Char('n') | Key::Esc => {
                app.confirm = None;
                app.message = "已放棄 cancel".to_string();
                Effect::None
            }
            _ => Effect::None,
        };
    }
    if app.help {
        app.help = false; // 任意鍵關閉
        return Effect::None;
    }
    match key {
        Key::Char('q') => Effect::Quit,
        Key::Char('?') => {
            app.help = true;
            Effect::None
        }
        Key::Tab => {
            app.panel = match app.panel {
                Panel::Owners => Panel::Workers,
                Panel::Workers => Panel::Owners,
            };
            Effect::None
        }
        Key::Char('j') | Key::Down => {
            move_sel(app, model, 1);
            Effect::None
        }
        Key::Char('k') | Key::Up => {
            move_sel(app, model, -1);
            Effect::None
        }
        Key::Enter => match app.panel {
            Panel::Owners => {
                app.message = "Enter 僅對 WORKERS 欄的 worker／task 列有效".to_string();
                Effect::None
            }
            Panel::Workers => match app.selected_row(model) {
                Some(Row::Worker(wi)) | Some(Row::Task { worker: wi, .. }) => {
                    let w = &model.workers[wi];
                    Effect::Focus {
                        pane: w.pane.clone(),
                        label: w.name.clone(),
                    }
                }
                None => Effect::None,
            },
        },
        Key::Char('x') => match (app.panel, app.selected_row(model)) {
            // `x` 的合法目標只有 task 列（§2 selection model：否則沒有唯一
            // 的 immutable id 可綁）；worker 列上按 x 無效並提示
            (Panel::Workers, Some(Row::Task { task, .. })) => {
                app.confirm = Some(model.tasks[task].id.clone());
                Effect::None
            }
            _ => {
                app.message =
                    "x 僅對 task 列有效（cancel 需要唯一 task id；worker 列不可 x）".to_string();
                Effect::None
            }
        },
        _ => Effect::None,
    }
}

fn move_sel(app: &mut App, model: &Model, delta: i64) {
    let (idx, len) = match app.panel {
        Panel::Owners => (&mut app.owner_idx, model.owners.len()),
        Panel::Workers => {
            let n = app.rows(model).len();
            (&mut app.row_idx, n)
        }
    };
    if len == 0 {
        return;
    }
    let cur = *idx as i64 + delta;
    *idx = cur.clamp(0, len as i64 - 1) as usize;
    if matches!(app.panel, Panel::Owners) {
        // 換 owner 後 WORKERS 欄從頭選起
        app.row_idx = 0;
        // 人自己選過 owner 之後，晚到的 origin 不得再改 selection
        app.owner_touched = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ab_core::registry::AgentSnapshot;
    use ab_core::task::InFlight;

    fn model() -> Model {
        Model {
            owners: vec!["it:@1".into()],
            workers: vec![
                AgentSnapshot {
                    name: "w1".into(),
                    pane: "%5".into(),
                    runtime: "codex".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawned: true,
                    corrupt: false,
                },
                AgentSnapshot {
                    name: "w2".into(),
                    pane: "%6".into(),
                    runtime: "agy".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawned: true,
                    corrupt: false,
                },
            ],
            tasks: vec![InFlight {
                id: "20260731T000001Z-aaaa".into(),
                from: "alice".into(),
                to: "w1".into(),
                status: "queued".into(),
            }],
        }
    }

    /// `x` 的合法目標只有 task 列：worker 列上按 x 必須提示且不開確認框
    /// （§2 selection model）。
    #[test]
    fn x_on_worker_row_is_rejected_with_hint() {
        let m = model();
        let mut app = App::new();
        assert_eq!(handle_key(&mut app, &m, Key::Char('x')), Effect::None);
        assert!(app.confirm.is_none(), "worker 列不得開確認框");
        assert!(app.message.contains("task 列"), "實際：{}", app.message);
    }

    /// task 列上 x → 確認框綁 immutable id；y 執行、n 放棄（§5 單確認）。
    #[test]
    fn x_on_task_row_confirms_then_cancels_by_id() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j')); // w1 → task 列
        handle_key(&mut app, &m, Key::Char('x'));
        assert_eq!(app.confirm.as_deref(), Some("20260731T000001Z-aaaa"));
        // 確認框開著時導航鍵一律吞掉，selection 不動
        assert_eq!(handle_key(&mut app, &m, Key::Char('j')), Effect::None);
        assert_eq!(app.row_idx, 1);
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('y')),
            Effect::Cancel {
                id: "20260731T000001Z-aaaa".into()
            }
        );
        assert!(app.confirm.is_none());

        // n 放棄：不產生 Cancel
        handle_key(&mut app, &m, Key::Char('x'));
        assert!(app.confirm.is_some());
        assert_eq!(handle_key(&mut app, &m, Key::Char('n')), Effect::None);
        assert!(app.confirm.is_none());
    }

    /// Enter：worker 列 focus 該 pane；task 列 focus 所屬 worker 的 pane；
    /// OWNERS 欄提示無效（§2／§3）。
    #[test]
    fn enter_focuses_worker_pane_from_both_row_kinds() {
        let m = model();
        let mut app = App::new();
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Focus {
                pane: "%5".into(),
                label: "w1".into()
            }
        );
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Focus {
                pane: "%5".into(),
                label: "w1".into()
            }
        );
        // 第三列是 w2
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Focus {
                pane: "%6".into(),
                label: "w2".into()
            }
        );
        // OWNERS 欄按 Enter：提示、無副作用
        handle_key(&mut app, &m, Key::Tab);
        assert_eq!(handle_key(&mut app, &m, Key::Enter), Effect::None);
        assert!(app.message.contains("WORKERS"));
    }

    /// 導航夾在合法範圍內；q 離開；? 開合法鍵頁、任意鍵關閉。
    #[test]
    fn navigation_clamps_and_meta_keys_work() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('k'));
        assert_eq!(app.row_idx, 0, "上緣不得越界");
        for _ in 0..10 {
            handle_key(&mut app, &m, Key::Char('j'));
        }
        assert_eq!(app.row_idx, 2, "下緣夾在最後一列");
        assert_eq!(handle_key(&mut app, &m, Key::Char('?')), Effect::None);
        assert!(app.help);
        handle_key(&mut app, &m, Key::Char('j'));
        assert!(!app.help, "任意鍵關閉合法鍵頁");
        assert_eq!(app.row_idx, 2, "關頁那一鍵不得同時移動 selection");
        assert_eq!(handle_key(&mut app, &m, Key::Char('q')), Effect::Quit);
    }

    /// §2「以 current owner 為根」：首頁 selection 落在呼叫者所在的 owner，
    /// 而不是字典序第 0 筆（審查 F2）。三條路徑各驗一次。
    #[test]
    fn initial_owner_is_current_owner_not_first_alphabetically() {
        let m = Model {
            owners: vec!["aaa:@1".into(), "zzz:@9".into()],
            workers: vec![
                AgentSnapshot {
                    name: "wa".into(),
                    pane: "%1".into(),
                    runtime: "codex".into(),
                    owner: "aaa:@1".into(),
                    ready: "ready".into(),
                    spawned: true,
                    corrupt: false,
                },
                AgentSnapshot {
                    name: "wz".into(),
                    pane: "%2".into(),
                    runtime: "codex".into(),
                    owner: "zzz:@9".into(),
                    ready: "ready".into(),
                    spawned: true,
                    corrupt: false,
                },
            ],
            tasks: Vec::new(),
        };
        // (1) owner 標籤直接對上：字典序在前的 aaa:@1 不是 current
        assert_eq!(origin_owner_idx(&m, Some("zzz:@9"), None), Some(1));
        // (2) 標籤對不上（例如 TUI 開在 worker 自己的 pane 裡）→ 以 pane 反查
        assert_eq!(origin_owner_idx(&m, Some("nope:@0"), Some("%2")), Some(1));
        // (3) 都對不上 → None（呼叫端維持第 0 筆，不猜）
        assert_eq!(origin_owner_idx(&m, Some("nope:@0"), Some("%404")), None);
        assert_eq!(origin_owner_idx(&m, None, None), None);

        // apply_origin 落地；人動過 OWNERS 欄之後不得再被 origin 拉走
        let mut app = App::new();
        app.origin_owner = Some("zzz:@9".into());
        app.apply_origin(&m);
        assert_eq!(app.owner_idx, 1, "首頁根 MUST 是 current owner");

        let mut app2 = App::new();
        app2.panel = Panel::Owners;
        handle_key(&mut app2, &m, Key::Char('j'));
        handle_key(&mut app2, &m, Key::Char('k')); // 人自己選回第 0 筆
        app2.origin_owner = Some("zzz:@9".into());
        app2.apply_origin(&m);
        assert_eq!(app2.owner_idx, 0, "人動過之後 origin 不得搶 selection");
    }

    /// 列表縮短後 clamp 把 selection 夾回（500ms 重讀路徑）。
    #[test]
    fn clamp_after_reload_keeps_selection_valid() {
        let m = model();
        let mut app = App::new();
        app.row_idx = 2;
        let empty = Model {
            owners: vec!["it:@1".into()],
            workers: Vec::new(),
            tasks: Vec::new(),
        };
        app.clamp(&empty);
        assert_eq!(app.row_idx, 0);
        app.clamp(&m);
        assert_eq!(app.row_idx, 0);
    }
}

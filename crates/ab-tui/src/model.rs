//! Read model 與 selection model（tui-design.md §2／§4）。
//!
//! 資料來源分兩層、兩種節奏（§4）：
//! - 磁碟 task-plane＋registry：每 500ms 重讀（權威）
//! - tmux liveness：每 2s 一輪（只補位置與死活；查不到＝unknown，不覆寫權威）
//!
//! 純資料轉換全部放這裡（不碰 terminal），單元測試不經 render。

use std::collections::HashMap;

use ab_core::paths::Paths;
use ab_core::registry::{self, AgentSnapshot};
use ab_core::task::{self, InFlight};
use ab_core::tmux::TmuxClient;

/// TASKS 面板一次載入的任務上限。`tasks/` 會長大而 dashboard 每 500ms 掃一
/// 輪：截斷（在讀檔前）是這條路徑唯一的成本上限。200 列遠超過一畫面高度，
/// 人要看更舊的請走 `list`／`gc` 那條 CLI 路徑。
pub const RECENT_LIMIT: usize = 200;

/// 磁碟 read model 的一輪快照。
pub struct Model {
    /// 去重後的 owner 標籤（字典序）。manual worker 統一掛在 `-` 之下
    /// （沿用 `list --long` 的 owner 欄慣例：人工註冊者沒有 owner 概念）。
    pub owners: Vec<String>,
    pub workers: Vec<AgentSnapshot>,
    pub tasks: Vec<InFlight>,
    /// TASKS 面板用：近期任務（**含終態**），id 反序。與 `tasks` 分開存放
    /// ——WORKERS 欄要的是 in-flight，TASKS 欄要的是「有東西可讀」的全集。
    pub recent: Vec<InFlight>,
}

impl Model {
    pub fn load(paths: &Paths) -> Self {
        let workers = registry::snapshot(paths);
        let tasks = task::in_flight(paths);
        let recent = task::recent_tasks(paths, RECENT_LIMIT);
        let mut owners: Vec<String> = workers.iter().map(owner_label).collect();
        owners.sort();
        owners.dedup();
        Model {
            owners,
            workers,
            tasks,
            recent,
        }
    }

    /// worker 名 → `workers` 索引（TASKS 欄的列要回頭找所屬 worker）。
    pub fn worker_idx(&self, name: &str) -> Option<usize> {
        self.workers.iter().position(|w| w.name == name)
    }
}

/// worker 的歸屬標籤：spawned 且有 owner 欄→其字面值；manual→`-`；
/// spawned 但 owner 缺失（或 registry 損壞）→`?`。
pub fn owner_label(w: &AgentSnapshot) -> String {
    if w.corrupt {
        "?".to_string()
    } else if !w.spawned {
        "-".to_string()
    } else if w.owner.is_empty() {
        "?".to_string()
    } else {
        w.owner.clone()
    }
}

/// tmux liveness 快照（§4：節流每 2s，且每條查詢 bounded——逾時整層降級
/// `None`＝unknown，MUST NOT 凍結 UI）。
pub struct LiveIndex {
    /// pane id → 所有出現位置 `(session_name, window_id)`（linked window 下
    /// 同一 pane 可出現多次，cardinality 不可丟——§2 focus 語意要用）。
    pub panes: Option<HashMap<String, Vec<(String, String)>>>,
    /// 現存 window id 集合（owner 死活判定）。
    pub windows: Option<Vec<String>>,
}

impl LiveIndex {
    pub fn query(tmux: &dyn TmuxClient) -> Self {
        let panes = tmux
            .exec(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{session_name}\t#{window_id}",
            ])
            .and_then(|o| o.ok_stdout())
            .map(|out| {
                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for line in out.lines() {
                    let mut it = line.splitn(3, '\t');
                    if let (Some(p), Some(s), Some(w)) = (it.next(), it.next(), it.next()) {
                        map.entry(p.to_string())
                            .or_default()
                            .push((s.to_string(), w.to_string()));
                    }
                }
                map
            });
        let windows = tmux
            .exec(&["list-windows", "-a", "-F", "#{window_id}"])
            .and_then(|o| o.ok_stdout())
            .map(|out| out.lines().map(|l| l.to_string()).collect());
        LiveIndex { panes, windows }
    }

    /// 全 unknown 的空快照。UI 的起始值就是它——第一輪 liveness 由背景
    /// worker 回報，UI thread 一次 tmux 都不自己查（審查 F1）。
    pub fn unknown() -> Self {
        LiveIndex {
            panes: None,
            windows: None,
        }
    }
}

/// BLOCKER 軸（tui-design §4 的雙軸狀態：ACTIVITY × BLOCKER 正交疊加）。
///
/// v1 契約**只承諾兩件事**，兩者都有現成實作來源（§4／§7 終審收窄）：
/// - `Prompt`：沿用 `notify::screen_has_prompt` 的硬編碼 matcher
///   （permission／plan 兩類框，含 agy 與下緣備援錨）
/// - `Occluded`：**結構性查詢** `pane_in_mode`（AB-COPYMODE-1），不靠畫面比對
///   ——copy-mode 是人在看，不是 worker 閒著
///
/// `Unknown` MUST 與 `None` 分開（顯示紀律 §5）：查不到不等於「沒有 blocker」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocker {
    /// 查得到、沒有可見 blocker
    None,
    /// 權限／計畫確認框（`screen_has_prompt`）
    Prompt,
    /// pane 在 copy-mode 等 tmux mode（結構性，不是畫面比對）
    Occluded,
    /// tmux 查不到（逾時／不可用／pane 已不在）
    Unknown,
}

/// 一輪 blocker 查詢的快照。與 `LiveIndex` 同一輪節流（2s），每條查詢
/// bounded（§4 bounded-read 硬條款）。
pub struct BlockerIndex {
    /// pane id → blocker。整層查不到時為 `None`（全 unknown）
    pub panes: Option<HashMap<String, Blocker>>,
}

impl BlockerIndex {
    /// 對指定 pane 逐一查詢。**只查傳進來的 pane**（呼叫端給的是 registry
    /// 快照裡的 pane）：dashboard 每 2s 一輪，對整台機器的 pane 掃描是白工。
    pub fn query(tmux: &dyn TmuxClient, panes: &[String]) -> Self {
        let mut map = HashMap::new();
        for p in panes {
            if p.is_empty() {
                continue;
            }
            map.insert(p.clone(), blocker_of(tmux, p));
        }
        BlockerIndex { panes: Some(map) }
    }

    pub fn unknown() -> Self {
        BlockerIndex { panes: None }
    }

    pub fn get(&self, pane: &str) -> Blocker {
        match &self.panes {
            None => Blocker::Unknown,
            Some(m) => m.get(pane).copied().unwrap_or(Blocker::Unknown),
        }
    }
}

/// screen-matcher 來源的 blocker 要**連續命中這麼多輪**才升旗。
///
/// 理由：`Prompt` 靠畫面字串比對，單幀就可能是助理輸出剛好寫出那些字
/// （2026-08-01 語料：一行 `rg` 指令回顯就湊齊三組特徵）。matcher 收窄後
/// 這類幀已大幅減少，但畫面比對本質上沒有上層可否決——去抖是第二道。
/// 延遲代價照實記：升旗是「**首次命中後再等一輪**」。以 2s 一輪計，框可能
/// 剛好在一輪剛掃完之後才出現，**自框出現算起最壞未滿 4s**（一輪等到首次
/// 命中、再一輪確認），**不是 2s**——低報一半會讓下一個人以為餘裕比實際多。
/// 假警報則最多閃一輪就消失。worker 停在權限框是**分鐘級**的等待，4s 對人的
/// 判斷沒有影響。
///
/// 結構性判定（`Occluded`／`pane_in_mode`）**不去抖**：它不是字串比對，
/// 沒有單幀誤判面，去抖只會白白延後。
const BLOCKER_DEBOUNCE_ROUNDS: u32 = 2;

/// 跨輪去抖狀態：pane → 連續命中輪數。**只存 TUI 記憶體**（同 §4 的 occluded
/// 前值紀律），重開從零起算。
#[derive(Default)]
pub struct BlockerDebounce {
    streak: HashMap<String, u32>,
}

impl BlockerDebounce {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一輪原始判定過一次去抖，回傳要拿去顯示的索引。
    ///
    /// - `Prompt`：連續第 `BLOCKER_DEBOUNCE_ROUNDS` 輪起才放行；在那之前回
    ///   `None`（「查得到、沒有可見 blocker」）。**不是** `Unknown`——這一輪
    ///   畫面確實讀到了，謊報成「沒訊號」會讓 §5 的三態語意失真。
    /// - 其餘（`None`／`Occluded`／`Unknown`）原樣通過，並把連勝歸零：
    ///   **降旗即時**，一輪沒命中就撤，不欠人一個「還要再等一輪」。
    /// - 不在本輪索引裡的 pane 會被丟掉，streak 不會無界成長。
    pub fn apply(&mut self, raw: BlockerIndex) -> BlockerIndex {
        let Some(panes) = raw.panes else {
            // 整層查不到：連勝全清（下一輪重新起算），維持 unknown
            self.streak.clear();
            return BlockerIndex { panes: None };
        };
        let mut next = HashMap::with_capacity(panes.len());
        let mut out = HashMap::with_capacity(panes.len());
        for (pane, blocker) in panes {
            let shown = if blocker == Blocker::Prompt {
                let n = self.streak.get(&pane).copied().unwrap_or(0) + 1;
                next.insert(pane.clone(), n);
                if n >= BLOCKER_DEBOUNCE_ROUNDS {
                    Blocker::Prompt
                } else {
                    Blocker::None
                }
            } else {
                blocker
            };
            out.insert(pane, shown);
        }
        self.streak = next;
        BlockerIndex { panes: Some(out) }
    }
}

/// 單一 pane 的 blocker 判定（純粹是兩次 bounded 查詢的組合，可用假件單測）。
///
/// 先問結構性的 copy-mode 再看畫面：copy-mode 下的 `capture-pane` 拿到的是
/// 人捲到的位置，拿它判 prompt 只會是誤判。查不到一律 `Unknown`——**MUST NOT
/// 當成 `None`**（§5：沒有訊號 ≠ 沒有 blocker）。
pub fn blocker_of(tmux: &dyn TmuxClient, pane: &str) -> Blocker {
    match tmux.pane_in_mode(pane) {
        Some(true) => return Blocker::Occluded,
        Some(false) => {}
        None => return Blocker::Unknown,
    }
    match tmux.capture_pane(pane) {
        Some(screen) if ab_core::notify::screen_has_prompt(&screen) => Blocker::Prompt,
        Some(_) => Blocker::None,
        None => Blocker::Unknown,
    }
}

/// 三態死活：查不到（tmux 逾時／不可用）≠ 查了但不在（同 `list --long` 的
/// 顯示紀律：誤標 dead 會讓人以為該回收）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Dead,
    Unknown,
}

pub fn pane_liveness(live: &LiveIndex, pane: &str) -> Liveness {
    if pane.is_empty() {
        return Liveness::Unknown;
    }
    match &live.panes {
        None => Liveness::Unknown,
        Some(m) => {
            if m.contains_key(pane) {
                Liveness::Live
            } else {
                Liveness::Dead
            }
        }
    }
}

/// owner 標籤形如 `<session>:@<winid>` 時以 window id 判死活；其他形
/// （`-`／`?`）無死活可言→Unknown。
pub fn owner_liveness(live: &LiveIndex, label: &str) -> Liveness {
    let Some((_, win)) = label.rsplit_once(':') else {
        return Liveness::Unknown;
    };
    if !win.starts_with('@') {
        return Liveness::Unknown;
    }
    match &live.windows {
        None => Liveness::Unknown,
        Some(ws) => {
            if ws.iter().any(|w| w == win) {
                Liveness::Live
            } else {
                Liveness::Dead
            }
        }
    }
}

/// WORKERS 欄的一列：worker 列或其下的 in-flight task 列（§2 selection
/// model：兩者皆可選取，task 列自帶 immutable task id）。值是 `Model`
/// 內的索引，不複製資料。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Worker(usize),
    Task { worker: usize, task: usize },
}

/// 攤平選中 owner 之下的 worker／task 列。worker 依 snapshot 序（檔名字典
/// 序），task 依 id 序（in_flight 已排序）。
pub fn worker_rows(model: &Model, owner: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for (wi, w) in model.workers.iter().enumerate() {
        if owner_label(w) != owner {
            continue;
        }
        rows.push(Row::Worker(wi));
        for (ti, t) in model.tasks.iter().enumerate() {
            if t.to == w.name {
                rows.push(Row::Task {
                    worker: wi,
                    task: ti,
                });
            }
        }
    }
    rows
}

/// TASKS 欄的列：`model.recent` 的索引。只留派給「當前選中 owner 底下某個
/// worker」的任務，順序沿用 `recent`（id 反序＝新的在上）。
///
/// 為什麼要含終態：`r` 讀的是回覆，而 `read` 只對 `completed`／`failed`
/// 合法；WORKERS 欄只有 in-flight 列，沒有終態任務就沒有東西可讀。
pub fn task_rows(model: &Model, owner: &str) -> Vec<usize> {
    let names: Vec<&str> = model
        .workers
        .iter()
        .filter(|w| owner_label(w) == owner)
        .map(|w| w.name.as_str())
        .collect();
    model
        .recent
        .iter()
        .enumerate()
        .filter(|(_, t)| names.contains(&t.to.as_str()))
        .map(|(i, _)| i)
        .collect()
}

/// 終態判定（`x` 的合法目標判斷用）。權威字沿用 `spec/state.md`。
pub fn is_terminal_status(st: &str) -> bool {
    matches!(st, "completed" | "failed" | "cancelled")
}

/// `Enter` focus 的執行計畫（§2）：目標在 current session→只 select；
/// 在別的 session→先 switch-client。linked window 多位置→優先 current
/// session 的 location，否則取第一個 live location；不彈詢問框。
/// 回傳 `None`＝pane 位置查不到（死了或 tmux unknown），呼叫端降級成訊息。
#[derive(PartialEq, Eq, Debug)]
pub struct FocusPlan {
    pub switch_to: Option<String>,
    pub window: String,
}

pub fn focus_plan(
    locs: Option<&Vec<(String, String)>>,
    current_session: Option<&str>,
) -> Option<FocusPlan> {
    let locs = locs?;
    let chosen = current_session
        .and_then(|cur| locs.iter().find(|(s, _)| s == cur))
        .or_else(|| locs.first())?;
    let switch_to = match current_session {
        Some(cur) if cur == chosen.0 => None,
        // tmux 外啟動（無 current session）也給出 switch 目標：有 client 就
        // 切得過去，沒 client 只是查詢失敗降級
        _ => Some(chosen.0.clone()),
    };
    Some(FocusPlan {
        switch_to,
        window: chosen.1.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, spawned: bool, owner: &str) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            pane: format!("%{}", name.len()),
            runtime: "codex".to_string(),
            owner: owner.to_string(),
            ready: "ready".to_string(),
            spawn_tag: format!("tag-{name}"),
            registered_at: "2026-07-31T00:00:00Z".to_string(),
            spawned,
            corrupt: false,
        }
    }

    fn inflight(id: &str, to: &str) -> InFlight {
        InFlight {
            id: id.to_string(),
            from: "alice".to_string(),
            to: to.to_string(),
            status: "queued".to_string(),
        }
    }

    /// selection model（§2）：worker 列與其 in-flight task 列皆為可選取列，
    /// task 緊接在所屬 worker 之後；別的 owner 的 worker 不得混入。
    #[test]
    fn worker_rows_interleave_tasks_under_their_worker() {
        let model = Model {
            owners: vec!["-".into(), "it:@1".into()],
            workers: vec![
                snap("w1", true, "it:@1"),
                snap("w2", true, "it:@1"),
                snap("manual", false, ""),
            ],
            tasks: vec![
                inflight("20260731T000001Z-aaaa", "w1"),
                inflight("20260731T000002Z-bbbb", "w2"),
                inflight("20260731T000003Z-cccc", "w1"),
            ],
            recent: Vec::new(),
        };
        let rows = worker_rows(&model, "it:@1");
        assert_eq!(
            rows,
            vec![
                Row::Worker(0),
                Row::Task { worker: 0, task: 0 },
                Row::Task { worker: 0, task: 2 },
                Row::Worker(1),
                Row::Task { worker: 1, task: 1 },
            ]
        );
        assert_eq!(worker_rows(&model, "-"), vec![Row::Worker(2)]);
    }

    /// TASKS 欄（§2 版面新增）：含終態、id 反序、只留本 owner 底下 worker 的
    /// 任務；別的 owner 的任務不得混入。
    #[test]
    fn task_rows_keep_terminal_tasks_of_this_owner_newest_first() {
        let mut done = inflight("20260731T000009Z-dddd", "w1");
        done.status = "completed".to_string();
        let model = Model {
            owners: vec!["it:@1".into(), "other:@2".into()],
            workers: vec![snap("w1", true, "it:@1"), snap("wx", true, "other:@2")],
            tasks: Vec::new(),
            // recent_tasks 的輸出已是反序
            recent: vec![
                done,
                inflight("20260731T000005Z-eeee", "wx"),
                inflight("20260731T000001Z-aaaa", "w1"),
            ],
        };
        assert_eq!(task_rows(&model, "it:@1"), vec![0, 2], "含終態、新的在上");
        assert_eq!(task_rows(&model, "other:@2"), vec![1]);
        assert!(is_terminal_status("completed") && is_terminal_status("cancelled"));
        assert!(!is_terminal_status("running"));
    }

    /// §2 focus 語意逐條：current session 優先、跨 session 才 switch、
    /// linked window 多位置不彈框、查不到＝None。
    #[test]
    fn focus_plan_follows_design_semantics() {
        let locs = vec![
            ("other".to_string(), "@7".to_string()),
            ("cur".to_string(), "@3".to_string()),
        ];
        // current session 的 location 優先，且不 switch
        assert_eq!(
            focus_plan(Some(&locs), Some("cur")),
            Some(FocusPlan {
                switch_to: None,
                window: "@3".to_string()
            })
        );
        // current session 沒有 → 取第一個 live location，先 switch
        assert_eq!(
            focus_plan(Some(&locs), Some("elsewhere")),
            Some(FocusPlan {
                switch_to: Some("other".to_string()),
                window: "@7".to_string()
            })
        );
        // 查不到位置（pane 死了／tmux unknown）→ None（降級，不凍結不猜）
        assert_eq!(focus_plan(None, Some("cur")), None);
        assert_eq!(focus_plan(Some(&Vec::new()), Some("cur")), None);
    }

    /// blocker 判定用的假 tmux：可分別調 `pane_in_mode` 與 `capture_pane`
    /// 的回應（含 `None`＝bounded 查詢逾時／不可用）。
    struct BlockerTmux {
        in_mode: Option<bool>,
        screen: Option<&'static str>,
    }

    impl TmuxClient for BlockerTmux {
        fn exec(&self, _args: &[&str]) -> Option<ab_core::tmux::TmuxOutput> {
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
            self.screen.map(|s| s.to_string())
        }
        fn pane_in_mode(&self, _p: &str) -> Option<bool> {
            self.in_mode
        }
        fn send_keys(&self, _p: &str, _k: &str) -> bool {
            false
        }
    }

    /// BLOCKER 軸的 v1 契約（§4）：硬編碼 prompt matcher ＋結構性 occlusion，
    /// 且 **unknown MUST 與 none 分開**（§5：沒有訊號 ≠ 沒有 blocker）。
    #[test]
    fn blocker_axis_keeps_v1_contract_and_three_states() {
        // 真的長得像 claude 權限框（`screen_has_prompt` 的第一組錨）
        let prompt = "Do you want to make this edit?\n 1. Yes\n 2. No\nEsc to cancel";
        let cases: [(Option<bool>, Option<&'static str>, Blocker); 6] = [
            // copy-mode 是結構性判定，先於畫面：人在看，不是 worker 閒著
            (Some(true), Some(prompt), Blocker::Occluded),
            (Some(true), None, Blocker::Occluded),
            // 非 copy-mode → 看畫面
            (Some(false), Some(prompt), Blocker::Prompt),
            (Some(false), Some("just some output\n$ "), Blocker::None),
            // 畫面查不到（bounded 逾時）→ unknown，MUST NOT 當成 none
            (Some(false), None, Blocker::Unknown),
            // 連 mode 都查不到 → unknown
            (None, Some(prompt), Blocker::Unknown),
        ];
        for (in_mode, screen, want) in cases {
            let tmux = BlockerTmux { in_mode, screen };
            assert_eq!(
                blocker_of(&tmux, "%1"),
                want,
                "in_mode={in_mode:?} screen={screen:?}"
            );
        }
    }

    /// `BlockerIndex`：只查傳進來的 pane；沒查過的與整層失效都是 unknown。
    #[test]
    fn blocker_index_defaults_to_unknown_outside_the_queried_set() {
        let tmux = BlockerTmux {
            in_mode: Some(false),
            screen: Some("idle output"),
        };
        let idx = BlockerIndex::query(&tmux, &["%1".to_string(), String::new()]);
        assert_eq!(idx.get("%1"), Blocker::None);
        // 空 pane 不查（registry 缺 pane_id），也不會被當成 none
        assert_eq!(idx.get(""), Blocker::Unknown);
        assert_eq!(idx.get("%404"), Blocker::Unknown, "沒查過的 pane＝unknown");
        // 整層 unknown（UI 起始值／worker 還沒回報）
        assert_eq!(BlockerIndex::unknown().get("%1"), Blocker::Unknown);
    }

    /// 三態死活：查不到 ≠ 不在（顯示紀律，tui-design §5）。
    #[test]
    fn liveness_keeps_unknown_distinct_from_dead() {
        let unknown = LiveIndex::unknown();
        assert!(matches!(pane_liveness(&unknown, "%1"), Liveness::Unknown));
        assert!(matches!(
            owner_liveness(&unknown, "it:@1"),
            Liveness::Unknown
        ));

        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), vec![("it".to_string(), "@1".to_string())]);
        let live = LiveIndex {
            panes: Some(panes),
            windows: Some(vec!["@1".to_string()]),
        };
        assert!(matches!(pane_liveness(&live, "%1"), Liveness::Live));
        assert!(matches!(pane_liveness(&live, "%9"), Liveness::Dead));
        assert!(matches!(owner_liveness(&live, "it:@1"), Liveness::Live));
        assert!(matches!(owner_liveness(&live, "it:@9"), Liveness::Dead));
        // manual（`-`）與 `?` 沒有死活可言
        assert!(matches!(owner_liveness(&live, "-"), Liveness::Unknown));
        assert!(matches!(owner_liveness(&live, "?"), Liveness::Unknown));
    }

    fn idx(pane: &str, b: Blocker) -> BlockerIndex {
        let mut m = HashMap::new();
        m.insert(pane.to_string(), b);
        BlockerIndex { panes: Some(m) }
    }

    /// 去抖升旗：screen-matcher 來源的 `Prompt` MUST 連續 K 輪才升起。
    ///
    /// 沒有這條，把 `BLOCKER_DEBOUNCE_ROUNDS` 改成 1 一樣全綠——而那正是
    /// 「單幀指令回顯 → 常駐假 ⛔blocked」的回歸。
    #[test]
    fn prompt_blocker_needs_consecutive_rounds_to_raise() {
        let mut d = BlockerDebounce::new();
        // 第一輪命中：還不升旗（顯示成「沒有可見 blocker」，不是 unknown）
        assert_eq!(d.apply(idx("%1", Blocker::Prompt)).get("%1"), Blocker::None);
        // 第二輪仍命中：升旗
        assert_eq!(
            d.apply(idx("%1", Blocker::Prompt)).get("%1"),
            Blocker::Prompt
        );
        // 持續命中維持升旗
        assert_eq!(
            d.apply(idx("%1", Blocker::Prompt)).get("%1"),
            Blocker::Prompt
        );
    }

    /// 單輪閃現不升旗，且連勝會被中斷的那一輪歸零（不得累加跨越間斷）。
    #[test]
    fn a_single_frame_never_raises_and_streaks_do_not_carry_over() {
        let mut d = BlockerDebounce::new();
        assert_eq!(d.apply(idx("%1", Blocker::Prompt)).get("%1"), Blocker::None);
        assert_eq!(d.apply(idx("%1", Blocker::None)).get("%1"), Blocker::None);
        // 歸零後再命中一次仍不足以升旗
        assert_eq!(d.apply(idx("%1", Blocker::Prompt)).get("%1"), Blocker::None);
    }

    /// 降旗**即時**：升旗後一輪未命中就撤，不欠人一個「再等一輪」。
    #[test]
    fn lowering_the_flag_is_immediate() {
        let mut d = BlockerDebounce::new();
        d.apply(idx("%1", Blocker::Prompt));
        assert_eq!(
            d.apply(idx("%1", Blocker::Prompt)).get("%1"),
            Blocker::Prompt
        );
        assert_eq!(d.apply(idx("%1", Blocker::None)).get("%1"), Blocker::None);
    }

    /// 結構性判定不受去抖影響：`Occluded`（`pane_in_mode`）第一輪就要顯示，
    /// `Unknown` 也 MUST NOT 被去抖改寫成 `None`（§5：沒有訊號 ≠ 沒有 blocker）。
    #[test]
    fn structural_verdicts_bypass_the_debounce() {
        let mut d = BlockerDebounce::new();
        assert_eq!(
            d.apply(idx("%1", Blocker::Occluded)).get("%1"),
            Blocker::Occluded
        );
        assert_eq!(
            d.apply(idx("%2", Blocker::Unknown)).get("%2"),
            Blocker::Unknown
        );
        // 整層查不到時維持 unknown
        assert_eq!(d.apply(BlockerIndex::unknown()).get("%1"), Blocker::Unknown);
    }

    /// 每個 pane 各自計數：一個 pane 的連勝不得幫另一個 pane 升旗。
    #[test]
    fn debounce_streaks_are_per_pane() {
        let mut d = BlockerDebounce::new();
        let mut m = HashMap::new();
        m.insert("%1".to_string(), Blocker::Prompt);
        m.insert("%2".to_string(), Blocker::None);
        d.apply(BlockerIndex { panes: Some(m) });

        let mut m2 = HashMap::new();
        m2.insert("%1".to_string(), Blocker::Prompt);
        m2.insert("%2".to_string(), Blocker::Prompt);
        let out = d.apply(BlockerIndex { panes: Some(m2) });
        assert_eq!(out.get("%1"), Blocker::Prompt); // 連續兩輪
        assert_eq!(out.get("%2"), Blocker::None); // 才第一輪
    }
}

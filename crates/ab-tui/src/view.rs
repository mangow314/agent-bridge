//! render 層（tui-design.md §2 版面：ORIGINS｜WORKERS／TASKS｜DETAIL）。
//!
//! 顯示紀律（§2／§5）：task status 一律顯示權威字（queued/delivered/running
//! ——不存在 `blocked`）；selection 以文字 marker `▶` 呈現（`capture-pane`
//! 的特徵字串斷言吃不到 style，marker 必須落在字元層）；不得以顏色／排序
//! 暗示可刪度。
//!
//! chrome 一律英文（P4.6 題 9）：欄位名、footer、空清單 placeholder、警告、
//! 確認框、pager 標題、`?` 頁。**不譯的東西**：payload 原文、agent 名、
//! 權威 status 字、CLI 命令原文——那些不是 chrome，是證據。

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::action::{cancel_cmdline, evidence};
use crate::app::{App, EnterAct, Panel, Sel};
use crate::model::{
    Blocker, BlockerIndex, DISK_STALE, Freshness, LiveIndex, Liveness, Model, Row,
    STANDALONE_LABEL, TMUX_STALE, age_is_worth_showing, breadcrumb, breadcrumb_line,
    breadcrumb_line_fit, group_label, group_line_offsets, group_visible_members, origin_label,
    pane_liveness, window_detail, worker_line_of,
};
use crate::theme;

/// task id 的固定長度（`YYYYMMDDTHHMMSSZ-xxxx`，見 `ab_core::task`）。
const TASK_ID_W: u16 = 21;
/// 中欄最小寬：選中 marker（2）＋列 glyph（2）＋完整 task id ＋左右邊框（2）。
const MID_MIN_W: u16 = 4 + TASK_ID_W + 2;
// ORIGINS 欄在 P4.7 切片 B 退場（lineage 成為唯一邏輯軸）。物理位置沒有被
// 刪掉，它降到 DETAIL 當證據列——那裡它是「這個 worker 現在坐在哪」，是事實。
/// DETAIL 欄：容得下最長的等價 CLI 原文 `  $ agent-bridge read <id>`
/// （2＋2＋18＋21＝43）＋左右邊框。
const DETAIL_W: u16 = 43 + 2;
/// 三欄同時成立所需的最小寬。不足時 DETAIL 改走整寬底條（見 `render`）。
/// 兩欄並排（中欄＋DETAIL）的最小寬。ORIGINS 退場後少了 24 欄，同一台 80 欄
/// 終端機因此更容易走得進並排版面
const TWO_COL_MIN_W: u16 = MID_MIN_W + DETAIL_W;
/// 底條模式下 DETAIL 的高度。由**最長的那一種選取**回推，而不是照 task 那
/// 一種抓個大概——DETAIL 的等價 CLI 原文是薄殼原則的憑證（見 `layout`），
/// 少一行就等於畫面上沒有那條命令。
///
/// - worker：name／pane／runtime／ready／lineage／origin／state＋blocker ＝8
///   ＋空行＋`evidence:`＋1 條命令 ＝**11**
/// - task：task-id／from／to／status／pane／blocker ＝6
///   ＋空行＋`evidence:`＋2 條命令 ＝10
///
/// 取 11＋上下邊框＝13。（P4.7 切片 B2 從 11 調上來：breadcrumb 那一列把
/// worker 這一支推長了一行，而舊值連調整前的 `evidence:` 都已經壓在邊界上。）
const DETAIL_STRIP_H: u16 = 13;
/// DETAIL 各列的 label 寬（`lineage: ` 等，冒號後一個空格）。breadcrumb 收縮
/// 時要從可用欄寬裡先扣掉它。
const LINEAGE_LABEL_W: usize = 9;
/// footer 一次畫得出的 sticky 警告則數（多的以「另有 N 則」帶出，見
/// `render_footer`）。
const WARN_ROWS: usize = 3;

/// 版面切分（§2）：三欄＋中欄縱切。**render 與 run loop 共用同一份計算**
/// ——PgUp／PgDn 的一頁是「該面板的可視高度」，兩邊各算一份就會在窄畫面下
/// 各說各話（切片 C）。
struct Areas {
    workers: Rect,
    tasks: Rect,
    detail: Rect,
    footer: Rect,
    /// DETAIL 走的是整寬底條（寬度不足的那一支）。底條**高度固定**，所以
    /// 它裡面的長字串不能換行——換一行就把底下的等價 CLI 原文推出畫面
    detail_strip: bool,
}

/// footer 高度隨 sticky 警告伸縮（major #2：警告不得被單行 message 覆寫，
/// 所以它們需要自己的行）。上限 `WARN_ROWS`，溢位以「另 N 則」帶出，
/// 不是靜默丟掉。＋1 是「（Esc 清除警告）」那行——只在有警告時才佔位。
/// `extra`＝footer 的額外行（P4.7 切片 C 的 filter 提示列與 copy-mode
/// banner，各 0／1 行）。它們**佔版面**，所以必須進這個算式——只在 render
/// 裡多畫一行的話，那一行會蓋掉面板的最後一列。
fn footer_rows(warnings: usize, extra: usize) -> u16 {
    // footer 三行：當前列的鍵、全域鍵、輪詢狀態＋message（P4.6 切片 B）
    3 + extra as u16
        + if warnings == 0 {
            0
        } else {
            warnings.min(WARN_ROWS) as u16 + 1
        }
}

fn layout(area: Rect, warnings: usize, extra: usize) -> Areas {
    let [main, footer] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(footer_rows(warnings, extra)),
    ])
    .areas(area);
    // 兩條硬不變量同時要守——中欄那 21 字元的 immutable task id MUST 永不截斷
    // （它是 dashboard 全部「證據」語意的承重點，§2／§5），而 DETAIL 的等價
    // CLI 原文 MUST 完整留在畫面上（薄殼原則，§2：截半條命令等於畫面上沒有
    // 那條命令）。
    //
    // 三者相加要 92 欄，80 欄的終端機放不下——**寬度不足時 DETAIL 改走整寬
    // 底條**，而不是壓縮任何一方。80 欄下底條有整整 78 欄，命令原文照樣成行；
    // 犧牲的只是垂直空間，那是列表捲動本來就處理得了的。
    let strip = main.width < TWO_COL_MIN_W;
    let (mid, detail) = if !strip {
        let [m, d] = Layout::horizontal([Constraint::Min(MID_MIN_W), Constraint::Length(DETAIL_W)])
            .areas(main);
        (m, d)
    } else {
        let [m, d] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(DETAIL_STRIP_H)]).areas(main);
        (m, d)
    };
    let [workers, tasks] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid);
    Areas {
        workers,
        tasks,
        detail,
        footer,
        detail_strip: strip,
    }
}

/// 各面板的可視列數（扣掉上下邊框）。run loop 每幀量一次回填給狀態機，
/// PgUp／PgDn 據此翻一頁（`app::PageSizes`）。
pub fn panel_heights(area: Rect, warnings: usize, extra: usize) -> crate::app::PageSizes {
    let a = layout(area, warnings, extra);
    let inner = |r: Rect| r.height.saturating_sub(2);
    crate::app::PageSizes {
        workers: inner(a.workers),
        tasks: inner(a.tasks),
        // pager 是全螢幕 overlay，不吃三欄版面（上下框各一格）
        pager: inner(area),
    }
}

pub fn render(
    f: &mut Frame,
    model: &Model,
    live: &LiveIndex,
    blockers: &BlockerIndex,
    app: &App,
    fresh: Freshness,
) {
    let a = layout(
        f.area(),
        app.warnings.len(),
        footer_extra_rows(model, app, blockers),
    );
    let (workers_area, tasks_area, detail_area, footer) = (a.workers, a.tasks, a.detail, a.footer);

    render_workers(f, workers_area, model, live, blockers, app);
    render_tasks(f, tasks_area, model, app);
    render_detail(f, detail_area, model, live, blockers, app, a.detail_strip);
    render_footer(f, footer, model, app, blockers, fresh);

    // overlay 優先序：確認框（cancel／evict）> 全文 pager > 摘要頁 >
    // 尾行預覽 > 合法鍵
    if let Some(id) = &app.confirm {
        render_confirm(f, id);
    } else if let Some(p) = &app.evict_prompt {
        render_evict_confirm(f, &p.lines);
    } else if app.pager.is_some() {
        render_pager(f, app);
    } else if let Some(lines) = &app.info {
        render_info(f, lines);
    } else if let Some(p) = &app.peek {
        render_peek(f, p);
    } else if app.help {
        render_help(f, model, app);
    }
}

fn sel_prefix(selected: bool) -> &'static str {
    if selected { "▶ " } else { "  " }
}

fn render_workers(
    f: &mut Frame,
    area: Rect,
    model: &Model,
    live: &LiveIndex,
    blockers: &BlockerIndex,
    app: &App,
) {
    let focused = app.panel == Panel::Workers;
    let rows = app.rows(model);
    // 每一列前面要先插幾行組標頭（純函式，捲動位移與畫線共用同一份答案）
    let heads = group_line_offsets(model, &app.filter);
    // 標頭的括號數字算**畫得出來的**那些（過濾中 `members.len()` 會說謊）
    let visible = group_visible_members(model, &app.filter);
    let mut lines = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        // 這一列是某組的第一個成員 → 先畫該組的標頭
        if let Some(gi) = heads.get(&i) {
            let g = &model.groups[*gi];
            // **標頭不上任何語意色**：組是分類不是狀態，染色會與 status／
            // liveness 搶同一份注意力，而它一格狀態資訊都沒有多給
            lines.push(Line::from(Span::raw(format!(
                "{} ({})",
                group_label(model, g),
                visible.get(gi).copied().unwrap_or(g.members.len())
            ))));
        }
        let selected = focused && i == app.row_idx;
        let spans = match *row {
            Row::Worker(wi) => {
                let w = &model.workers[wi];
                let rt = if w.runtime.is_empty() {
                    "-"
                } else {
                    &w.runtime
                };
                let l = pane_liveness(live, &w.pane);
                let alive = match l {
                    Liveness::Live => "",
                    Liveness::Dead => "  ✗dead",
                    Liveness::Unknown => "  ?",
                };
                let b = blockers.get(&w.pane);
                // 前段（名稱／runtime／pane／ready）不帶語意色；死活與 blocker
                // 各自成 Span 上色。三段接起來與原本那個 format! 逐字相同
                let mut v = vec![Span::raw(format!(
                    "{}▸ {}  {}  {}  {}",
                    sel_prefix(selected),
                    w.name,
                    rt,
                    if w.pane.is_empty() { "-" } else { &w.pane },
                    w.ready,
                ))];
                if !alive.is_empty() {
                    v.push(Span::styled(alive, theme::liveness_style(l)));
                }
                let mark = blocker_mark(b);
                if !mark.is_empty() {
                    v.push(match theme::blocker_style(b) {
                        Some(s) => Span::styled(mark, s),
                        None => Span::raw(mark),
                    });
                }
                v
            }
            Row::Task { task, .. } => {
                let t = &model.tasks[task];
                // status 是權威字（queued/delivered/running），不縮寫不造詞。
                // 縮排只留 `└ `：中欄最窄（MID_MIN_W）時 task id 仍要整條進得去
                vec![
                    Span::raw(format!("{}└ {}  ", sel_prefix(selected), t.id)),
                    Span::styled(t.status.clone(), theme::status_style(&t.status)),
                ]
            }
        };
        lines.push(styled(spans, selected));
    }
    if rows.is_empty() {
        // 空的原因有兩種，畫面要說得出是哪一種：一個都沒註冊，還是被 filter
        // 篩掉了（後者按 Esc 就回得來，前者按什麼都沒用）
        lines.push(Line::from(if app.filter.is_active() {
            "  (no rows match the filter)"
        } else {
            "  (no workers registered)"
        }));
    }
    let block = panel_block("WORKERS", focused);
    let scroll = scroll_offset(worker_line_of(model, &app.filter, app.row_idx), area);
    f.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// TASKS 欄（§2）：**全 pool** 的任務平坦列表（P4.7 切片 B1 起不再依 owner
/// 過濾——過濾軸隨 ORIGINS 面板一起退場），**含終態**，id 反序。沒有這一欄，
/// 畫面上就沒有 `r` 讀得動的任務（read 只對 completed／failed 合法）。
fn render_tasks(f: &mut Frame, area: Rect, model: &Model, app: &App) {
    let focused = app.panel == Panel::Tasks;
    let rows = app.task_rows(model);
    let mut lines = Vec::new();
    for (i, &ti) in rows.iter().enumerate() {
        let selected = focused && i == app.task_idx;
        let t = &model.recent[ti];
        // status 一律權威字（completed/failed/cancelled/queued/…），glyph 是
        // 另一軸的裝飾，不取代狀態字
        let g = match t.status.as_str() {
            "completed" => "✓",
            "failed" => "✗",
            "cancelled" => "-",
            _ => "⚙",
        };
        // glyph 與 status 是**同一個語意軸**（status），同色；中間的 id／to
        // 不帶語意色。字元序列與原本那個 format! 逐字相同
        let st = theme::status_style(&t.status);
        lines.push(styled(
            vec![
                Span::raw(sel_prefix(selected)),
                Span::styled(g, st),
                Span::raw(format!(" {}  {}  ", t.id, t.to)),
                Span::styled(t.status.clone(), st),
            ],
            selected,
        ));
    }
    if rows.is_empty() {
        // 空的原因有三種，畫面 MUST 說得出是哪一種。**`recent` 本身是空的要
        // 排在最前面**（修正輪 R2／F5）：一筆任務都不存在時說「全部都掛上了」
        // 是在宣稱一件沒有證據的事（全新 pool 切到 Unattached 就會踩到），
        // 與同檔 `+` 旗標防的是同一類毛病
        lines.push(Line::from(if model.recent.is_empty() {
            "  (no recent tasks)"
        } else if app.filter.is_active() {
            "  (no rows match the filter)"
        } else if app.scope == crate::model::Scope::Unattached {
            "  (every task is attached to a worker)"
        } else {
            "  (no recent tasks)"
        }));
    }
    // 標題帶 `N/total`（P4.6 切片 C）：沒有它，人看不出自己在第幾筆、也看不出
    // 底下還有多少。**截斷要說出來**——`RECENT_LIMIT` 之外還有更舊的任務時
    // 標 `+`，否則畫面等於宣稱「就這些了」。
    //
    // P4.6 切片 C 的 F5：「只有全 pool 的數字才配得上 `+`」——`recent_truncated`
    // 是**全 pool** 的旗標，貼到子集合的數字上等於宣稱一件沒有證據的事。
    // 切片 B 期間這一欄本來就是全 pool，前提自動成立；**切片 C 又生出兩個
    // 子集合**（filter 與 Unattached scope），所以 F5 的前半條要顯式寫回來：
    // 篩選中或非 All scope 時不標 `+`。另外半條照舊：一列都沒有時也不標
    // （`TASKS 0/0+` 是在說「還有更舊的」卻連一筆都沒載到）。
    let total = rows.len();
    let pos = if total == 0 { 0 } else { app.task_idx + 1 };
    let whole_pool = !app.filter.is_active() && app.scope == crate::model::Scope::All;
    let more = if model.recent_truncated && total > 0 && whole_pool {
        "+"
    } else {
        ""
    };
    // scope 進標題（footer 也印一份）：`s` 切過去之後，畫面上少了一半的列，
    // 人得看得出那是 scope 而不是資料不見了
    let title = format!("TASKS {pos}/{total}{more} [{}]", app.scope.label());
    f.render_widget(
        Paragraph::new(lines)
            .block(panel_block(&title, focused))
            .scroll((scroll_offset(app.task_idx, area), 0)),
        area,
    );
    render_scrollbar(f, area, total, app.task_idx);
}

/// TASKS 欄的捲軸（P4.6 切片 C）。
///
/// **只在真的捲得動時才畫**：列數塞得進畫面時畫一條空軌道，等於用一欄寬度說
/// 一句沒有資訊量的話。thumb 位置由 (選取序位, 總數) 決定——首列在頂、末列在
/// 底、中段在中，人用它判斷「我在清單的哪裡」。
///
/// 樣式走 theme 語意層（P4.5：view 內零個 `Style::` 字面）。
fn render_scrollbar(f: &mut Frame, area: Rect, total: usize, pos: usize) {
    let viewport = area.height.saturating_sub(2) as usize;
    if total <= viewport {
        return;
    }
    let mut state = ScrollbarState::new(total).position(pos);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight).style(theme::scrollbar_style()),
        // 上下各留一格給邊框：軌道畫在框內，不蓋掉面板的上下框線
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

/// DETAIL 欄（§2）：**不可聚焦**，永遠是「當前聚焦面板選中項」的唯讀投影。
/// evidence 區與 `c` 的 payload 共用 `action::evidence`（白名單同一份來源）。
fn render_detail(
    f: &mut Frame,
    area: Rect,
    model: &Model,
    live: &LiveIndex,
    blockers: &BlockerIndex,
    app: &App,
    strip: bool,
) {
    let sel = app.selection(model);
    let mut lines: Vec<Line> = Vec::new();
    match &sel {
        Sel::None => lines.push(Line::from("(nothing selected)")),
        Sel::Worker(w) => {
            // 底條模式：breadcrumb 必須壓進**一行**（見 `Areas::detail_strip`）
            let fit = strip.then(|| area.width.saturating_sub(2) as usize);
            for l in worker_detail(model, w, live, fit) {
                lines.push(l);
            }
            // BLOCKER 是與 ACTIVITY 正交的另一軸（§4 雙軸狀態）：獨立一欄，
            // 不與死活混寫
            lines.push(blocker_line(blockers.get(&w.pane)));
        }
        Sel::Task { task, worker } => {
            lines.push(Line::from(format!("task-id: {}", task.id)));
            lines.push(Line::from(format!("from   : {}", task.from)));
            lines.push(Line::from(format!("to     : {}", task.to)));
            lines.push(Line::from(vec![
                Span::raw("status : "),
                Span::styled(task.status.clone(), theme::status_style(&task.status)),
            ]));
            if let Some(w) = worker {
                let l = pane_liveness(live, &w.pane);
                lines.push(Line::from(vec![
                    Span::raw(format!(
                        "pane   : {}  ",
                        if w.pane.is_empty() { "-" } else { &w.pane }
                    )),
                    Span::styled(liveness_word(l), theme::liveness_style(l)),
                ]));
                lines.push(blocker_line(blockers.get(&w.pane)));
            }
        }
    }
    let cmds: Vec<String> = evidence(&sel)
        .into_iter()
        .filter(|e| e.starts_with("agent-bridge"))
        .collect();
    if !cmds.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("evidence:"));
        for c in cmds {
            lines.push(Line::from(format!("  $ {c}")));
        }
    }
    // 窄畫面下**換行而不截斷**：evidence 區的等價 CLI 原文是薄殼原則的憑證
    // （§2），截半條命令等於畫面上沒有那條命令。
    f.render_widget(
        Paragraph::new(lines)
            .block(panel_block("DETAIL", false))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// `<label><liveness_word>` 一行，字面在後、單獨上色（label 不帶語意色）。
fn liveness_line(label: &'static str, l: Liveness) -> Line<'static> {
    Line::from(vec![
        Span::raw(label),
        Span::styled(liveness_word(l), theme::liveness_style(l)),
    ])
}

/// `blocker: <blocker_word>` 一行。只有真的被擋住才上色（`theme::blocker_style`
/// 對 none／occluded／unknown 回 `None`）——把 `none` 畫成紅色是謊報。
fn blocker_line(b: Blocker) -> Line<'static> {
    let word = blocker_word(b);
    Line::from(vec![
        Span::raw("blocker: "),
        match theme::blocker_style(b) {
            Some(s) => Span::styled(word, s),
            None => Span::raw(word),
        },
    ])
}

/// worker DETAIL（題 3）。`origin :` 與 `state  :` **拆成兩列**：前者講的是
/// 「這個 worker 是誰在哪裡 spawn 出來的、那個 window 現在如何」，後者講的是
/// 「這個 worker 自己的 pane 還在不在」。P4.5 之前只有一列，兩件事被壓成一件
/// ——那正是使用者回饋題 2／5／7 的來源。
fn worker_detail(
    model: &Model,
    w: &ab_core::registry::AgentSnapshot,
    live: &LiveIndex,
    // `Some(可用欄寬)`＝這一行不得換行（底條模式）；`None`＝照舊 wrap
    fit: Option<usize>,
) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("name   : {}", w.name)),
        Line::from(format!(
            "pane   : {}",
            if w.pane.is_empty() { "-" } else { &w.pane }
        )),
        Line::from(format!(
            "runtime: {}",
            if w.runtime.is_empty() {
                "-"
            } else {
                &w.runtime
            }
        )),
        Line::from(format!("ready  : {}", w.ready)),
        // lineage 在 origin **之前**：邏輯歸屬是唯一的歸屬軸（P4.7 B5），
        // 物理位置是它下面的一條註腳。說不出世代的列（legacy／manual／
        // invalid）寫 `(standalone)`——**與組標頭同一個字面**（切片 C）：同一
        // 件事在兩個位置用兩種寫法（`-` 與 `(standalone)`），人得自己猜它們
        // 是不是同一回事
        Line::from(format!(
            "lineage: {}",
            match breadcrumb(model, w) {
                // 收縮只發生在底條模式，且是 display-only：節點序列本身不變
                Some(c) => match fit {
                    Some(w) => breadcrumb_line_fit(&c, w.saturating_sub(LINEAGE_LABEL_W)),
                    None => breadcrumb_line(&c),
                },
                None => STANDALONE_LABEL.to_string(),
            }
        )),
        Line::from(format!(
            "origin : {}",
            window_detail(live, &origin_label(w))
        )),
        liveness_line("state  : ", pane_liveness(live, &w.pane)),
    ]
}

/// BLOCKER 軸的列標記（§2 雙軸：`running ⛔`——blocker 是**另一軸**，
/// MUST NOT 取代 task status，也 MUST NOT 寫成不存在的 task 狀態字）。
///
/// glyph 後面帶英文字面（`blocked`／`copy-mode`）而不是只有 emoji：
/// `capture-pane` 的特徵字串斷言吃不到樣式，也吃不準 emoji 的寬度——標記
/// 必須落在字元層才驗得到（同 selection marker 的理由，§2）。
/// `Unknown` 一律**不畫**：沒有訊號 ≠ 沒有 blocker，畫成空白比畫成「無」誠實
/// （DETAIL 欄仍逐字寫出 unknown）。
pub(crate) fn blocker_mark(b: Blocker) -> &'static str {
    match b {
        Blocker::Prompt => "  ⛔blocked",
        Blocker::Occluded => "  👁copy-mode",
        Blocker::None | Blocker::Unknown => "",
    }
}

/// BLOCKER 軸的字面（DETAIL／`i` 摘要頁用；三態不得壓成兩態）。
fn blocker_word(b: Blocker) -> &'static str {
    match b {
        Blocker::None => "none",
        Blocker::Prompt => "permission/plan prompt (blocked)",
        Blocker::Occluded => "occluded (copy-mode: a human is reading)",
        Blocker::Unknown => "unknown",
    }
}

/// 三態死活的字面（`unknown` MUST NOT 寫成 dead，§5 顯示紀律）。
fn liveness_word(l: Liveness) -> &'static str {
    match l {
        Liveness::Live => "live",
        Liveness::Dead => "dead",
        Liveness::Unknown => "unknown",
    }
}

/// `r` 的全螢幕 pager。內容列由 `action::pager_lines` 組好（bytes → 字串的
/// lossy 轉換在那裡做一次），這裡只負責**樣式投影**與畫。
fn render_pager(f: &mut Frame, app: &App) {
    let Some(p) = &app.pager else { return };
    let area = f.area();
    f.render_widget(Clear, area);
    f.render_widget(
        pager_widget(p, highlight_pager(&crate::action::pager_lines(p))),
        area,
    );
}

/// pager 的外框與捲動位置。**高亮與不高亮共用這一份**：gate (e) 的字元層
/// 比對要能把「同一份 bytes、只差樣式」這件事驗到底，兩條路徑各寫一份外框
/// 就會比到別的東西（見 `highlighting_never_changes_the_character_layer`）。
fn pager_widget(p: &crate::app::Pager, lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("read (read-only, full text)"),
        )
        .scroll((p.scroll as u16, 0))
}

/// 一段 fenced code block 的開場圍籬。
struct Fence {
    /// 圍籬字元（`` ` `` 或 `~`）：**閉合必須用同一種**，否則
    /// ` ```rust ` 段落裡的 `~~~` 會把它提早關掉
    ch: u8,
    /// 開場圍籬長度：閉合圍籬 MUST 不短於它（CommonMark）
    len: usize,
    /// info string 是不是 `diff`（唯二會染 `+`／`-` 的情境之一）
    diff: bool,
}

/// pager 的 **markdown-lite 樣式投影**（P4.6 切片 D，gate (e)）。
///
/// 契約只有一條、但是硬的：**只加樣式，不動字元**。每一列切出的 span 逐段
/// 都是原字串的切片，串起來與原列**逐字相同**（含空白與 tab）。不插入、不
/// 刪除、不重排——P2 的 read bytes gate 依賴這件事。
///
/// 「lite」的意思是**逐行判定＋一個 fence 狀態機**，不引入 markdown parser：
/// 這裡要的是「一眼看出結構」，不是正確渲染 markdown。判不出來就不上色，
/// 上錯色比不上色糟。
///
/// diff 染色**嚴格設界**在兩種情境（gate (e) 明文）：
/// 1. ` ```diff ` 圍起來的段落內
/// 2. 出現 `diff --git` 或 `@@ … @@` hunk 標頭之後的明確 diff 區段
///
/// 散文的行首 `+`／`-` **MUST NOT** 染色——清單用 `-` 開頭是常態，把它染成
/// 「刪除行」是直接的誤導。
pub(crate) fn highlight_pager(lines: &[String]) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut fence: Option<Fence> = None;
    let mut in_diff = false;

    for raw in lines {
        let line = raw.as_str();

        // ---- 1. fence 內：整段 code，只認閉合圍籬 ----
        if let Some(f) = &fence {
            let closing = fence_marker(line)
                .is_some_and(|(ch, len, info)| ch == f.ch && len >= f.len && info.is_empty());
            // 未閉合的 fence **不得把剩餘全文吃掉**：ATX 標題與中繼標頭是強
            // 結構訊號，撞上就視為段落已經結束。這是刻意偏離 CommonMark
            // （那裡未閉合的 fence 吃到文件結尾）——一個落單的 ``` 把整份回覆
            // 連同底下的鍵位提示都染成 code，損害遠大於「少染幾行」。
            if !closing && (heading_hashes(line).is_some() || meta_key_len(line).is_some()) {
                fence = None;
            } else {
                let spans = match diff_style(line) {
                    Some(st) if f.diff && !closing => vec![Span::styled(line.to_string(), st)],
                    _ => vec![Span::styled(line.to_string(), theme::md_code_style())],
                };
                if closing {
                    fence = None;
                }
                out.push(Line::from(spans));
                continue;
            }
        }

        // ---- 2. diff 區段的進出 ----
        if is_diff_header(line) {
            in_diff = true;
        } else if in_diff && !continues_diff(line) {
            // 區段結束：這一列照散文規則重新判定（下面的分支會處理）
            in_diff = false;
        }

        // ---- 3. 逐行判定（順序即優先序）----
        let spans = if let Some((ch, len, info)) = fence_marker(line) {
            fence = Some(Fence {
                ch,
                len,
                diff: info.eq_ignore_ascii_case("diff"),
            });
            in_diff = false;
            vec![Span::styled(line.to_string(), theme::md_code_style())]
        } else if heading_hashes(line).is_some() {
            in_diff = false;
            vec![Span::styled(line.to_string(), theme::md_heading_style())]
        } else if let Some(n) = meta_key_len(line) {
            in_diff = false;
            split_styled(line, 0, n, theme::md_meta_key_style())
        } else if let Some(st) = diff_style(line).filter(|_| in_diff) {
            vec![Span::styled(line.to_string(), st)]
        } else if let Some((start, end)) = list_marker(line) {
            split_styled(line, start, end, theme::md_list_marker_style())
        } else {
            vec![Span::raw(line.to_string())]
        };
        out.push(Line::from(spans));
    }
    out
}

/// `line[start..end]` 上色、其餘原色。**三段都是原字串的切片**，串起來逐字
/// 等於 `line`（空段落不產生 span，只是少一個空 `Span`，字元層不變）。
fn split_styled(
    line: &str,
    start: usize,
    end: usize,
    style: ratatui::style::Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(3);
    if start > 0 {
        spans.push(Span::raw(line[..start].to_string()));
    }
    spans.push(Span::styled(line[start..end].to_string(), style));
    if end < line.len() {
        spans.push(Span::raw(line[end..].to_string()));
    }
    spans
}

/// 行首的 fence 標記 → `(圍籬字元, 長度, info string)`。
///
/// CommonMark 允許最多三格縮排、圍籬至少三個字元。反引號圍籬的 info string
/// 不得再含反引號——沒有這條，一列 `` `a` 與 `b` `` 就會被當成 fence 開場。
fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = match rest.as_bytes().first()? {
        b'`' => b'`',
        b'~' => b'~',
        _ => return None,
    };
    let len = rest.bytes().take_while(|&b| b == ch).count();
    if len < 3 {
        return None;
    }
    let info = rest[len..].trim();
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

/// ATX 標題的 `#` 個數。**須行首**（不吃縮排）且 `#` 之後必須是空白或行尾
/// ——`#hashtag` 不是標題，`a # b` 更不是。
fn heading_hashes(line: &str) -> Option<usize> {
    if !line.starts_with('#') {
        return None;
    }
    let n = line.bytes().take_while(|&b| b == b'#').count();
    if n > 6 {
        return None;
    }
    match line.as_bytes().get(n) {
        None | Some(b' ') | Some(b'\t') => Some(n),
        _ => None,
    }
}

/// `agent-bridge read`／`receive` 標頭列的 key 白名單。
///
/// 用白名單而不是「任何 `word:` 開頭」：後者會把散文的 `note: 見下` 一起染，
/// 而中繼標頭的重點正是「這幾行不是內文」。
const META_KEYS: [&str; 4] = ["task-id", "from", "to", "working_directory"];

/// 中繼標頭 key（含冒號）的長度。**須行首**，冒號後必須是空白或行尾。
fn meta_key_len(line: &str) -> Option<usize> {
    META_KEYS.iter().find_map(|k| {
        let n = k.len() + 1;
        let head = line.get(..n)?;
        if !head.starts_with(k) || !head.ends_with(':') {
            return None;
        }
        match line.as_bytes().get(n) {
            None | Some(b' ') | Some(b'\t') => Some(n),
            _ => None,
        }
    })
}

/// 清單 marker 的 `(起, 迄)`（不含其後的空白）。`- ` / `* ` / `+ ` 與
/// `1.` / `1)` 兩族；marker 後**必須**有空白，否則 `-5 度` 這種散文會中招。
fn list_marker(line: &str) -> Option<(usize, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let b = rest.as_bytes();
    if matches!(b.first(), Some(b'-' | b'*' | b'+')) && b.get(1) == Some(&b' ') {
        return Some((indent, indent + 1));
    }
    let digits = rest.bytes().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && matches!(b.get(digits), Some(b'.' | b')')) && b.get(digits + 1) == Some(&b' ')
    {
        return Some((indent, indent + digits + 1));
    }
    None
}

/// 明確的 diff 區段起點：`diff --git` 或 `@@ … @@` hunk 標頭。
fn is_diff_header(line: &str) -> bool {
    line.starts_with("diff --git") || (line.starts_with("@@") && line[2..].contains("@@"))
}

/// 這一列還在 diff 區段裡嗎。
///
/// unified diff 的每一列都帶前綴（context 是空格、新增 `+`、刪除 `-`、hunk
/// `@@`、`\ No newline` 是反斜線）。**空行視為區段結束**：git 的 context 行
/// 即使內容是空的也會帶一個空格，真正的空行代表 diff 貼完了。判錯的方向是
/// 「少染」——那正是這一項要的保守方向。
fn continues_diff(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    matches!(line.as_bytes()[0], b' ' | b'+' | b'-' | b'@' | b'\\')
        || line.starts_with("diff --git")
        || line.starts_with("index ")
}

/// 行首 `+`／`-` 的 diff 色。**呼叫端負責先確認情境**（fence info 是 diff，
/// 或已在明確 diff 區段內）——這個函式本身不判斷情境。
fn diff_style(line: &str) -> Option<ratatui::style::Style> {
    match line.as_bytes().first()? {
        b'+' => Some(theme::diff_add_style()),
        b'-' => Some(theme::diff_del_style()),
        _ => None,
    }
}

/// `i` 的 worker 摘要頁（內容由 `action::info_page` 組好，這裡只負責畫）。
fn render_info(f: &mut Frame, lines: &[String]) {
    let h = (lines.len() as u16).saturating_add(2);
    let w = lines
        .iter()
        .map(|l| l.chars().count() as u16 + 4)
        .max()
        .unwrap_or(40)
        .max(40);
    popup(
        f,
        w,
        h,
        "worker info (read-only)",
        lines.iter().map(|l| Line::from(l.clone())).collect(),
    );
}

/// `L` 的尾行預覽（內容由 `action::peek_page` 組好，這裡只負責畫）。
///
/// **底部對齊**是這一頁與 `i` 唯一的差別，也是它必須另寫一份的理由：畫不下時
/// `Paragraph` 截的是尾端，而尾行預覽要看的正是最後那幾行——沿用 `render_info`
/// 等於把唯一有用的部分切掉。截斷過就在頂端補一行標記，人才知道上面還有東西。
fn render_peek(f: &mut Frame, p: &crate::app::PeekView) {
    let area = f.area();
    // **寬高一律先在 `usize` 裡算完再夾回 `u16`**（跨廠複核 M4／verifier 同時
    // 抓到）：byte 界 64 KiB ＋ `-J` 併行之後單行寬度可以逼近 65535，`as u16`
    // 在 debug build（overflow-checks 開著）會 panic，恰好 65536 則靜靜變成 0。
    // 這條路徑的輸入來自別人的畫面，不能假設它的寬度落在 u16 裡
    let longest = p.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let w = fit_u16(longest + 4).max(40).min(area.width.max(1));
    let inner = w.saturating_sub(2).max(1) as usize;
    // **每行截到一列寬**（不換行）：這一頁是「最後幾行」，一條 64 KiB 的長行
    // 若讓它折行，光它自己就吃掉整個 overlay，尾端反而看不見——那正是這一頁
    // 存在的理由。截掉的部分以 `…` 標出來，不假裝那是全部
    let fit = |s: &String| crate::model::truncate_with_ellipsis(s, inner);
    // 邊框 2 列＋提示 1 列＋（截斷時）標記 1 列
    let chrome = 3 + usize::from(p.truncated);
    let room = (area.height as usize).saturating_sub(chrome).max(1);
    let start = p.lines.len().saturating_sub(room);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if p.truncated {
        lines.push(Line::from(fit(&format!(
            "\u{2026} truncated at {} KiB (byte bound)",
            ab_core::config::TAIL_MAX_BYTES / 1024
        ))));
    }
    lines.extend(p.lines[start..].iter().map(|l| Line::from(fit(l))));
    lines.push(Line::from(fit(&"press any key to close".to_string())));
    let h = fit_u16(lines.len() + 2);
    popup(f, w, h, &p.title, lines);
}

/// `usize` 的版面尺寸 → `u16`（超出就夾在 `u16::MAX`）。
///
/// 存在的理由只有一個：`as u16` 是靜默窄化，而這裡的輸入來自別人的 pane 畫面。
/// 夾在最大值是安全的——`popup` 接著還會夾回實際畫面尺寸。
fn fit_u16(v: usize) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

/// 全域鍵（與選中列無關的那一段）。`?` 頁與 footer 共用同一份字面。
const GLOBAL_KEYS: &str = "Tab/S-Tab panes \u{b7} j/k (\u{2193}\u{2191}) move \u{b7} PgUp/PgDn page \u{b7} Home/End \u{b7} / filter \u{b7} S scope \u{b7} Esc clear warn \u{b7} ? keys \u{b7} q quit";

/// **當前選中列**有效的鍵（P4.6 切片 B 的 contextual footer）。
///
/// 為什麼要 contextual：舊 footer 把七個鍵一次列出來，其中大半在當下那一列是
/// 無效的——人照著按，換來的是一行「x only acts on task rows」。列出來的鍵
/// MUST 是按下去真的會動的鍵。
///
/// footer 第一段與 `?` 頁的「current row」區共用這一份正本：分成兩份寫，改鍵
/// 位時一定有一邊漂掉。
///
/// **判定不在這裡**：能不能按由 `app::row_caps` 說了算，而 `dispatch_key` 讀
/// 的是同一份 caps（審查 minor：提示與可用性脫鉤時，畫面會說一個按下去只會
/// 回拒絕訊息的鍵）。這裡只負責把 caps 翻成字。
fn row_keys(model: &Model, app: &App) -> String {
    let caps = crate::app::row_caps(app, model);
    let mut parts: Vec<&str> = Vec::new();
    match caps.enter {
        EnterAct::Focus => parts.push("Enter focus pane"),
        // `r` 是 Enter 的 alias（同一條路徑），一段字講完兩個鍵
        EnterAct::Read => parts.push("Enter/r read full text"),
        EnterAct::None => {}
    }
    if caps.info {
        parts.push("i info");
    }
    if caps.peek {
        parts.push("L tail preview");
    }
    if caps.copy {
        parts.push("c copy evidence");
    }
    if caps.cancel {
        parts.push("x cancel");
    }
    if caps.evict {
        parts.push("e evict");
    }
    if !parts.is_empty() {
        return parts.join(" \u{b7} ");
    }
    // 一個鍵都沒有：選著一列與根本沒選中列是兩件不同的事，說法要分開——
    // 後者人得先按 j／k。P4.7 切片 B 之後 worker／task 列都至少有一個鍵，
    // 第二個分支形同保險絲：真的走到了，代表 `row_caps` 漏了一列型。
    match app.selection(model) {
        Sel::None => "(no row selected)".to_string(),
        _ => "(no keys act on this row)".to_string(),
    }
}

/// 一軸的新鮮度字樣（P4.6 切片 C）。三段式，**低噪**是刻意的：
///
/// - 年輕：只寫節奏（`disk 500ms`），不寫數字。正常態每一幀 age 都在跳，逐秒
///   顯示會訓練人忽略這個位置，等真的 stale 了也不會注意到。
/// - 過半程：`disk 2s ago`——開始往 stale 走了，這時的數字才有意義。
/// - 逾門檻：`disk STALE 12s`，且該軸資料同時降級為 unknown（見 lib.rs）。
///
/// 第四種：`age` 是 `None`＝**至今沒有可信的樣本**（tmux 軸專屬，審查 F2）。
/// 那時說不出任何年紀，寫 `tmux unknown` 並比照 stale 上色——畫面上的死活本來
/// 就已經是 unknown，footer 不該在那時顯示一個看起來很新的數字。
///
/// 回傳字串與是否 stale，呼叫端據此決定要不要上色。
fn axis_label(
    name: &str,
    cadence: &str,
    age: Option<Duration>,
    stale_at: Duration,
) -> (String, bool) {
    let Some(age) = age else {
        return (format!("{name} unknown"), true);
    };
    let secs = age.as_secs();
    if age >= stale_at {
        (format!("{name} STALE {secs}s"), true)
    } else if age_is_worth_showing(age, stale_at) {
        (format!("{name} {secs}s ago"), false)
    } else {
        (format!("{name} {cadence}"), false)
    }
}

/// footer 的額外行數（filter 提示列＋copy-mode banner）。
///
/// run loop 量 `panel_heights` 時也要算同一份，否則翻頁的一頁長度會與畫面
/// 差一行。
pub fn footer_extra_rows(model: &Model, app: &App, blockers: &BlockerIndex) -> usize {
    usize::from(filter_line(app).is_some())
        + usize::from(copy_mode_banner(model, app, blockers).is_some())
}

/// filter 的狀態列（`None`＝沒在篩也沒在輸入）。
fn filter_line(app: &App) -> Option<String> {
    if app.filter_input {
        // `\u{2588}` 是游標塊：輸入模式下人得看得出鍵盤此刻被誰吃掉
        Some(format!(" /{}█", app.filter.query))
    } else if app.filter.is_active() {
        // Esc 在命令模式是「清警告」，**不會**清 filter——說明文字照實寫，
        // 不要教人按一個在這裡沒有那個效果的鍵
        Some(format!(
            " filter: {} (press / then Esc to clear)",
            app.filter.query
        ))
    } else {
        None
    }
}

/// copy-mode banner（P4.7 切片 C）。
///
/// 只消費既有的 `BlockerIndex` 快照——`pane_in_mode` 的 bounded 查詢在背景
/// worker 那一輪就做完了，render 路徑一個 tmux 呼叫都不發（那是 §4 bounded-read
/// 的硬條款：畫面每 500ms 重繪，render 一旦能發查詢就等於無界地打 tmux）。
///
/// 三態逐字對應：`Occluded`（`pane_in_mode` 回 true）→ 有 banner；`None`／
/// `Prompt`（回 false）→ 無；`Unknown`（查不到）→ **無**（查不出就不宣稱）。
/// 措辭沿用 `blocker_word` 那一家，不新造第二套詞彙。
fn copy_mode_banner(model: &Model, app: &App, blockers: &BlockerIndex) -> Option<String> {
    let (name, pane) = match app.selection(model) {
        Sel::Worker(w) => (w.name.clone(), w.pane.clone()),
        Sel::Task {
            worker: Some(w), ..
        } => (w.name.clone(), w.pane.clone()),
        _ => return None,
    };
    match blockers.get(&pane) {
        Blocker::Occluded => Some(format!(
            " [info] '{name}' is {} — keys you send may land in the pager",
            blocker_word(Blocker::Occluded)
        )),
        _ => None,
    }
}

fn render_footer(
    f: &mut Frame,
    area: Rect,
    model: &Model,
    app: &App,
    blockers: &BlockerIndex,
    fresh: Freshness,
) {
    // disk 軸恆有值：`Model::load` 不回錯，但「上次**完成**一輪掃描」永遠說得出
    // 口（F3：說得出口的只有 completed scan，不是 success）
    let (disk, disk_stale) = axis_label("disk", "500ms", Some(fresh.disk), DISK_STALE);
    let (tmux, tmux_stale) = axis_label("tmux", "2s", fresh.tmux, TMUX_STALE);
    let mut status = vec![Span::raw(" [")];
    status.push(if disk_stale {
        Span::styled(disk, theme::stale_style())
    } else {
        Span::raw(disk)
    });
    status.push(Span::raw(" \u{b7} "));
    status.push(if tmux_stale {
        Span::styled(tmux, theme::stale_style())
    } else {
        Span::raw(tmux)
    });
    status.push(Span::raw(format!("] {}", app.message)));
    let mut lines = vec![
        Line::from(format!(" {}", row_keys(model, app))),
        Line::from(format!(" {GLOBAL_KEYS}")),
        Line::from(status),
    ];
    // filter 提示列：輸入中畫游標塊，篩選生效但不在輸入中則說得出「現在還篩著」
    // ——沒有這一行，人會把「列變少了」讀成資料不見了
    if let Some(l) = filter_line(app) {
        lines.push(Line::from(l));
    }
    // copy-mode banner：**只讀 blocker 快照**（bounded 查詢在背景 worker 那一
    // 側做過了），render 一律不發 tmux 呼叫
    if let Some(b) = copy_mode_banner(model, app, blockers) {
        lines.push(Line::from(b));
    }
    // sticky 警告（最新的在最下面，人的視線落點）。畫得下幾則就畫幾則，
    // 剩下的以計數帶出——「被覆寫」與「畫面放不下但說得出還有幾則」是兩件事
    let n = app.warnings.len();
    if n > 0 {
        let shown = n.min(WARN_ROWS);
        let hidden = n - shown;
        for (i, w) in app.warnings[n - shown..].iter().enumerate() {
            let text = if i == 0 && hidden > 0 {
                format!(" \u{26a0} ({hidden} older warning(s) not shown) {w}")
            } else {
                format!(" ⚠ {w}")
            };
            lines.push(Line::from(text).style(theme::warning_style()));
        }
        lines.push(Line::from(" (Esc clears warnings)"));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// `x` 的單確認框（§2 薄殼原則：畫面留下等價 CLI 原文；§5：cancel 綁
/// immutable task id，單確認即可）。
fn render_confirm(f: &mut Frame, id: &str) {
    let cmd = format!("$ {}", cancel_cmdline(id));
    let lines = vec![
        Line::from("Confirm to run the equivalent CLI:"),
        Line::from(cmd.clone()),
        Line::from("[y/Enter] run \u{b7} [n/Esc] abort"),
    ];
    let w = (cmd.chars().count() as u16 + 4).max(34);
    popup(f, w, 5, "cancel task", lines);
}

/// `e` 的證據框（§5）。內容由 `action::evict_confirm_lines` 組好，這裡只負責
/// 畫——措辭紀律（「派收尾任務後回收」、無「安全刪除」語彙）的正本在 action 層，
/// 兩邊各寫一份就會有一邊漂移。
fn render_evict_confirm(f: &mut Frame, lines: &[String]) {
    let w = lines
        .iter()
        .map(|l| l.chars().count() as u16 + 4)
        .max()
        .unwrap_or(46)
        .max(46);
    let h = lines.len() as u16 + 2;
    popup(
        f,
        w,
        h,
        "evict (wrap-up task, then reclaim)",
        lines.iter().map(|l| Line::from(l.clone())).collect(),
    );
}

/// `?` 頁：**兩區**——全域鍵一區、當前選中列一區（P4.6 切片 B）。
///
/// 分區的理由與 contextual footer 同一條：混成一張平表時，人分不出哪些鍵此刻
/// 按得動。第二區與 footer 第一段共用 `row_keys`，其後再補一行「為什麼某個鍵
/// 不在上面」的說明——那是 footer 一行塞不下、但人真的會問的東西。
fn render_help(f: &mut Frame, model: &Model, app: &App) {
    let mut lines = vec![
        Line::from("global:"),
        Line::from(format!("  {GLOBAL_KEYS}")),
        Line::from(""),
        Line::from("current row:"),
        Line::from(format!("  {}", row_keys(model, app))),
    ];
    match app.selection(model) {
        Sel::Worker(_) => lines.push(Line::from(
            "  x is task-rows only (cancel needs a unique task id)",
        )),
        Sel::Task { task, .. } => {
            if crate::model::is_terminal_status(&task.status) {
                lines.push(Line::from(
                    "  terminal task: x does nothing (no transition left to cancel)",
                ));
            } else {
                lines.push(Line::from(
                    "  non-terminal task: read is refused until it is answered",
                ));
            }
        }
        Sel::None => {}
    }
    // footer 那兩軸各自證明得了什麼，逐字說清楚（審查 F3）。**兩軸的說法刻意
    // 不同**：tmux 查詢有明確的失敗訊號，disk 沒有——`Model::load` 讀不到就是
    // 空快照、單檔損壞逕自跳過，它只證明得了「這一輪掃描跑完了」。把它也寫成
    // success，UI 就是在宣稱一件它證明不了的事。
    lines.push(Line::from(""));
    lines.push(Line::from("freshness (footer):"));
    lines.push(Line::from(
        "  disk = age of the last completed scan (a scan can complete with unreadable entries skipped)",
    ));
    lines.push(Line::from(
        "  tmux = age of the last successful query round; unknown = no successful round yet",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from("press any key to close"));
    // 寬度要容得下最長那行：popup 雖然會換行，但把一條規則折成兩段仍然難讀
    // 118 而不是 100：全域鍵那一行先後加入翻頁鍵與 `/`／`s`（切片 C），窄一點
    // 就會被 popup 折成兩段。畫面不足這個寬度時 `popup` 自己會夾回去並換行
    popup(
        f,
        118,
        lines.len() as u16 + 2,
        "keys (current selection)",
        lines,
    );
}

/// 一列的組裝：語意色由各 Span 自帶，選取狀態是**整列的底樣式**。
///
/// 選取用背景色而非 `Modifier::REVERSED`：REVERSED 會把 fg／bg 對調，語意色
/// 跟著被翻成背景而失去意義。`Line::style` 是底、Span 樣式 patch 在上，所以
/// 選取列上每個 Span 的 fg 原樣保留（見 `theme::selected_row_style`）。
fn styled(spans: Vec<Span<'static>>, selected: bool) -> Line<'static> {
    let line = Line::from(spans);
    if selected {
        line.style(theme::selected_row_style())
    } else {
        line
    }
}

fn panel_block(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(theme::panel_border_type(focused))
        .border_style(theme::panel_border_style(focused))
        .title(Span::styled(
            title.to_string(),
            theme::panel_title_style(focused),
        ))
}

/// 讓 selection 保持可見的最小捲動（P1 fixture 小，這裡只做下緣跟隨）。
fn scroll_offset(idx: usize, area: Rect) -> u16 {
    let inner_h = area.height.saturating_sub(2) as usize; // 邊框上下各一
    if inner_h == 0 || idx < inner_h {
        0
    } else {
        (idx + 1 - inner_h) as u16
    }
}

fn popup(f: &mut Frame, w: u16, h: u16, title: &str, lines: Vec<Line<'static>>) {
    let area = f.area();
    let w = w.min(area.width);
    // 換行後每行可能佔多列：估一下需要幾列，免得 80／100 欄下把 evict 的
    // 等價 CLI（約 112 字元）截掉尾端的 generation——那條命令原文是薄殼原則
    // 的憑證，截半條等於畫面上沒有它（codex 複核 minor #5）
    let inner = w.saturating_sub(2).max(1) as usize;
    // `rows` 由**別人畫面的行寬**推出來（尾行預覽），可能遠超過 u16：先在
    // `usize` 裡算完再夾（跨廠複核 M4）。`h.min(area.height)` 隨後還會夾一次
    let rows: usize = lines.iter().map(|l| l.width().div_ceil(inner).max(1)).sum();
    let h = h.max(fit_u16(rows + 2));
    let h = h.min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title.to_string()),
            )
            // 窄畫面下**換行而不截斷**（同 DETAIL 欄的處置）：確認框裡的等價
            // CLI 原文是人決定按不按 y 的依據
            .wrap(Wrap { trim: false }),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ab_core::registry::AgentSnapshot;
    use ab_core::task::InFlight;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};
    use std::collections::HashMap;

    fn worker(name: &str, pane: &str, runtime: &str, owner: &str, spawned: bool) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            pane: pane.to_string(),
            runtime: runtime.to_string(),
            owner: owner.to_string(),
            ready: "ready".to_string(),
            // 每一列各自的舊式 tag（**不共用**）：共用的話 B2 修正輪 H2 的
            // 重複偵測會把它們全判成 invalid，這份 fixture 想要的是 legacy
            spawn_tag: format!("t1-{name}"),
            // 比 fixture 的 task `created_at` 早一天：切片 C 的掛載判準是
            // **嚴格晚於** registered_at（同秒＝不可證），註冊時間與第一筆
            // task 撞在同一秒的話，內嵌 task 列會整條消失
            registered_at: "2026-07-31T00:00:00Z".to_string(),
            spawned,
            corrupt: false,
            // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
            lineage_root: None,
            parent_agent: None,
        }
    }

    /// 測試用：由 task-id 前綴反推 ISO `created_at`（真實資料裡兩者同源，
    /// **不一致是例外**——那條例外由 `attachment_needs_...` 專門驗）。
    fn iso_from_id(id: &str) -> String {
        let s: Vec<char> = id.chars().take(16).collect();
        if s.len() < 16 {
            return String::new();
        }
        let g = |a: usize, b: usize| -> String { s[a..b].iter().collect() };
        format!(
            "{}-{}-{}T{}:{}:{}Z",
            g(0, 4),
            g(4, 6),
            g(6, 8),
            g(9, 11),
            g(11, 13),
            g(13, 15)
        )
    }

    fn task(id: &str, to: &str, status: &str) -> InFlight {
        InFlight {
            created_at: iso_from_id(id),
            id: id.to_string(),
            from: "boss".to_string(),
            to: to.to_string(),
            status: status.to_string(),
        }
    }

    /// fixture 的 `groups` 一律由 `group_by_lineage` 推導（與 `Model::load`
    /// 同一條路徑）。
    fn with_groups(mut m: Model) -> Model {
        m.groups = crate::model::group_by_lineage(&m.workers);
        m
    }

    /// 一個涵蓋全部語意的最小 fixture：五種 status、三種死活、一個 blocker、
    /// 一個選取列、focus／非 focus 面板各一。純資料，**不碰 tmux 與磁碟**。
    fn fixture() -> (Model, LiveIndex, BlockerIndex, App) {
        let model = with_groups(Model {
            groups: Vec::new(),
            workers: vec![
                worker("alive-w", "%1", "claude", "s:@1", true),
                worker("dead-w", "%2", "codex", "s:@1", true),
                worker("occl-w", "%5", "agy", "s:@1", true),
                worker("manual-w", "%3", "agy", "", false),
                worker("other-w", "%4", "claude", "s:@9", true),
            ],
            tasks: vec![task("20260801T000000Z-aaaa", "alive-w", "running")],
            recent: vec![
                task("20260801T000000Z-aaaa", "alive-w", "running"),
                task("20260801T000001Z-bbbb", "alive-w", "failed"),
                task("20260801T000002Z-cccc", "dead-w", "completed"),
                task("20260801T000003Z-dddd", "alive-w", "queued"),
                task("20260801T000004Z-eeee", "dead-w", "cancelled"),
                task("20260801T000005Z-ffff", "alive-w", "delivered"),
            ],
            recent_truncated: false,
        });
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), vec![("s".to_string(), "@1".to_string())]);
        panes.insert("%5".to_string(), vec![("s".to_string(), "@1".to_string())]);
        // `@1` 還在（名叫 `main`）、`@9` 不在——origin 標籤的 live／gone 兩態
        // 都在同一張畫面上驗得到
        let live = LiveIndex {
            panes: Some(panes),
            windows: Some(HashMap::from([(
                "@1".to_string(),
                vec![("s".to_string(), "main".to_string())],
            )])),
            ..LiveIndex::unknown()
        };
        let mut bl = HashMap::new();
        bl.insert("%1".to_string(), Blocker::Prompt);
        bl.insert("%2".to_string(), Blocker::None);
        bl.insert("%5".to_string(), Blocker::Occluded);
        let blockers = BlockerIndex { panes: Some(bl) };

        let mut app = App::new();
        app.panel = Panel::Workers; // WORKERS focus、TASKS 非 focus
        app.row_idx = 0; // 選中第一列（alive-w，身上帶 blocker）
        // 警告文字刻意用單寬 ASCII：斷言要靠 `find` 逐格比對（見其註解）
        app.warnings.push("WARN-fixture".to_string());
        (model, live, blockers, app)
    }

    /// WORKERS 欄的列序（`worker_rows` 把任務交錯在其 worker 之下）：
    /// 0=alive-w（Prompt）、1=其 running 任務、2=dead-w（None）、3=occl-w（Occluded）
    const ROW_DEAD_W: usize = 2;
    const ROW_OCCL_W: usize = 3;

    fn draw() -> Buffer {
        draw_with(|_| {})
    }

    /// 同一份 fixture，由呼叫端先調 selection／焦點再畫。
    ///
    /// DETAIL 欄只投影**當前選中項**，所以「`none` 沒被染紅」這類斷言必須真的
    /// 把選取移過去才驗得到——固定 selection 的單一 `draw()` 驗不出來
    /// （審查 minor #3：原本的測試名說驗了 none，實際一條斷言都沒有）。
    fn draw_with(tweak: impl FnOnce(&mut App)) -> Buffer {
        draw_model_with(|_, app| tweak(app))
    }

    /// 連 read model 一起改的版本：空 scope、收件人已不在 registry 的 task
    /// 這類形狀，`fixture()` 造不出來（它刻意是一份「什麼都正常」的畫面）。
    fn draw_model_with(tweak: impl FnOnce(&mut Model, &mut App)) -> Buffer {
        draw_fresh(Freshness::default(), tweak)
    }

    /// 再加一層：連 freshness 都由呼叫端指定（stale 顯示與降級要驗得到）。
    /// 預設 `Freshness::default()`＝兩軸都是 0，也就是「剛更新」。
    fn draw_fresh(fresh: Freshness, tweak: impl FnOnce(&mut Model, &mut App)) -> Buffer {
        draw_at(120, 40, fresh, tweak)
    }

    /// 指定終端機尺寸的版本：窄畫面（DETAIL 走整寬底條）只有這樣才畫得出來。
    fn draw_at(
        w: u16,
        h: u16,
        fresh: Freshness,
        tweak: impl FnOnce(&mut Model, &mut App),
    ) -> Buffer {
        let (mut model, live, blockers, mut app) = fixture();
        tweak(&mut model, &mut app);
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, &model, &live, &blockers, &app, fresh))
            .unwrap();
        t.backend().buffer().clone()
    }

    /// blocker 快照也由呼叫端指定（copy-mode banner 的三態要各驗一次）。
    fn draw_blockers(bl: BlockerIndex, tweak: impl FnOnce(&mut App)) -> Buffer {
        let (model, live, _, mut app) = fixture();
        tweak(&mut app);
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render(f, &model, &live, &bl, &app, Freshness::default()))
            .unwrap();
        t.backend().buffer().clone()
    }

    /// 找出 `needle` 在畫面上第一次出現的位置（回傳起點 cell 座標）。
    ///
    /// 逐 cell 比對而非把整行 join 成字串再找 byte offset：後者在 CJK／emoji
    /// 下 byte 位置對不回 cell 座標，斷言會抓到隔壁格。
    ///
    /// **needle 請只用單寬字元**：雙寬字元（`⛔`、CJK）在 buffer 裡佔兩格，
    /// 第二格被 `Cell::reset()` 寫成空白，逐格拼出來的字串會多一個空格而永遠
    /// 對不上。要驗雙寬 glyph 本身，用 ASCII 錨定位後直接檢查前面那一格。
    fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
        find_in(buf, needle, 0, buf.area().width)
    }

    /// 限定欄位範圍的 `find`。
    ///
    /// 需要它是因為同一個字面會同時出現在 WORKERS 的列標記與 DETAIL 的字面
    /// （`copy-mode`／`blocked`），全畫面搜到的是「y 比較小的那個」——那取決於
    /// 版面而不是語意，斷言會驗到不是自己想驗的那一格。
    fn find_in(buf: &Buffer, needle: &str, x0: u16, x1: u16) -> (u16, u16) {
        let area = buf.area();
        for y in 0..area.height {
            for x in x0..x1.min(area.width) {
                let mut acc = String::new();
                let mut xx = x;
                while xx < area.width && acc.chars().count() < needle.chars().count() + 4 {
                    acc.push_str(buf[(xx, y)].symbol());
                    if acc.starts_with(needle) {
                        return (x, y);
                    }
                    xx += 1;
                }
            }
        }
        panic!("畫面上找不到「{needle}」");
    }

    fn style_at(buf: &Buffer, needle: &str) -> ratatui::style::Style {
        let (x, y) = find(buf, needle);
        buf[(x, y)].style()
    }

    /// WORKERS／DETAIL 兩欄的 x 起點（120 欄、**兩欄**版面——ORIGINS 退場
    /// 之後中欄從 0 起）。
    const WORKERS_X0: u16 = 0;
    const DETAIL_X0: u16 = 120 - DETAIL_W;

    fn style_in_detail(buf: &Buffer, needle: &str) -> ratatui::style::Style {
        let (x, y) = find_in(buf, needle, DETAIL_X0, 120);
        buf[(x, y)].style()
    }

    fn style_in_workers(buf: &Buffer, needle: &str) -> ratatui::style::Style {
        let (x, y) = find_in(buf, needle, WORKERS_X0, DETAIL_X0);
        buf[(x, y)].style()
    }

    fn text(buf: &Buffer) -> String {
        let area = buf.area();
        let mut s = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    #[ignore = "字元層對照用：AB_DUMP=<path> 時把畫面字元倒進檔案"]
    fn dump_character_layer() {
        if let Ok(p) = std::env::var("AB_DUMP") {
            std::fs::write(p, text(&draw())).unwrap();
        }
    }

    /// status 五組語意色，一組一條斷言（權威字本身上色，不是整列）。
    #[test]
    fn status_words_carry_their_semantic_colour() {
        let buf = draw();
        assert_eq!(style_at(&buf, "running").fg, Some(Color::Cyan));
        assert_eq!(style_at(&buf, "completed").fg, Some(Color::Green));
        assert_eq!(style_at(&buf, "failed").fg, Some(Color::Red));
        assert_eq!(style_at(&buf, "queued").fg, Some(Color::Yellow));
        assert_eq!(style_at(&buf, "cancelled").fg, Some(Color::DarkGray));
        assert_eq!(style_at(&buf, "delivered").fg, Some(Color::Yellow));
    }

    /// pane 死活三態各自的色（WORKERS 欄的後綴）。`Unknown` MUST 與 `Dead`
    /// 不同色——它們是不同的事實（§5 三態不得壓成兩態）。
    ///
    /// **ORIGINS 欄不在這條的範圍內**：P4.6 之後那一欄一個 liveness glyph 都
    /// 沒有（見 `origin_rows_carry_no_liveness_glyph`）。
    #[test]
    fn liveness_glyphs_keep_three_states_apart() {
        let buf = draw();
        assert_eq!(style_at(&buf, "✗dead").fg, Some(Color::Red));
        // DETAIL 的 pane state 字面。**要指名 `state  :` 那一列**：
        // `origin :` 那一列也會出現 `live`，而它 y 比較小、且刻意不上色
        let (x, y) = find_in(&buf, "state  : live", DETAIL_X0, 120);
        assert_eq!(
            buf[(x + "state  : ".len() as u16, y)].style().fg,
            Some(Color::Green)
        );
        let dead = draw_with(|a| a.row_idx = ROW_DEAD_W);
        let (dx, dy) = find_in(&dead, "state  : dead", DETAIL_X0, 120);
        assert_eq!(
            dead[(dx + "state  : ".len() as u16, dy)].style().fg,
            Some(Color::Red)
        );
    }

    /// worker DETAIL 仍留著 origin（物理位置）那一列——ORIGINS **面板**退場
    /// 不等於證據消失：origin 是「這個 worker 現在坐在哪」，那是事實；當成
    /// 歸屬軸才是謊（§11 根因）。
    #[test]
    fn detail_keeps_origin_as_evidence_next_to_agent_state() {
        // worker 列（alive-w，origin s:@1 還活著）
        let buf = draw();
        let (x, y) = find_in(&buf, "origin : s:main (@1, live)", DETAIL_X0, 120);
        assert!(x >= DETAIL_X0 && y > 0);
        // 同一張 DETAIL 上，agent 自己的 pane 死活是**另一列**——兩者不得混寫
        find_in(&buf, "state  : live", DETAIL_X0, 120);
    }

    /// 畫成紅色是謊報。
    #[test]
    fn blocker_is_red_bold_and_none_is_not_coloured() {
        let buf = draw();
        // `⛔blocked`：用 ASCII 部分定位，再回頭檢查雙寬 glyph 那一格
        let (x, y) = find(&buf, "blocked");
        let s = buf[(x, y)].style();
        assert_eq!(s.fg, Some(Color::Red));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        let glyph_cell = &buf[(x - 2, y)]; // ⛔ 佔兩格，第二格是 reset 出來的空白
        assert_eq!(glyph_cell.symbol(), "⛔");
        assert_eq!(glyph_cell.style().fg, Some(Color::Red));
        assert!(glyph_cell.style().add_modifier.contains(Modifier::BOLD));

        // DETAIL 欄的 blocker 字面同色（選中的是帶 blocker 的 worker）
        let d = style_in_detail(&buf, "permission/plan prompt");
        assert_eq!(d.fg, Some(Color::Red));
        assert!(d.add_modifier.contains(Modifier::BOLD));

        // `none`：選中沒有 blocker 的 worker，DETAIL 的 none MUST NOT 被染紅／加粗
        // ——把「沒有 blocker」畫成警報是謊報（審查 minor #3）
        let buf_none = draw_with(|a| a.row_idx = ROW_DEAD_W);
        let n = style_in_detail(&buf_none, "none");
        assert_ne!(n.fg, Some(Color::Red), "none MUST NOT 上 blocker 色");
        assert!(!n.add_modifier.contains(Modifier::BOLD));

        // `occluded`：人正在看，不是異常，同樣 MUST NOT 上警報色
        let buf_occ = draw_with(|a| a.row_idx = ROW_OCCL_W);
        let o = style_in_detail(&buf_occ, "occluded");
        assert_ne!(o.fg, Some(Color::Red), "occluded MUST NOT 上 blocker 色");
        assert!(!o.add_modifier.contains(Modifier::BOLD));
        // WORKERS 欄該列的 copy-mode 標記也不得染紅
        let m = style_in_workers(&buf_occ, "copy-mode");
        assert_ne!(m.fg, Some(Color::Red));
        assert!(!m.add_modifier.contains(Modifier::BOLD));
    }

    /// **選取背景 MUST NOT 與任何語意前景同色**，否則選中那一列的字直接隱形。
    ///
    /// 兩個實際會撞的組合（審查 major #1）：`cancelled` 的 status 色與
    /// `Liveness::Unknown` 的 glyph 色都是 DarkGray——選取背景一旦也是 DarkGray，
    /// 選中就等於把該格擦掉。斷言只問一件事：語意 cell 的 fg ≠ bg。
    #[test]
    fn selection_background_never_swallows_a_semantic_foreground() {
        // cancelled task 列（TASKS 欄，選中它）
        let buf = draw_with(|a| {
            a.panel = Panel::Tasks;
            a.task_idx = 4; // recent 第五筆＝cancelled
        });
        // 限定在中欄：DETAIL 也會投影出 `status : cancelled`，而它 y 比較小，
        // 全畫面搜會先命中那一格（不是選取列）
        let (x, y) = find_in(&buf, "cancelled", WORKERS_X0, DETAIL_X0);
        let s = buf[(x, y)].style();
        assert_eq!(s.bg, Some(Color::Blue), "選取列背景");
        assert_ne!(s.fg, s.bg, "cancelled 的 fg 與選取 bg 同色＝字隱形");
    }

    /// 非 focus 面板的**標題**不得被邊框的 DarkGray 一起壓暗（審查 minor #2）：
    /// 面板叫什麼名字是導航資訊，不是裝飾。
    #[test]
    fn unfocused_panel_title_is_not_dimmed_with_its_border() {
        let buf = draw();
        // TASKS 非 focus（fixture 的焦點在 WORKERS）：邊框 DarkGray，標題不該是
        let (x, y) = find(&buf, "TASKS");
        assert_ne!(
            buf[(x, y)].style().fg,
            Some(Color::DarkGray),
            "非 focus 面板的標題被邊框色壓暗了"
        );
        // 非 focus 面板的邊框仍應暗（TASKS 的左上角）
        let tasks_top = layout(Rect::new(0, 0, 120, 40), 1, 0).tasks.top();
        assert_eq!(
            buf[(0, tasks_top)].style().fg,
            Some(Color::DarkGray),
            "邊框仍應暗"
        );
    }

    /// focus／非 focus 面板：粗框＋BOLD vs DarkGray 邊框。
    #[test]
    fn focused_panel_is_distinguishable_from_the_rest() {
        let buf = draw();
        // TASKS 非 focus：左上角是細框且邊框色 DarkGray
        let tasks_top = layout(Rect::new(0, 0, 120, 40), 1, 0).tasks.top();
        let tasks_corner = &buf[(0, tasks_top)];
        assert_eq!(tasks_corner.symbol(), "┌");
        assert_eq!(tasks_corner.style().fg, Some(Color::DarkGray));
        // WORKERS focus：左上角是粗框且帶 BOLD（ORIGINS 退場後中欄從 x=0 起）
        let workers_corner = &buf[(WORKERS_X0, 0)];
        assert_eq!(workers_corner.symbol(), "┏");
        assert!(workers_corner.style().add_modifier.contains(Modifier::BOLD));
    }

    /// 選取列用**背景**色，且 MUST NOT 吃掉同列的語意前景色。
    ///
    /// 這是換掉 `Modifier::REVERSED` 的理由：REVERSED 會把 fg／bg 對調，
    /// 選中一列就等於讓那列的 status／blocker 色失去意義。
    #[test]
    fn selected_row_uses_background_and_keeps_semantic_foreground() {
        let buf = draw();
        let (x, y) = find_in(&buf, "alive-w", WORKERS_X0, DETAIL_X0);
        assert_eq!(buf[(x, y)].style().bg, Some(Color::Blue));
        // 同一列（選取列）上的 blocker 前景色仍在
        let (bx, by) = find_in(&buf, "blocked", WORKERS_X0, DETAIL_X0);
        assert_eq!(by, y, "fixture 預期 blocker 就在選取列上");
        assert_eq!(buf[(bx, by)].style().bg, Some(Color::Blue));
        assert_eq!(buf[(bx, by)].style().fg, Some(Color::Red));
    }

    /// footer 的 sticky 警告：BOLD＋Yellow。
    #[test]
    fn footer_warning_is_bold_yellow() {
        let buf = draw();
        let s = style_at(&buf, "WARN-fixture");
        assert_eq!(s.fg, Some(Color::Yellow));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    /// **權威字不變 invariant**：上色只加 style，字元層 MUST 與上色前相同。
    /// 六個權威字與三個 glyph 都要還在畫面上——既有 shell 分組全部是
    /// `capture-pane` 的字元比對，字元一變那邊就整片紅。
    #[test]
    fn character_layer_still_carries_the_authoritative_words() {
        let buf = draw();
        let t = text(&buf);
        for w in [
            "queued",
            "delivered",
            "running",
            "completed",
            "failed",
            "cancelled",
        ] {
            // 六個字**全部**要真的出現在 buffer 上（審查 minor #4：原本
            // `delivered` 走 `theme::status_style` 直呼繞過 buffer，等於沒驗到
            // 它有沒有被畫出來）
            assert!(t.contains(w), "畫面上少了權威字「{w}」");
        }
        // `●` 已隨 P4.6 的 origin 列一起退場（window 死活不再冒充 agent
        // 死活）；留下的兩個都是 **worker 自己那一軸** 的標記
        for g in ["✗", "⛔"] {
            assert!(t.contains(g), "畫面上少了 glyph「{g}」");
        }
        assert!(
            !t.contains("●"),
            "origin 列的 liveness glyph MUST 已退場（P4.6 題 2）"
        );
        // `blocked` 不是 task 狀態字（它是 BLOCKER 軸的字面）：theme MUST NOT
        // 認得它，否則等於承認了一個不存在的 task 狀態（tui-design.md §2）
        assert_eq!(
            theme::status_style("blocked"),
            ratatui::style::Style::default(),
            "blocked 不是 task 狀態字，MUST NOT 有 status 對映色"
        );
    }

    /// P4.7 切片 D：`L` 的提示與 dispatch 是同一份事實源（`RowCaps`）——
    /// 有 pane 的 worker 列才列得出來，task 列與無 pane 的列上不得出現。
    #[test]
    fn the_footer_offers_the_tail_preview_only_where_it_works() {
        assert!(text(&draw()).contains("L tail preview"), "worker 列缺 L");
        assert!(
            !text(&draw_with(|a| a.row_idx = 1)).contains("L tail preview"),
            "task 列 MUST NOT 列出 L（沒有唯一的 pane 可 capture）"
        );
        let t = text(&draw_model_with(|m, _| m.workers[0].pane = String::new()));
        assert!(
            !t.contains("L tail preview"),
            "pane 欄空的列 MUST NOT 列出 L：\n{t}"
        );
    }

    /// 尾行預覽畫的是**最後**那幾行：畫不下時截頭不截尾，且截斷過要說出來。
    ///
    /// 這一條是它不能沿用 `render_info` 的理由——那個 popup 溢出時砍的是尾端，
    /// 也就是尾行預覽唯一有用的部分。
    #[test]
    fn the_tail_preview_shows_the_end_not_the_beginning() {
        let lines: Vec<String> = (0..300).map(|i| format!("line-{i}")).collect();
        let t = text(&draw_with(|a| {
            a.peek = Some(crate::app::PeekView {
                title: "tail preview — w1 (%5)".to_string(),
                lines: lines.clone(),
                truncated: true,
            })
        }));
        assert!(t.contains("tail preview \u{2014} w1 (%5)"), "缺標題：\n{t}");
        assert!(t.contains("line-299"), "最後一行 MUST 看得到：\n{t}");
        assert!(!t.contains("line-0 "), "畫不下時 MUST 砍開頭：\n{t}");
        assert!(t.contains("truncated at"), "截斷 MUST 標記：\n{t}");
        assert!(t.contains("press any key to close"));

        // 沒截斷就不要無中生有一行標記
        let t = text(&draw_with(|a| {
            a.peek = Some(crate::app::PeekView {
                title: "tail preview \u{2014} w1 (%5)".to_string(),
                lines: vec!["only".to_string()],
                truncated: false,
            })
        }));
        assert!(t.contains("only"));
        assert!(!t.contains("truncated at"), "未截斷時 MUST NOT 標記：\n{t}");
    }

    /// 一整個 byte 界寬的**單行**畫得出來，不 panic、不靜默歸零。
    ///
    /// 這一條釘的是型別窄化（跨廠複核 M4／verifier 獨立抓到同一條）：`-J` 會把
    /// 軟折行併成一條，所以「64 KiB 的單行」不是假想——`l.width() as u16` 在
    /// debug build（overflow-checks 開著）會 panic，恰好 65536 則 cast 成 0。
    /// 兩個尺寸都要驗：`TAIL_MAX_BYTES - 1`（65535，`as u16` 的邊界）與整界。
    #[test]
    fn a_pane_line_as_wide_as_the_byte_bound_still_renders() {
        for len in [
            ab_core::config::TAIL_MAX_BYTES - 1,
            ab_core::config::TAIL_MAX_BYTES,
        ] {
            let wide = "w".repeat(len);
            let t = text(&draw_with(|a| {
                a.peek = Some(crate::app::PeekView {
                    title: "tail preview \u{2014} w1 (%5)".to_string(),
                    lines: vec![wide.clone(), "tail-marker".to_string()],
                    truncated: true,
                })
            }));
            // 畫得出來就是第一個斷言（panic 會讓測試直接紅）
            assert!(
                t.contains("tail-marker"),
                "{len} 寬的單行之後，尾端仍 MUST 看得到：\n{t}"
            );
            assert!(
                t.contains("press any key to close"),
                "{len}：一條長行 MUST NOT 把整個 overlay 吃掉：\n{t}"
            );
        }
    }

    /// **contextual footer**（P4.6 切片 B）：第一段只列當前選中列按得動的鍵。
    /// 三種列型各一條斷言，且每一條都要驗「別的列型的鍵不在上面」——只驗
    /// 「有出現」的話，把三種都印出來的舊 footer 照樣全綠。
    #[test]
    fn footer_lists_only_the_keys_valid_for_the_selected_row() {
        // worker 列（fixture 預設選取）：focus／evict 在，cancel 不在
        let t = text(&draw());
        assert!(t.contains("Enter focus pane"), "worker 列缺 focus 鍵");
        assert!(t.contains("e evict"));
        assert!(
            !t.contains("x cancel"),
            "worker 列 MUST NOT 列出 x（它只對 task 列有效）"
        );

        // WORKERS 的內嵌 task 列（第 1 列＝alive-w 的 running 任務，非終態）
        let t = text(&draw_with(|a| a.row_idx = 1));
        assert!(t.contains("Enter/r read full text"), "task 列缺 read 鍵");
        assert!(t.contains("x cancel"), "非終態 task 列 MUST 列出 x");
        assert!(
            !t.contains("e evict"),
            "task 列 MUST NOT 列出 e（evict 只對 worker 列有效）"
        );

        // TASKS 欄的列（P4.7 切片 B：原本這格驗的是 origin 列）
        let t = text(&draw_with(|a| a.panel = Panel::Tasks));
        assert!(t.contains("Enter/r read full text"));
        assert!(
            !t.contains("e evict"),
            "TASKS 欄選的是 task，MUST NOT 列出 e"
        );

        // 全域鍵是**第二段**，三種列型下都在
        for buf in [
            draw(),
            draw_with(|a| a.row_idx = 1),
            draw_with(|a| a.panel = Panel::Tasks),
        ] {
            assert!(text(&buf).contains("? keys \u{b7} q quit"), "全域段不見了");
        }
    }

    /// 終態 task 列不得列出 `x`：列出來等於承諾一個做不到的動作
    /// （按下去只會換來一行「already terminal」）。
    #[test]
    fn footer_hides_cancel_on_terminal_tasks() {
        // TASKS 欄第 1 列＝20260801T000001Z-bbbb（failed＝終態）
        let t = text(&draw_with(|a| {
            a.panel = Panel::Tasks;
            a.task_idx = 1;
        }));
        assert!(t.contains("Enter/r read full text"));
        assert!(!t.contains("x cancel"), "終態 task 列 MUST NOT 列出 x");
    }

    /// **審查 minor 的 regression（a）**：按下去不做事的鍵 MUST NOT 出現在
    /// footer。P4.7 切片 B：ORIGINS 退場後，這個情境由「一列都沒有」承接
    /// （原測試名 `footer_drops_the_enter_hint_on_an_empty_scope`）。
    #[test]
    fn footer_drops_every_key_hint_when_nothing_is_selected() {
        // 正向對照：有 worker 時照樣提示
        let t = text(&draw());
        assert!(t.contains("Enter focus pane"));

        // registry 全空：沒有任何列，也就沒有任何鍵
        let t = text(&draw_model_with(|m, _| {
            m.workers.clear();
            m.tasks.clear();
            m.groups.clear();
        }));
        assert!(
            !t.contains("Enter focus pane"),
            "沒有選中列 MUST NOT 提示一個按下去不做事的 Enter"
        );
        assert!(
            t.contains("(no row selected)"),
            "要說明「沒有選中列」（而不是「這一列沒有鍵」）"
        );
        assert!(
            t.contains("(no workers registered)"),
            "空清單 placeholder 仍在"
        );
    }

    /// **審查 minor 的 regression（b）**：收件人已不在 registry 的 task 列
    /// （只有 `ALL` 看得到）按 `i` 其實沒有摘要可看，footer MUST NOT 提示它。
    #[test]
    fn footer_drops_the_info_hint_when_the_recipient_is_gone() {
        let t = text(&draw_model_with(|m, a| {
            m.recent
                .insert(0, task("20260801T000009Z-9999", "vanished-w", "completed"));
            a.panel = Panel::Tasks;
            a.task_idx = 0;
        }));
        assert!(t.contains("Enter/r read full text"), "全文仍讀得到");
        assert!(t.contains("c copy evidence"), "證據仍複製得到");
        assert!(
            !t.contains("i info"),
            "收件人已不在 registry：MUST NOT 提示 i"
        );

        // 正向對照：收件人還在的 task 列照樣提示 i
        let t = text(&draw_with(|a| {
            a.panel = Panel::Tasks;
            a.task_idx = 0;
        }));
        assert!(t.contains("i info"));
    }

    /// `?` 頁擴成兩區：全域鍵一區、當前列一區（第二區與 footer 同一份正本）。
    #[test]
    fn help_page_has_a_global_zone_and_a_current_row_zone() {
        let t = text(&draw_with(|a| a.help = true));
        assert!(t.contains("global:"), "`?` 頁缺全域區");
        assert!(t.contains("current row:"), "`?` 頁缺當前列區");
        assert!(t.contains("Tab/S-Tab panes"), "全域區缺換欄鍵");
        // 新鍵 MUST 出現在唯一的鍵位正本上（畫面沒列出來的鍵等於不存在）
        assert!(t.contains("/ filter"), "全域區缺 `/`");
        assert!(t.contains("S scope"), "全域區缺 `S`");
        assert!(t.contains("Enter focus pane"), "當前列區缺該列的鍵");
        // 兩區的內容隨選中列換（TASKS 欄不該還印 worker 列那一套）
        let t = text(&draw_with(|a| {
            a.panel = Panel::Tasks;
            a.help = true;
        }));
        assert!(t.contains("Enter/r read full text"));
        assert!(!t.contains("Enter focus pane"));
    }

    // ---- P4.7 切片 B1：lineage 分組（render 面）----

    /// 測試用 canonical generation key（與 `ab_core::spawn::GEN_KEY_RE` 同文法）。
    fn canon(tag: &str) -> String {
        format!("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-{tag}")
    }

    /// 一列 lineage worker。
    fn lin(name: &str, pane: &str, tag: &str, root: &str, parent: Option<&str>) -> AgentSnapshot {
        AgentSnapshot {
            spawn_tag: canon(tag),
            lineage_root: Some(canon(root)),
            parent_agent: parent.map(canon),
            ..worker(name, pane, "codex", "s:@1", true)
        }
    }

    /// 把 fixture 的 worker 清單整批換掉（groups 一併重推）。
    fn draw_workers(ws: Vec<AgentSnapshot>) -> Buffer {
        draw_model_with(move |m, _| {
            m.workers = ws;
            m.tasks.clear();
            m.groups = crate::model::group_by_lineage(&m.workers);
        })
    }

    /// 組標頭是**畫面上的裝飾行**：帶標籤與成員數，且排在該組第一列之前。
    /// 它不是 `Row`——這一點由 `model::worker_rows` 那條測試守住。
    #[test]
    fn workers_column_prints_a_header_line_before_each_group() {
        let root = "root-1-aaaaaaaaaaaa";
        let buf = draw_workers(vec![
            lin("root", "%1", root, root, None),
            lin("kid", "%2", "kid-2-bbbbbbbbbbbb", root, Some(root)),
            worker("solo", "%3", "agy", "", false),
        ]);
        let (_, y_head) = find_in(&buf, "lineage root (2)", WORKERS_X0, DETAIL_X0);
        let (_, y_root) = find_in(&buf, "\u{25b8} root ", WORKERS_X0, DETAIL_X0);
        let (_, y_kid) = find_in(&buf, "\u{25b8} kid ", WORKERS_X0, DETAIL_X0);
        let (_, y_stand) = find_in(&buf, "(standalone) (1)", WORKERS_X0, DETAIL_X0);
        let (_, y_solo) = find_in(&buf, "\u{25b8} solo ", WORKERS_X0, DETAIL_X0);
        assert!(y_head < y_root && y_root < y_kid, "組標頭 MUST 在成員之前");
        assert!(y_kid < y_stand, "standalone 段恆在最後");
        assert!(y_stand < y_solo);
    }

    /// **gate (a) render 面**：root→A→B→C，A／B 的 registry 不在了，C 仍畫在
    /// 同一個 lineage 標頭底下——歸屬的證據是 C 自己身上的 generation key。
    #[test]
    fn a_lineage_header_still_covers_the_survivor_after_its_middle_is_gone() {
        let root = "root-1-aaaaaaaaaaaa";
        let buf = draw_workers(vec![
            lin("root", "%1", root, root, None),
            lin(
                "C",
                "%4",
                "c-4-dddddddddddd",
                root,
                Some("b-3-cccccccccccc"),
            ),
        ]);
        let (_, y_head) = find_in(&buf, "lineage root (2)", WORKERS_X0, DETAIL_X0);
        let (_, y_c) = find_in(&buf, "\u{25b8} C ", WORKERS_X0, DETAIL_X0);
        assert!(y_head < y_c);
        assert!(
            !text(&buf).contains("(standalone)"),
            "C MUST NOT 被踢去 standalone"
        );

        // root 也不在了：標籤降級成 `†＋世代短碼`（不冒用任何在場者的名字），
        // 但 C 還是自成同一組
        let buf = draw_workers(vec![lin(
            "C",
            "%4",
            "c-4-dddddddddddd",
            root,
            Some("b-3-cccccccccccc"),
        )]);
        let t = text(&buf);
        assert!(
            t.contains("lineage root\u{2020} (aaaa)"),
            "根不在場 MUST 標 †＋世代短碼，實際畫面：{t}"
        );
        assert!(!t.contains("(standalone)"));
    }

    /// DETAIL 欄逐列的字元（**只取 DETAIL 的 x 範圍**）。
    ///
    /// breadcrumb 的字面帶 `→`／`…`／`†`，全是 East Asian Ambiguous（單寬），
    /// 但整條 needle 只可能落在一列裡——逐列取字串再 `contains`，比 `find_in`
    /// 的滑動視窗少一層「跨列拼接」的誤判空間。
    fn detail_lines(buf: &Buffer) -> Vec<String> {
        (0..buf.area().height)
            .map(|y| {
                (DETAIL_X0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// **gate (a) render 面的 breadcrumb**：選中 C 時 DETAIL 投影
    /// `root → … → B† → C`。防護矩陣的主力在 model 層（純函式），這裡抽樣
    /// 驗兩件只有畫面說得準的事：字面真的畫得出來、以及說不出世代的列
    /// 寫 `-` 而不是一條假血緣。
    #[test]
    fn detail_projects_the_lineage_breadcrumb() {
        let root = "root-1-aaaaaaaaaaaa";
        let buf = draw_model_with(|m, app| {
            m.workers = vec![
                lin("root", "%1", root, root, None),
                // parent B 的 registry 不在了 → 墓碑；B 的 parent（A）無從得知
                // → 省略號
                lin(
                    "C",
                    "%4",
                    "C-4-dddddddddddd",
                    root,
                    Some("B-3-cccccccccccc"),
                ),
            ];
            m.tasks.clear();
            m.groups = crate::model::group_by_lineage(&m.workers);
            app.row_idx = 1; // 選中 C（DETAIL 只投影選中列）
        });
        let want = "lineage: root \u{2192} \u{2026} \u{2192} B\u{2020} (cccc) \u{2192} C";
        let lines = detail_lines(&buf);
        assert!(
            lines.iter().any(|l| l.contains(want)),
            "DETAIL MUST 畫出「{want}」，實際：{lines:#?}"
        );

        // 說不出世代的列（fixture 的 worker 是 legacy 形狀：有 spawn_tag、
        // 兩欄缺席）→ `(standalone)`（切片 C：與組標頭同一個字面），
        // MUST NOT 畫出只有自己一節的假 breadcrumb
        let plain = detail_lines(&draw());
        assert!(
            plain.iter().any(|l| l.contains("lineage: (standalone)")),
            "legacy 列 MUST 與組標頭同字面，實際：{plain:#?}"
        );
        assert!(
            !plain.iter().any(|l| l.contains("lineage: alive-w")),
            "MUST NOT 把 self 自己畫成一條 lineage"
        );
    }

    /// **底條模式**（寬度不足 → DETAIL 走整寬底條）：breadcrumb 與等價 CLI
    /// 原文 MUST 都還在畫面上。
    ///
    /// 這條在 B2 之前不存在，而 `DETAIL_STRIP_H` 也就沒人守：breadcrumb 讓
    /// worker 那一支長了一行，舊值 11 會把 `evidence:` 連同命令一起推出畫面
    /// ——薄殼原則（`layout` 的註解）說那等於畫面上沒有那條命令。
    #[test]
    fn the_detail_strip_still_fits_the_breadcrumb_and_the_command() {
        // **會換行的鏈**才驗得到 H3：八代、每個名字 13 欄，整條 125 欄，
        // 遠超過底條的 68 欄內寬
        let names: Vec<String> = (1..=8).map(|i| format!("relay-node-{i:02}")).collect();
        let tags: Vec<String> = (1..=8)
            .map(|i| format!("relay-node-{i:02}-{i}-{i:012x}"))
            .collect();
        let ws: Vec<AgentSnapshot> = (0..8)
            .map(|i| {
                lin(
                    &names[i],
                    &format!("%{}", i + 1),
                    &tags[i],
                    &tags[0],
                    if i == 0 { None } else { Some(&tags[i - 1]) },
                )
            })
            .collect();
        // 70 欄 < TWO_COL_MIN_W（27＋45）→ DETAIL 走整寬底條
        let buf = draw_at(70, 24, Freshness::default(), |m, app| {
            m.workers = ws;
            m.tasks.clear();
            m.groups = crate::model::group_by_lineage(&m.workers);
            app.row_idx = 7; // 選中最後一代
        });
        let t = text(&buf);
        assert!(
            t.contains("lineage: relay-node-01 \u{2192} \u{2026} \u{2192} relay-node-08"),
            "底條模式的 breadcrumb MUST 收縮成一行（保留 root 與 self）：\n{t}"
        );
        assert!(
            !t.contains("relay-node-01 \u{2192} relay-node-02"),
            "收縮 MUST 真的發生（整條展開就會換行，把命令推出畫面）：\n{t}"
        );
        assert!(
            t.contains("$ agent-bridge list --long"),
            "底條模式 MUST 留住等價 CLI 原文（薄殼原則），實際畫面：\n{t}"
        );
    }

    /// 全高（兩欄）模式**不收縮**：那裡 DETAIL 高度不固定，wrap 是既有行為，
    /// H3 的收縮只針對底條（不得順手改掉另一支的顯示）。
    #[test]
    fn the_two_column_detail_still_wraps_instead_of_collapsing() {
        let names: Vec<String> = (1..=8).map(|i| format!("relay-node-{i:02}")).collect();
        let tags: Vec<String> = (1..=8)
            .map(|i| format!("relay-node-{i:02}-{i}-{i:012x}"))
            .collect();
        let ws: Vec<AgentSnapshot> = (0..8)
            .map(|i| {
                lin(
                    &names[i],
                    &format!("%{}", i + 1),
                    &tags[i],
                    &tags[0],
                    if i == 0 { None } else { Some(&tags[i - 1]) },
                )
            })
            .collect();
        let buf = draw_model_with(|m, app| {
            m.workers = ws;
            m.tasks.clear();
            m.groups = crate::model::group_by_lineage(&m.workers);
            app.row_idx = 7;
        });
        let lines = detail_lines(&buf);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("relay-node-01 \u{2192} relay-node-02")),
            "兩欄模式 MUST 照舊展開＋wrap，實際：{lines:#?}"
        );
    }

    // ── P4.7 切片 C：filter 提示列／scope／copy-mode banner ────────────

    /// filter 生效時畫面要說得出「現在還篩著」，且空結果的兩種原因分得開。
    #[test]
    fn the_filter_says_it_is_on_and_why_the_panel_is_empty() {
        // 輸入模式：畫游標塊
        let t = text(&draw_with(|a| {
            a.filter_input = true;
            a.filter.query = "ali".into();
        }));
        assert!(t.contains("/ali\u{2588}"), "輸入模式 MUST 畫游標塊：\n{t}");

        // 生效但不在輸入中：說得出目前篩的是什麼
        let t = text(&draw_with(|a| a.filter.query = "ali".into()));
        assert!(t.contains("filter: ali"), "MUST 顯示目前的 filter：\n{t}");

        // 空結果：MUST 說是被篩掉的，而不是「沒有 worker」——後者按什麼都沒用，
        // 前者按 Esc 就回得來
        let t = text(&draw_with(|a| a.filter.query = "zzzz".into()));
        assert!(t.contains("(no rows match the filter)"), "實際：\n{t}");
        assert!(
            !t.contains("(no workers registered)"),
            "篩空 MUST NOT 說成沒註冊：\n{t}"
        );
    }

    /// filter 生效時，組標頭的括號數字算**畫得出來的**那些。
    #[test]
    fn a_group_header_counts_only_the_rows_it_still_has() {
        let root = "root-1-aaaaaaaaaaaa";
        let buf = draw_model_with(|m, app| {
            m.workers = vec![
                lin("root", "%1", root, root, None),
                lin("kid", "%2", "kid-2-bbbbbbbbbbbb", root, Some(root)),
            ];
            m.tasks.clear();
            m.groups = crate::model::group_by_lineage(&m.workers);
            app.filter.query = "kid".into();
        });
        let t = text(&buf);
        assert!(t.contains("lineage root (1)"), "MUST 算可見的那些：\n{t}");
        assert!(!t.contains("lineage root (2)"));
    }

    /// TASKS 標題帶 scope，且 Unattached 的空畫面說的是「都掛上了」而不是
    /// 「沒有任務」——那是兩件不同的事。
    #[test]
    fn the_tasks_panel_shows_its_scope() {
        let t = text(&draw());
        assert!(t.contains("[all]"), "預設 scope MUST 在標題上：\n{t}");
        let t = text(&draw_with(|a| a.scope = crate::model::Scope::Unattached));
        assert!(t.contains("[unattached]"), "實際：\n{t}");
        assert!(
            t.contains("(every task is attached to a worker)"),
            "空 Unattached MUST 說得出原因：\n{t}"
        );

        // **一筆任務都沒有時 MUST NOT 說「全部都掛上了」**（修正輪 R2／F5）：
        // 全新 pool 切到 Unattached 就會踩到——那句話宣稱了一件沒有證據的事
        let t = text(&draw_model_with(|m, a| {
            m.recent = Vec::new();
            a.scope = crate::model::Scope::Unattached;
        }));
        assert!(t.contains("(no recent tasks)"), "實際：\n{t}");
        assert!(
            !t.contains("(every task is attached"),
            "空 pool MUST NOT 宣稱全部掛好了：\n{t}"
        );
    }

    /// **Unattached 的歷史 task 被選中時，畫面 MUST NOT 認領當代同名 worker**
    /// （修正輪 R2／F3）。
    ///
    /// 舊寫法（`worker_idx(&task.to)` 純名字比對）會讓 DETAIL 長出當代 pane／
    /// blocker、footer 提示 `i`、`c` 的 payload 帶當代 pane——而同一筆 task 在
    /// WORKERS 欄明明沒有掛在它底下。同一張畫面兩個答案。
    #[test]
    fn an_unattached_task_never_borrows_the_current_worker() {
        let buf = draw_model_with(|m, app| {
            // occl-w 於 2026-08-02 才註冊；這筆 task 是前一天建立的
            m.workers = vec![AgentSnapshot {
                registered_at: "2026-08-02T00:00:00Z".to_string(),
                ..worker("occl-w", "%5", "codex", "s:@1", true)
            }];
            m.groups = crate::model::group_by_lineage(&m.workers);
            m.tasks.clear();
            m.recent = vec![task("20260801T000000Z-aaaa", "occl-w", "running")];
            app.panel = Panel::Tasks;
            app.task_idx = 0;
        });
        let d = detail_lines(&buf);
        let joined = d.join("\n");
        assert!(
            joined.contains("task-id: 20260801T000000Z-aaaa"),
            "DETAIL 仍投影這筆 task：{d:#?}"
        );
        assert!(
            !joined.contains("pane   : %5") && !joined.contains("pane   : "),
            "MUST NOT 借用當代 worker 的 pane：{d:#?}"
        );
        let t = text(&buf);
        assert!(
            !t.contains("i info"),
            "footer MUST NOT 提示 `i`（沒有可證的 worker）：\n{t}"
        );
        // occl-w 在 copy-mode（fixture 的 %5），但這筆 task 不屬於它 → 無 banner
        assert!(
            !t.contains("[info] "),
            "MUST NOT 替無主 task 畫別人的 copy-mode banner：\n{t}"
        );
    }

    /// **copy-mode banner 的三態**（切片 C）：`pane_in_mode` 回 true → 有；
    /// 回 false → 無；查不到（unknown）→ **無**（查不出就不宣稱）。
    #[test]
    fn the_copy_mode_banner_follows_the_three_state_snapshot() {
        let word = "occluded (copy-mode: a human is reading)";
        // Some(true)：occl-w（%5）在 copy-mode
        let t = text(&draw_with(|a| a.row_idx = ROW_OCCL_W));
        assert!(t.contains("[info] 'occl-w' is"), "MUST 有 banner：\n{t}");
        assert!(t.contains(word), "措辭 MUST 沿用 blocker_word 家族：\n{t}");
        // Some(false)：dead-w（%2）查得到、沒有 blocker
        let t = text(&draw_with(|a| a.row_idx = ROW_DEAD_W));
        assert!(!t.contains("[info] "), "非 copy-mode MUST 無 banner：\n{t}");
        // None：整層查不到 → unknown
        let t = text(&draw_blockers(BlockerIndex::unknown(), |a| {
            a.row_idx = ROW_OCCL_W
        }));
        assert!(
            !t.contains("[info] "),
            "unknown MUST NOT 宣稱有人在看：\n{t}"
        );
    }

    /// **bounded 斷言**：banner 只讀快照，render 路徑一個 tmux 呼叫都不發。
    ///
    /// 用計數假件證明：先讓 `BlockerIndex::query` 走一輪（計數會動），之後畫
    /// 20 幀，計數 MUST 一格都不動。畫面每 500ms 重繪，render 一旦能發查詢
    /// 就等於無界地打 tmux（§4 bounded-read 硬條款）。
    #[test]
    fn rendering_the_banner_never_touches_tmux() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingTmux(AtomicUsize);
        impl ab_core::tmux::TmuxClient for CountingTmux {
            fn exec(&self, _a: &[&str]) -> Option<ab_core::tmux::TmuxOutput> {
                self.0.fetch_add(1, Ordering::SeqCst);
                None
            }
            fn available(&self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
            fn resolve_pane(&self, _t: &str) -> Option<String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                None
            }
            fn pane_exists(&self, _p: &str) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
            fn capture_pane(&self, _p: &str) -> Option<String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                None
            }
            fn pane_in_mode(&self, _p: &str) -> Option<bool> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Some(true)
            }
            fn send_keys(&self, _p: &str, _k: &str) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                false
            }
        }
        let tmux = CountingTmux(AtomicUsize::new(0));
        let (model, live, _, mut app) = fixture();
        app.row_idx = ROW_OCCL_W;
        // 快照在**背景 worker 那一側**取得（這一步當然要打 tmux）
        let bl = BlockerIndex::query(&tmux, &["%5".to_string()]);
        let after_query = tmux.0.load(Ordering::SeqCst);
        assert!(after_query > 0, "取快照本來就會打 tmux（對照組）");

        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        for _ in 0..20 {
            t.draw(|f| render(f, &model, &live, &bl, &app, Freshness::default()))
                .unwrap();
        }
        assert_eq!(
            tmux.0.load(Ordering::SeqCst),
            after_query,
            "render 路徑 MUST NOT 發任何 tmux 呼叫"
        );
        // 而且 banner 真的畫出來了（否則這條會在「什麼都沒畫」時假綠）
        assert!(text(t.backend().buffer()).contains("[info] 'occl-w' is"));
    }

    /// **gate (b) render 面**：Legacy／Manual／Invalid 三型都畫在 `(standalone)`
    /// 底下，且 MUST NOT 生出自己的 lineage 標頭——「說不出組別」與「屬於某一
    /// 組」是兩件事，畫面不得把前者演成後者。
    #[test]
    fn legacy_manual_and_invalid_all_land_under_standalone() {
        let mut legacy = worker("legacy-w", "%1", "codex", "s:@1", true);
        legacy.spawn_tag = "ab-spawn-old-1-aaaaaaaaaaaa".to_string(); // 舊式：非 canonical
        let manual = AgentSnapshot {
            spawn_tag: String::new(), // 人工註冊：沒有世代
            ..worker("manual-w", "%2", "agy", "", false)
        };
        let mut invalid = worker("invalid-w", "%3", "claude", "s:@1", true);
        invalid.spawn_tag = canon("inv-9-eeeeeeeeeeee");
        invalid.lineage_root = Some(String::new()); // 非字串欄位＝無效標記

        let buf = draw_workers(vec![legacy, manual, invalid]);
        let t = text(&buf);
        assert!(t.contains("(standalone) (3)"), "三型 MUST 都在 standalone");
        assert!(
            !t.contains("lineage "),
            "說不出組別的列 MUST NOT 生出 lineage 標頭：{t}"
        );
    }

    /// 組標頭是**行**不是列：選取第 n 列時捲動要換算過標頭，否則列數一多就
    /// 會把選中列捲出畫面（P4.6 切片 C 的捲動語意不得因分組而失效）。
    #[test]
    fn group_headers_shift_the_rendered_line_of_a_row() {
        let root = "root-1-aaaaaaaaaaaa";
        let ws = vec![
            lin("root", "%1", root, root, None),
            lin("kid", "%2", "kid-2-bbbbbbbbbbbb", root, Some(root)),
            worker("solo", "%3", "agy", "", false),
        ];
        let mut m = with_groups(Model {
            groups: Vec::new(),
            workers: ws,
            tasks: Vec::new(),
            recent: Vec::new(),
            recent_truncated: false,
        });
        m.groups = crate::model::group_by_lineage(&m.workers);
        // 列 0/1＝lineage 兩員（前面 1 個標頭）、列 2＝solo（前面 2 個標頭）
        let nof = crate::model::Filter::default();
        assert_eq!(worker_line_of(&m, &nof, 0), 1);
        assert_eq!(worker_line_of(&m, &nof, 1), 2);
        assert_eq!(worker_line_of(&m, &nof, 2), 4);
    }

    // ---- P4.6 切片 C：scrollbar／N-total／freshness ----

    /// 造一份 n 筆 recent 的模型（TASKS 欄剛好 n 列）。
    fn with_tasks(n: usize, truncated: bool) -> impl FnOnce(&mut Model, &mut App) {
        move |m: &mut Model, a: &mut App| {
            m.recent = (0..n)
                .map(|i| {
                    task(
                        &format!("20260801T0000{i:02}Z-t{i:03}"),
                        "alive-w",
                        "completed",
                    )
                })
                .collect();
            m.recent_truncated = truncated;
            a.panel = Panel::Tasks;
        }
    }

    /// TASKS 面板的位置（版面計算與 render 共用同一份 `layout`——fixture 的
    /// app 帶 1 則警告，footer 高度要跟著算）。
    fn tasks_area() -> Rect {
        layout(Rect::new(0, 0, 120, 40), 1, 0).tasks
    }

    /// 捲軸畫在中欄最右那一欄（TASKS 面板的右框上）。**只掃 TASKS 那幾列**：
    /// 同一欄在上半部是 WORKERS 面板的右框，掃進去會把邊框當成軌道。
    fn scrollbar_col(buf: &Buffer, sym: &str) -> Vec<u16> {
        let a = tasks_area();
        let x = a.right() - 1;
        (a.top()..a.bottom())
            .filter(|&y| buf[(x, y)].symbol() == sym)
            .collect()
    }

    fn thumb_y(buf: &Buffer) -> Option<u16> {
        scrollbar_col(buf, "█").first().copied()
    }

    /// 捲軸：**只在捲得動時才畫**，thumb 首/中/末各在頂/中/底。
    #[test]
    fn tasks_scrollbar_tracks_the_selection_and_only_shows_when_needed() {
        // 40 列遠多於 TASKS 欄的可視高度
        let top = thumb_y(&draw_model_with(with_tasks(40, false))).expect("該畫捲軸");
        let mid = thumb_y(&draw_model_with(|m, a| {
            with_tasks(40, false)(m, a);
            a.task_idx = 20;
        }))
        .expect("該畫捲軸");
        let bottom = thumb_y(&draw_model_with(|m, a| {
            with_tasks(40, false)(m, a);
            a.task_idx = 39;
        }))
        .expect("該畫捲軸");
        assert!(top < mid, "首列的 thumb 要在中段之上（{top} vs {mid}）");
        assert!(
            mid < bottom,
            "中段的 thumb 要在末列之上（{mid} vs {bottom}）"
        );

        // 列數塞得下時**一格捲軸都不畫**（空軌道等於用一欄寬度說廢話）
        let buf = draw_model_with(with_tasks(3, false));
        assert!(thumb_y(&buf).is_none(), "塞得下時不得畫 thumb");
        // 連**軌道與端點箭頭**都不得留下（空軌道等於用一欄寬度說廢話）
        let scrolling = draw_model_with(with_tasks(40, false));
        for sym in ["║", "▲", "▼"] {
            assert!(
                !scrollbar_col(&scrolling, sym).is_empty(),
                "前提：捲得動時這一欄真的有「{sym}」（否則下一條斷言驗不到東西）"
            );
            assert!(
                scrollbar_col(&buf, sym).is_empty(),
                "塞得下時不得留下捲軸字元「{sym}」"
            );
        }
    }

    /// `N/total`：隨選取移動而變；被載入上限截斷時**說出來**。
    #[test]
    fn tasks_title_shows_position_total_and_truncation() {
        let t = text(&draw_model_with(with_tasks(6, false)));
        assert!(t.contains("TASKS 1/6"), "首列＝1/6");
        let t = text(&draw_model_with(|m, a| {
            with_tasks(6, false)(m, a);
            a.task_idx = 3;
        }));
        assert!(t.contains("TASKS 4/6"), "序位隨選取走");
        assert!(!t.contains("TASKS 4/6+"), "沒截斷就不得標 +");

        // 截斷：`+` 表示「還有更舊的沒載入」
        let t = text(&draw_model_with(with_tasks(6, true)));
        assert!(
            t.contains("TASKS 1/6+"),
            "截斷 MUST 說出來（否則畫面等於宣稱就這些了）"
        );
    }

    /// **F5（P4.7 切片 B 改寫）：`+` 的前提與旗標必須同一個範圍。**
    ///
    /// 原本這條驗的是「scoped view MUST NOT 帶全域截斷旗標」。ORIGINS 退場後
    /// TASKS 欄**本來就是全 pool**，過濾這件事不存在了，於是改驗保留下來的那半
    /// 條：`recent` 一筆都沒有時 MUST NOT 標 `+`——`TASKS 0/0+` 是最無稽的那
    /// 一種宣稱（說「還有更舊的」卻連一筆都沒載到）。
    #[test]
    fn the_truncation_flag_never_rides_on_an_empty_task_list() {
        // 正向對照：有列且截斷 → 標 `+`
        let t = text(&draw_model_with(with_tasks(6, true)));
        assert!(t.contains("TASKS 1/6+"), "有列時截斷要標出來");

        // 一列都沒有：旗標還在，但畫面上 MUST NOT 出現 `0/0+`
        let t = text(&draw_model_with(|m, a| {
            with_tasks(6, true)(m, a);
            m.recent = Vec::new();
        }));
        assert!(t.contains("TASKS 0/0"), "空清單的標題");
        assert!(!t.contains("TASKS 0/0+"), "0/0+ 是最無稽的那一種宣稱");

        // 切片 C：filter 與 Unattached scope 各自都是**子集合**，全 pool 的
        // 截斷旗標不得貼到它們的數字上（F5 的前半條）
        let t = text(&draw_model_with(|m, a| {
            with_tasks(6, true)(m, a);
            a.filter.query = "20260801".into();
        }));
        assert!(!t.contains("+ ["), "篩選後的數字 MUST NOT 帶 `+`：\n{t}");
        let t = text(&draw_model_with(|m, a| {
            with_tasks(6, true)(m, a);
            a.scope = crate::model::Scope::Unattached;
        }));
        assert!(!t.contains("+ ["), "非 All scope MUST NOT 帶 `+`：\n{t}");
    }

    /// 兩軸都有成功樣本的正常態。
    fn fresh_at(disk: Duration, tmux: Duration) -> Freshness {
        Freshness {
            disk,
            tmux: Some(tmux),
        }
    }

    /// freshness：正常態低噪（只寫節奏、不寫數字）；逾門檻標 STALE。
    #[test]
    fn footer_shows_freshness_and_marks_stale_axes() {
        let t = text(&draw_fresh(
            fresh_at(Duration::ZERO, Duration::ZERO),
            |_, _| {},
        ));
        assert!(t.contains("[disk 500ms · tmux 2s]"), "正常態只寫節奏");
        assert!(!t.contains("STALE"));

        // tmux 軸逾門檻
        let t = text(&draw_fresh(
            fresh_at(Duration::ZERO, Duration::from_secs(30)),
            |_, _| {},
        ));
        assert!(t.contains("tmux STALE 30s"), "逾門檻要標 STALE＋年紀");
        assert!(t.contains("disk 500ms"), "另一軸不受波及");

        // 過半程但未逾門檻：顯示數字（開始往 stale 走了）
        let t = text(&draw_fresh(
            fresh_at(Duration::from_secs(2), Duration::ZERO),
            |_, _| {},
        ));
        assert!(t.contains("disk 2s ago"), "半程之後才顯示數字（低噪）");
    }

    /// **F2：還沒有任何成功 round 時，footer MUST 說 unknown**——不得顯示
    /// 一個從啟動時間算出來的年紀，那是替一份根本不存在的樣本背書。
    #[test]
    fn footer_says_unknown_before_the_first_successful_tmux_round() {
        // `Freshness::default()` 就是啟動當下的狀態：tmux 尚無樣本
        let t = text(&draw_fresh(Freshness::default(), |_, _| {}));
        assert!(
            t.contains("tmux unknown"),
            "沒有成功樣本 MUST 說 unknown，實際 footer：{}",
            t.lines().find(|l| l.contains("disk")).unwrap_or("(找不到)")
        );
        assert!(
            !t.contains("tmux 2s") && !t.contains("tmux 0s"),
            "MUST NOT 拿啟動時間冒充新鮮度"
        );
        assert!(t.contains("disk 500ms"), "disk 軸不受波及（它確實掃完過）");
    }

    /// **F3：footer 兩軸的契約逐字說得出口**——disk 只證明得了「上次**完成**
    /// 一輪掃描」（`Model::load` 不回錯、單檔損壞逕自跳過），MUST NOT 宣稱
    /// success；tmux 才有明確的失敗訊號。
    #[test]
    fn help_states_what_each_freshness_axis_actually_proves() {
        let t = text(&draw_with(|a| a.help = true));
        assert!(
            t.contains("disk = age of the last completed scan"),
            "disk 軸的說法 MUST 是 completed scan，不得宣稱 success"
        );
        assert!(
            t.contains("tmux = age of the last successful query round"),
            "tmux 軸有失敗訊號，說得出 successful"
        );
        assert!(
            t.contains("no successful round yet"),
            "unknown 這個字在 footer 出現時，`?` 頁要說得出它是什麼意思"
        );
    }

    /// stale 的 tmux 軸 MUST 降級成 **unknown**——不是續留舊死活，也不是說它
    /// 死了。降級發生在 run loop（把 unknown 快照交給 render），這裡驗的是
    /// 「拿到 unknown 快照時畫面說 unknown」這一半。
    #[test]
    fn a_stale_tmux_axis_reads_as_unknown_not_as_gone() {
        let (model, _, _, app) = fixture();
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let fresh = fresh_at(Duration::ZERO, Duration::from_secs(30));
        t.draw(|f| {
            render(
                f,
                &model,
                &LiveIndex::unknown(),
                &BlockerIndex::unknown(),
                &app,
                fresh,
            )
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        let txt = text(&buf);
        assert!(txt.contains("tmux STALE"), "footer 要說這一軸已經舊了");
        // worker 列：unknown 標記在，`✗dead` 不得出現（unknown ≠ gone）
        assert!(
            !txt.contains("✗dead"),
            "unknown MUST NOT 被說成 dead（三態不得壓成兩態）"
        );
        assert!(
            text(&draw()).contains("✗dead"),
            "正向對照：新鮮快照下 dead 標記本來就在"
        );
        // DETAIL 的 agent 死活列也要是 unknown
        assert!(
            txt.contains("state  : unknown"),
            "DETAIL 的死活列要說 unknown"
        );
    }

    /// CJK 判定（含全形標點與全形括號——「（）」「：」正是最容易漏掉的一批）。
    fn is_cjk(c: char) -> bool {
        matches!(c as u32,
            0x3000..=0x303F      // CJK 標點（　、。〈〉…）
            | 0x3040..=0x30FF    // 假名
            | 0x3400..=0x4DBF    // 擴充 A
            | 0x4E00..=0x9FFF    // 統一漢字
            | 0xF900..=0xFAFF    // 相容漢字
            | 0xFF00..=0xFFEF) // 全形／半形形式（（）：·…）
    }

    fn assert_no_cjk(buf: &Buffer, what: &str) {
        for (i, c) in text(buf).chars().enumerate() {
            assert!(
                !is_cjk(c),
                "{what}：chrome 上出現 CJK 字元「{c}」（第 {i} 個字元）"
            );
        }
    }

    /// **P4.6 題 9 的機器 gate**：所有 chrome 一次改完，不留半套。
    ///
    /// 逐畫面掃過每一個 overlay（dashboard／`?`／`x` 確認框／`e` 證據框／
    /// `i` 摘要頁），斷言 buffer 上一個 CJK 字元都沒有。留半套的話——譬如
    /// footer 譯了、`?` 頁沒譯——這條就會紅。
    #[test]
    fn every_chrome_surface_is_english_only() {
        assert_no_cjk(&draw(), "dashboard");
        // 切片 C 的兩條新 chrome：filter 提示列與 copy-mode banner
        assert_no_cjk(
            &draw_with(|a| {
                a.filter_input = true;
                a.filter.query = "ali".into();
            }),
            "filter prompt",
        );
        assert_no_cjk(
            &draw_with(|a| a.filter.query = "zzzz".into()),
            "filter empty",
        );
        assert_no_cjk(&draw_with(|a| a.row_idx = ROW_OCCL_W), "copy-mode banner");
        assert_no_cjk(
            &draw_with(|a| a.scope = crate::model::Scope::Unattached),
            "unattached scope",
        );
        assert_no_cjk(&draw_with(|a| a.help = true), "? keys page");
        assert_no_cjk(
            &draw_with(|a| a.confirm = Some("20260801T000000Z-aaaa".into())),
            "cancel confirm",
        );
        let mut req = ab_core::evict::EvictRequest::new("alive-w");
        req.expect_pane = Some("%1".to_string());
        req.expect_generation = Some("t1".to_string());
        assert_no_cjk(
            &draw_with(|a| {
                a.evict_prompt = Some(crate::action::EvictPrompt {
                    shown: crate::action::EvictShown {
                        name: "alive-w".into(),
                        pane: "%1".into(),
                        spawn_tag: "t1".into(),
                    },
                    lines: crate::action::evict_confirm_lines(&req),
                });
            }),
            "evict evidence box",
        );
        // `i` 摘要頁：內容由 action 層組好（那一層也在題 9 的範圍內）
        let (model, live, blockers, _) = fixture();
        assert_no_cjk(
            &draw_with(|a| {
                a.info = Some(crate::action::info_page(
                    &model, &live, &blockers, "alive-w",
                ));
            }),
            "worker info page",
        );
        // 空清單 placeholder（一列都沒有的 WORKERS／TASKS）也是 chrome
        assert_no_cjk(
            &draw_model_with(|m, _| {
                m.workers.clear();
                m.tasks.clear();
                m.groups.clear();
                m.recent.clear();
            }),
            "empty-list placeholders",
        );
    }

    /// **反向那一半**：payload 原文 MUST NOT 被譯。
    ///
    /// pager 顯示的是 task 回覆的原始 bytes；上一條若寫成「整個畫面都不准有
    /// CJK」，最省事的過關方式就是把 payload 也濾掉——那會是資料損毀，不是
    /// 本地化。故這裡刻意餵一段中文 payload，要求它逐字出現在畫面上。
    #[test]
    fn payload_text_is_never_translated_or_stripped() {
        let payload = "收尾筆記：這段是 payload 原文";
        let buf = draw_with(|a| {
            a.pager = Some(crate::app::Pager {
                id: "20260801T000000Z-aaaa".into(),
                from: "boss".into(),
                to: "alive-w".into(),
                bytes: payload.as_bytes().to_vec(),
                scroll: 0,
            });
        });
        // 雙寬字元在 buffer 裡佔兩格（第二格是 reset 出來的空白），逐格拼字串
        // 對不上——所以逐字檢查「這個字有沒有被畫成某一格」
        let rendered = text(&buf);
        for c in payload.chars().filter(|c| !c.is_whitespace()) {
            assert!(
                rendered.contains(c),
                "payload 原文少了「{c}」（MUST 逐字留在畫面上）"
            );
        }
        // 同一張畫面上，pager 自己的 chrome（標題、捲動提示）仍是英文
        find(&buf, "read (read-only, full text)");
        find(&buf, "Esc/q close");
    }

    // ---- P4.6 切片 D：pager 的 markdown-lite 高亮（gate (e)）----

    /// 一份內文開一個 pager。
    fn pager_of(body: &str) -> crate::app::Pager {
        crate::app::Pager {
            id: "20260801T000000Z-aaaa".into(),
            from: "boss".into(),
            to: "alive-w".into(),
            bytes: body.as_bytes().to_vec(),
            scroll: 0,
        }
    }

    /// 內文逐列的樣式（跳過 `pager_lines` 的四列標頭），供逐條斷言。
    fn body_styles(body: &str) -> Vec<ratatui::style::Style> {
        let p = pager_of(body);
        let lines = highlight_pager(&crate::action::pager_lines(&p));
        lines[4..lines.len() - 2]
            .iter()
            .map(|l| {
                // 一列可能切成多段（marker／meta key）：取**第一個有樣式的**
                // span，沒有就回 default
                l.spans
                    .iter()
                    .map(|s| s.style)
                    .find(|s| *s != ratatui::style::Style::default())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// **gate (e) 第一句：bytes 不變。**
    ///
    /// 高亮純粹是樣式投影——同一份 bytes 進去，buffer 的**字元層必須逐格
    /// 完全相同**，只有 Style 不同。兩條路徑共用 `pager_widget`（同一個外框、
    /// 同一個捲動位置），比到的差異只可能來自高亮本身。
    #[test]
    fn highlighting_never_changes_the_character_layer() {
        let body = "# Title\n\nprose - not a list\n- item one\n  1. nested\n\n```rust\nfn main() {}\n- inside code\n```\n\ndiff --git a/x b/x\n@@ -1,2 +1,2 @@\n-old line\n+new line\n\ntask-id: 20260801T000000Z-bbbb\nsee: below\n\t tabbed  and  spaced \n";
        let p = pager_of(body);
        let raw = crate::action::pager_lines(&p);

        let render_into = |lines: Vec<Line<'static>>| {
            let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
            t.draw(|f| {
                let area = f.area();
                f.render_widget(Clear, area);
                f.render_widget(pager_widget(&p, lines), area);
            })
            .unwrap();
            t.backend().buffer().clone()
        };
        let plain = render_into(raw.iter().map(|s| Line::from(s.clone())).collect());
        let highlighted = render_into(highlight_pager(&raw));

        assert_eq!(plain.area(), highlighted.area());
        let area = *plain.area();
        let mut styled_cells = 0;
        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(
                    plain[(x, y)].symbol(),
                    highlighted[(x, y)].symbol(),
                    "字元層 MUST 逐格相同（({x},{y}) 高亮後變了）"
                );
                if plain[(x, y)].style() != highlighted[(x, y)].style() {
                    styled_cells += 1;
                }
            }
        }
        assert!(
            styled_cells > 0,
            "前提：這份輸入真的有東西被上色（否則上一條在驗空氣）"
        );

        // 逐列再驗一次不變式的來源：每一列的 span 串起來 MUST 逐字等於原列
        for (l, r) in highlight_pager(&raw).iter().zip(raw.iter()) {
            let joined: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(&joined, r, "span 串接 MUST 逐字等於原列");
        }
    }

    /// fence：` ```rust ` 段內整段是 code、閉合後恢復；**未閉合的 fence 不得
    /// 把剩餘全文吃掉**（撞上強結構訊號就結束）。
    #[test]
    fn fenced_code_blocks_start_and_stop_where_the_fence_says() {
        let code = theme::md_code_style();
        let st = body_styles("before\n```rust\nfn main() {}\n```\nafter\n");
        assert_ne!(st[0], code, "fence 之前是散文");
        assert_eq!(st[1], code, "開場圍籬本身算 code");
        assert_eq!(st[2], code, "段內是 code");
        assert_eq!(st[3], code, "閉合圍籬本身算 code");
        assert_ne!(st[4], code, "閉合之後 MUST 恢復");

        // 圍籬字元不同族不得提早關掉（`~~~` 關不掉 ``` 段）
        let st = body_styles("```\n~~~\nstill code\n```\nout\n");
        assert_eq!(st[1], code, "不同族的圍籬 MUST NOT 關掉這一段");
        assert_eq!(st[2], code);
        assert_ne!(st[4], code, "同族圍籬才關得掉");

        // 未閉合：ATX 標題與中繼標頭都是強結構訊號，撞上即結束
        let st = body_styles("```\ncode here\n# Heading\nprose again\n");
        assert_eq!(st[1], code);
        assert_eq!(
            st[2],
            theme::md_heading_style(),
            "未閉合的 fence MUST NOT 把標題也吃掉"
        );
        assert_ne!(st[3], code, "標題之後的散文 MUST NOT 還是 code");
    }

    /// 標題：`#`…`######` 須**行首**且後接空白；`a # b`／`#hashtag` 不得誤染。
    #[test]
    fn atx_headings_match_only_at_line_start() {
        let h = theme::md_heading_style();
        let st = body_styles("# One\n### Three\n####### Seven\na # b\n#hashtag\n  # indented\n");
        assert_eq!(st[0], h, "`# ` 是標題");
        assert_eq!(st[1], h, "`### ` 是標題");
        assert_ne!(st[2], h, "七個 `#` 超過 ATX 上限");
        assert_ne!(st[3], h, "行首不是 `#` 就不是標題");
        assert_ne!(st[4], h, "`#hashtag` 沒有空白，不是標題");
        assert_ne!(st[5], h, "須行首：縮排過的 `#` 不算");
    }

    /// **gate (e) 的核心一條：散文的行首 `+`／`-` MUST NOT 染 diff 色。**
    ///
    /// 清單用 `-` 開頭是常態，染成「刪除行」是直接的誤導。同一份內容放進
    /// ` ```diff ` 之後才 MUST 染。
    #[test]
    fn prose_plus_and_minus_are_never_coloured_as_diff() {
        let (add, del) = (theme::diff_add_style(), theme::diff_del_style());
        let st = body_styles("- item one\n+ item two\n-5 度是散文\n+1 也是\n");
        for (i, s) in st.iter().enumerate() {
            assert_ne!(*s, del, "第 {i} 列是散文，MUST NOT 染刪除色");
            assert_ne!(*s, add, "第 {i} 列是散文，MUST NOT 染新增色");
        }
        // 前兩列是清單：只有 marker 上色，內文不上色
        assert_eq!(st[0], theme::md_list_marker_style());
        assert_eq!(st[1], theme::md_list_marker_style());

        // 同一份內容進 ```diff → MUST 染
        let st = body_styles("```diff\n- item one\n+ item two\n```\n- back to prose\n");
        assert_eq!(st[1], del, "```diff 段內的 `-` MUST 染刪除色");
        assert_eq!(st[2], add, "```diff 段內的 `+` MUST 染新增色");
        assert_eq!(st[4], theme::md_list_marker_style(), "出了 fence 就是清單");

        // 明確 diff 區段（`diff --git`／`@@`）：染；空行結束區段之後不再染
        let st = body_styles("diff --git a/x b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n\n- next steps\n");
        assert_eq!(st[2], del);
        assert_eq!(st[3], add);
        assert_eq!(
            st[5],
            theme::md_list_marker_style(),
            "空行結束 diff 區段：後面的 `-` 是清單，不是刪除行"
        );
    }

    /// 只染 marker、不染內文（清單）；中繼標頭只染 key、不染值。
    #[test]
    fn list_markers_and_meta_keys_colour_only_their_own_span() {
        let p = pager_of("- item one\ntask-id: 20260801T000000Z-bbbb\nsee: below\n");
        let lines = highlight_pager(&crate::action::pager_lines(&p));

        // 標頭四列之後：清單列切成 `-`（上色）＋` item one`（原色）
        let list = &lines[4];
        assert_eq!(list.spans[0].content.as_ref(), "-");
        assert_eq!(list.spans[0].style, theme::md_list_marker_style());
        assert_eq!(list.spans[1].content.as_ref(), " item one");
        assert_eq!(
            list.spans[1].style,
            ratatui::style::Style::default(),
            "清單內文 MUST NOT 上色"
        );

        let meta = &lines[5];
        assert_eq!(meta.spans[0].content.as_ref(), "task-id:");
        assert_eq!(meta.spans[0].style, theme::md_meta_key_style());
        assert_eq!(
            meta.spans[1].style,
            ratatui::style::Style::default(),
            "值是證據，MUST NOT 上色"
        );

        // 內文冒號不是中繼標頭（白名單之外）
        let prose = &lines[6];
        assert_eq!(prose.spans.len(), 1);
        assert_eq!(
            prose.spans[0].style,
            ratatui::style::Style::default(),
            "`see:` 不在白名單，MUST NOT 誤染"
        );
    }
}

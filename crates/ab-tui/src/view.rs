//! render 層（tui-design.md §2 版面的第一縱切子集：OWNERS｜WORKERS 兩欄）。
//!
//! 顯示紀律（§2／§5）：task status 一律顯示權威字（queued/delivered/running
//! ——不存在 `blocked`）；selection 以文字 marker `▶` 呈現（`capture-pane`
//! 的特徵字串斷言吃不到 style，marker 必須落在字元層）；不得以顏色／排序
//! 暗示可刪度。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::action::{cancel_cmdline, evidence};
use crate::app::{App, Panel, Sel};
use crate::model::{
    Blocker, BlockerIndex, LiveIndex, Liveness, Model, Row, owner_liveness, pane_liveness,
};
use crate::theme;

/// task id 的固定長度（`YYYYMMDDTHHMMSSZ-xxxx`，見 `ab_core::task`）。
const TASK_ID_W: u16 = 21;
/// 中欄最小寬：選中 marker（2）＋列 glyph（2）＋完整 task id ＋左右邊框（2）。
const MID_MIN_W: u16 = 4 + TASK_ID_W + 2;
/// OWNERS 欄：marker（2）＋游標（2）＋死活 glyph（2）＋標籤＋邊框。窄畫面下
/// 先犧牲的是這裡（owner 標籤截斷不影響證據語意）。
const OWNERS_W: u16 = 20;
/// DETAIL 欄：容得下最長的等價 CLI 原文 `  $ agent-bridge read <id>`
/// （2＋2＋18＋21＝43）＋左右邊框。
const DETAIL_W: u16 = 43 + 2;
/// 三欄同時成立所需的最小寬。不足時 DETAIL 改走整寬底條（見 `render`）。
const THREE_COL_MIN_W: u16 = OWNERS_W + MID_MIN_W + DETAIL_W;
/// 底條模式下 DETAIL 的高度：task 細節 5 行＋空行＋`evidence:`＋2 條命令＋
/// 上下邊框。
const DETAIL_STRIP_H: u16 = 11;
/// footer 一次畫得出的 sticky 警告則數（多的以「另有 N 則」帶出，見
/// `render_footer`）。
const WARN_ROWS: usize = 3;

pub fn render(f: &mut Frame, model: &Model, live: &LiveIndex, blockers: &BlockerIndex, app: &App) {
    // footer 高度隨 sticky 警告伸縮（major #2：警告不得被單行 message 覆寫，
    // 所以它們需要自己的行）。上限 `WARN_ROWS`，溢位以「另 N 則」帶出，
    // 不是靜默丟掉
    // ＋1 是「（Esc 清除警告）」那行——只在有警告時才佔位
    let warn_rows = if app.warnings.is_empty() {
        0
    } else {
        app.warnings.len().min(WARN_ROWS) as u16 + 1
    };
    let [main, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2 + warn_rows)]).areas(f.area());
    // §2 版面：三欄＋中欄縱切。兩條硬不變量同時要守——中欄那 21 字元的
    // immutable task id MUST 永不截斷（它是 dashboard 全部「證據」語意的
    // 承重點，§2／§5），而 DETAIL 的等價 CLI 原文 MUST 完整留在畫面上
    // （薄殼原則，§2：截半條命令等於畫面上沒有那條命令）。
    //
    // 三者相加要 92 欄，80 欄的終端機放不下——**寬度不足時 DETAIL 改走整寬
    // 底條**，而不是壓縮任何一方。80 欄下底條有整整 78 欄，命令原文照樣成行；
    // 犧牲的只是垂直空間，那是列表捲動本來就處理得了的。
    let (owners_area, mid_area, detail_area) = if main.width >= THREE_COL_MIN_W {
        let [o, m, d] = Layout::horizontal([
            Constraint::Length(OWNERS_W),
            Constraint::Min(MID_MIN_W),
            Constraint::Length(DETAIL_W),
        ])
        .areas(main);
        (o, m, d)
    } else {
        let [top, d] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(DETAIL_STRIP_H)]).areas(main);
        let [o, m] = Layout::horizontal([Constraint::Length(OWNERS_W), Constraint::Min(MID_MIN_W)])
            .areas(top);
        (o, m, d)
    };
    let [workers_area, tasks_area] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid_area);

    render_owners(f, owners_area, model, live, app);
    render_workers(f, workers_area, model, live, blockers, app);
    render_tasks(f, tasks_area, model, app);
    render_detail(f, detail_area, model, live, blockers, app);
    render_footer(f, footer, app);

    // overlay 優先序：確認框（cancel／evict）> 全文 pager > 摘要頁 > 合法鍵
    if let Some(id) = &app.confirm {
        render_confirm(f, id);
    } else if let Some(p) = &app.evict_prompt {
        render_evict_confirm(f, &p.lines);
    } else if app.pager.is_some() {
        render_pager(f, app);
    } else if let Some(lines) = &app.info {
        render_info(f, lines);
    } else if app.help {
        render_help(f, model, app);
    }
}

fn glyph(l: Liveness) -> &'static str {
    match l {
        Liveness::Live => "●",
        Liveness::Dead => "✗",
        Liveness::Unknown => "?",
    }
}

fn sel_prefix(selected: bool) -> &'static str {
    if selected { "▶ " } else { "  " }
}

fn render_owners(f: &mut Frame, area: Rect, model: &Model, live: &LiveIndex, app: &App) {
    let focused = app.panel == Panel::Owners;
    let mut lines = Vec::new();
    for (i, o) in model.owners.iter().enumerate() {
        let selected = focused && i == app.owner_idx;
        let marker = if i == app.owner_idx { "▸" } else { " " };
        // 拆 Span 只為了讓死活 glyph 單獨上色；**字元逐字等同**原本的
        // `format!("{}{marker} {} {o}", …)`，capture-pane 的字元斷言不受影響
        let l = owner_liveness(live, o);
        lines.push(styled(
            vec![
                Span::raw(format!("{}{marker} ", sel_prefix(selected))),
                Span::styled(glyph(l), theme::liveness_style(l)),
                Span::raw(format!(" {o}")),
            ],
            selected,
        ));
    }
    let block = panel_block("OWNERS", focused);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll_offset(app.owner_idx, area), 0)),
        area,
    );
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
    let mut lines = Vec::new();
    for (i, row) in rows.iter().enumerate() {
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
        lines.push(Line::from("  （此 owner 下沒有 worker）"));
    }
    let block = panel_block("WORKERS", focused);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll_offset(app.row_idx, area), 0)),
        area,
    );
}

/// TASKS 欄（§2）：當前 owner 底下所有 worker 的任務平坦列表，**含終態**，
/// id 反序。沒有這一欄，畫面上就沒有 `r` 讀得動的任務（read 只對
/// completed／failed 合法）。
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
        lines.push(Line::from("  （此 owner 下沒有任務）"));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(panel_block("TASKS", focused))
            .scroll((scroll_offset(app.task_idx, area), 0)),
        area,
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
) {
    let sel = app.selection(model);
    let mut lines: Vec<Line> = Vec::new();
    match &sel {
        Sel::None => lines.push(Line::from("（無選中項）")),
        Sel::Owner(o) => {
            lines.push(Line::from(format!("owner : {o}")));
            lines.push(liveness_line("狀態  : ", owner_liveness(live, o)));
        }
        Sel::Worker(w) => {
            for l in worker_detail(w, live) {
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

fn worker_detail(w: &ab_core::registry::AgentSnapshot, live: &LiveIndex) -> Vec<Line<'static>> {
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
        liveness_line("狀態   : ", pane_liveness(live, &w.pane)),
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
        Blocker::Prompt => "permission/plan prompt（blocked）",
        Blocker::Occluded => "occluded（copy-mode：人正在看）",
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

/// `r` 的全螢幕 pager。bytes → 字串**只在這裡**做 lossy 轉換（action 層一律
/// 保留原始 bytes）。標頭三欄與 CLI 的 stderr 同一組欄位。
fn render_pager(f: &mut Frame, app: &App) {
    let Some(p) = &app.pager else { return };
    let text = String::from_utf8_lossy(&p.bytes).into_owned();
    let mut lines = vec![
        Line::from(format!("task-id: {}", p.id)),
        Line::from(format!("from: {}", p.from)),
        Line::from(format!("to: {}", p.to)),
        Line::from("────────"),
    ];
    for l in text.lines() {
        lines.push(Line::from(l.to_string()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("j/k（↓↑）捲動 · Esc/q 關閉"));
    let area = f.area();
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("read（唯讀全文）"),
            )
            .scroll((p.scroll as u16, 0)),
        area,
    );
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
        "worker 摘要（唯讀）",
        lines.iter().map(|l| Line::from(l.clone())).collect(),
    );
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![
        Line::from(
            " Tab/S-Tab/j/k（↓↑）導航 · Enter focus · r read · i 摘要 · c 複製證據 · x cancel · e evict · ? 合法鍵 · q 離開",
        ),
        Line::from(format!(" [poll 500ms · tmux 2s] {}", app.message)),
    ];
    // sticky 警告（最新的在最下面，人的視線落點）。畫得下幾則就畫幾則，
    // 剩下的以計數帶出——「被覆寫」與「畫面放不下但說得出還有幾則」是兩件事
    let n = app.warnings.len();
    if n > 0 {
        let shown = n.min(WARN_ROWS);
        let hidden = n - shown;
        for (i, w) in app.warnings[n - shown..].iter().enumerate() {
            let text = if i == 0 && hidden > 0 {
                format!(" ⚠ （另有 {hidden} 則較早的警告）{w}")
            } else {
                format!(" ⚠ {w}")
            };
            lines.push(Line::from(text).style(theme::warning_style()));
        }
        lines.push(Line::from(" （Esc 清除警告）"));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// `x` 的單確認框（§2 薄殼原則：畫面留下等價 CLI 原文；§5：cancel 綁
/// immutable task id，單確認即可）。
fn render_confirm(f: &mut Frame, id: &str) {
    let cmd = format!("$ {}", cancel_cmdline(id));
    let lines = vec![
        Line::from("確認後執行下列等價 CLI："),
        Line::from(cmd.clone()),
        Line::from("[y/Enter] 執行 · [n/Esc] 放棄"),
    ];
    let w = (cmd.chars().count() as u16 + 4).max(34);
    popup(f, w, 5, "取消任務", lines);
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
        "evict（派收尾任務後回收）",
        lines.iter().map(|l| Line::from(l.clone())).collect(),
    );
}

fn render_help(f: &mut Frame, model: &Model, app: &App) {
    // `?`＝當前選中項的合法鍵（§3）
    let mut lines = vec![Line::from(
        "Tab 換欄（S-Tab 反向）· j/k（↓↑）移動 · Esc 清警告 · ? 本頁 · q 離開",
    )];
    match app.panel {
        Panel::Owners => lines.push(Line::from("owner 列：無列上動作（Enter/x 無效）")),
        Panel::Workers => match app.selected_row(model) {
            Some(Row::Worker(_)) => {
                lines.push(Line::from(
                    "worker 列：Enter focus 其 pane；e evict（證據框）；x 無效（僅 task 列）",
                ));
            }
            Some(Row::Task { .. }) => {
                lines.push(Line::from(
                    "task 列：Enter focus 所屬 worker；x cancel（單確認）",
                ));
            }
            None => lines.push(Line::from("（無選中列）")),
        },
        Panel::Tasks => match app.selection(model) {
            Sel::Task { task, .. } => {
                lines.push(Line::from("task 列：r 讀全文 · i 摘要 · c 複製證據"));
                if crate::model::is_terminal_status(&task.status) {
                    lines.push(Line::from("終態任務：x 無效（已無可取消的轉換）"));
                } else {
                    lines.push(Line::from("非終態任務：x cancel（單確認）"));
                }
            }
            _ => lines.push(Line::from("（無選中列）")),
        },
    }
    lines.push(Line::from("按任意鍵關閉"));
    // 寬度要容得下最長那行（worker 列的合法鍵，CJK 雙寬）：popup 不換行，
    // 截半句等於畫面上沒有那條規則
    popup(f, 74, 7, "合法鍵（當前選中項）", lines);
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
    let rows: usize = lines.iter().map(|l| l.width().div_ceil(inner).max(1)).sum();
    let h = h.max(rows as u16 + 2);
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
            spawn_tag: "t1".to_string(),
            registered_at: "2026-08-01T00:00:00Z".to_string(),
            spawned,
            corrupt: false,
        }
    }

    fn task(id: &str, to: &str, status: &str) -> InFlight {
        InFlight {
            id: id.to_string(),
            from: "boss".to_string(),
            to: to.to_string(),
            status: status.to_string(),
        }
    }

    /// 一個涵蓋全部語意的最小 fixture：五種 status、三種死活、一個 blocker、
    /// 一個選取列、focus／非 focus 面板各一。純資料，**不碰 tmux 與磁碟**。
    fn fixture() -> (Model, LiveIndex, BlockerIndex, App) {
        let model = Model {
            owners: vec!["-".to_string(), "s:@1".to_string(), "s:@9".to_string()],
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
        };
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), vec![("s".to_string(), "@1".to_string())]);
        panes.insert("%5".to_string(), vec![("s".to_string(), "@1".to_string())]);
        let live = LiveIndex {
            panes: Some(panes),
            windows: Some(vec!["@1".to_string()]),
        };
        let mut bl = HashMap::new();
        bl.insert("%1".to_string(), Blocker::Prompt);
        bl.insert("%2".to_string(), Blocker::None);
        bl.insert("%5".to_string(), Blocker::Occluded);
        let blockers = BlockerIndex { panes: Some(bl) };

        let mut app = App::new();
        app.panel = Panel::Workers; // WORKERS focus、OWNERS 非 focus
        app.owner_idx = 1; // 選中 owner "s:@1"
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
        let (model, live, blockers, mut app) = fixture();
        tweak(&mut app);
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render(f, &model, &live, &blockers, &app))
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

    /// WORKERS／DETAIL 兩欄的 x 起點（120 欄、三欄版面）。
    const WORKERS_X0: u16 = OWNERS_W;
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

    /// 三態死活各自的色。`Unknown` MUST 與 `Dead` 不同色——它們是不同的事實
    /// （§5 三態不得壓成兩態），畫面上就要分得出來。
    #[test]
    fn liveness_glyphs_keep_three_states_apart() {
        let buf = draw();
        assert_eq!(style_at(&buf, "● s:@1").fg, Some(Color::Green));
        assert_eq!(style_at(&buf, "✗ s:@9").fg, Some(Color::Red));
        assert_eq!(style_at(&buf, "? -").fg, Some(Color::DarkGray));
        // WORKERS 欄的死活後綴同一組色
        assert_eq!(style_at(&buf, "✗dead").fg, Some(Color::Red));
    }

    /// blocker 是 Red＋BOLD；且 **`none` MUST NOT 上色**——把「沒有 blocker」
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

        // Unknown liveness 的 owner 列（OWNERS 欄，選中 manual owner "-"）
        let buf2 = draw_with(|a| {
            a.panel = Panel::Owners;
            a.owner_idx = 0; // "-"＝manual，死活 Unknown
        });
        let (gx, gy) = find(&buf2, "? -");
        let g = buf2[(gx, gy)].style();
        assert_eq!(g.bg, Some(Color::Blue));
        assert_ne!(g.fg, g.bg, "Unknown glyph 的 fg 與選取 bg 同色＝glyph 隱形");
    }

    /// 非 focus 面板的**標題**不得被邊框的 DarkGray 一起壓暗（審查 minor #2）：
    /// 面板叫什麼名字是導航資訊，不是裝飾。
    #[test]
    fn unfocused_panel_title_is_not_dimmed_with_its_border() {
        let buf = draw();
        // OWNERS 非 focus：邊框 DarkGray，但標題不該是
        let (x, y) = find(&buf, "OWNERS");
        assert_ne!(
            buf[(x, y)].style().fg,
            Some(Color::DarkGray),
            "非 focus 面板的標題被邊框色壓暗了"
        );
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::DarkGray), "邊框仍應暗");
    }

    /// focus／非 focus 面板：粗框＋BOLD vs DarkGray 邊框。
    #[test]
    fn focused_panel_is_distinguishable_from_the_rest() {
        let buf = draw();
        // OWNERS 非 focus：左上角是細框且邊框色 DarkGray
        let owners_corner = &buf[(0, 0)];
        assert_eq!(owners_corner.symbol(), "┌");
        assert_eq!(owners_corner.style().fg, Some(Color::DarkGray));
        // WORKERS focus：左上角是粗框且帶 BOLD
        let workers_corner = &buf[(OWNERS_W, 0)];
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
        for g in ["●", "✗", "⛔"] {
            assert!(t.contains(g), "畫面上少了 glyph「{g}」");
        }
        // `blocked` 不是 task 狀態字（它是 BLOCKER 軸的字面）：theme MUST NOT
        // 認得它，否則等於承認了一個不存在的 task 狀態（tui-design.md §2）
        assert_eq!(
            theme::status_style("blocked"),
            ratatui::style::Style::default(),
            "blocked 不是 task 狀態字，MUST NOT 有 status 對映色"
        );
    }
}

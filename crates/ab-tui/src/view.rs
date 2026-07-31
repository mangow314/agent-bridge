//! render 層（tui-design.md §2 版面的第一縱切子集：OWNERS｜WORKERS 兩欄）。
//!
//! 顯示紀律（§2／§5）：task status 一律顯示權威字（queued/delivered/running
//! ——不存在 `blocked`）；selection 以文字 marker `▶` 呈現（`capture-pane`
//! 的特徵字串斷言吃不到 style，marker 必須落在字元層）；不得以顏色／排序
//! 暗示可刪度。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::action::{cancel_cmdline, evidence};
use crate::app::{App, Panel, Sel};
use crate::model::{LiveIndex, Liveness, Model, Row, owner_liveness, pane_liveness};

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

pub fn render(f: &mut Frame, model: &Model, live: &LiveIndex, app: &App) {
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
        let [o, m] =
            Layout::horizontal([Constraint::Length(OWNERS_W), Constraint::Min(MID_MIN_W)])
                .areas(top);
        (o, m, d)
    };
    let [workers_area, tasks_area] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid_area);

    render_owners(f, owners_area, model, live, app);
    render_workers(f, workers_area, model, live, app);
    render_tasks(f, tasks_area, model, app);
    render_detail(f, detail_area, model, live, app);
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
        let text = format!(
            "{}{marker} {} {o}",
            sel_prefix(selected),
            glyph(owner_liveness(live, o))
        );
        lines.push(styled(text, selected));
    }
    let block = panel_block("OWNERS", focused);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll_offset(app.owner_idx, area), 0)),
        area,
    );
}

fn render_workers(f: &mut Frame, area: Rect, model: &Model, live: &LiveIndex, app: &App) {
    let focused = app.panel == Panel::Workers;
    let rows = app.rows(model);
    let mut lines = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let selected = focused && i == app.row_idx;
        let text = match *row {
            Row::Worker(wi) => {
                let w = &model.workers[wi];
                let rt = if w.runtime.is_empty() {
                    "-"
                } else {
                    &w.runtime
                };
                let alive = match pane_liveness(live, &w.pane) {
                    Liveness::Live => "",
                    Liveness::Dead => "  ✗dead",
                    Liveness::Unknown => "  ?",
                };
                format!(
                    "{}▸ {}  {}  {}  {}{}",
                    sel_prefix(selected),
                    w.name,
                    rt,
                    if w.pane.is_empty() { "-" } else { &w.pane },
                    w.ready,
                    alive
                )
            }
            Row::Task { task, .. } => {
                let t = &model.tasks[task];
                // status 是權威字（queued/delivered/running），不縮寫不造詞。
                // 縮排只留 `└ `：中欄最窄（MID_MIN_W）時 task id 仍要整條進得去
                format!("{}└ {}  {}", sel_prefix(selected), t.id, t.status)
            }
        };
        lines.push(styled(text, selected));
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
        lines.push(styled(
            format!(
                "{}{g} {}  {}  {}",
                sel_prefix(selected),
                t.id,
                t.to,
                t.status
            ),
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
fn render_detail(f: &mut Frame, area: Rect, model: &Model, live: &LiveIndex, app: &App) {
    let sel = app.selection(model);
    let mut lines: Vec<Line> = Vec::new();
    match &sel {
        Sel::None => lines.push(Line::from("（無選中項）")),
        Sel::Owner(o) => {
            lines.push(Line::from(format!("owner : {o}")));
            lines.push(Line::from(format!(
                "狀態  : {}",
                liveness_word(owner_liveness(live, o))
            )));
        }
        Sel::Worker(w) => {
            for l in worker_detail(w, live) {
                lines.push(Line::from(l));
            }
        }
        Sel::Task { task, worker } => {
            lines.push(Line::from(format!("task-id: {}", task.id)));
            lines.push(Line::from(format!("from   : {}", task.from)));
            lines.push(Line::from(format!("to     : {}", task.to)));
            lines.push(Line::from(format!("status : {}", task.status)));
            if let Some(w) = worker {
                lines.push(Line::from(format!(
                    "pane   : {}  {}",
                    if w.pane.is_empty() { "-" } else { &w.pane },
                    liveness_word(pane_liveness(live, &w.pane))
                )));
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

fn worker_detail(w: &ab_core::registry::AgentSnapshot, live: &LiveIndex) -> Vec<String> {
    vec![
        format!("name   : {}", w.name),
        format!("pane   : {}", if w.pane.is_empty() { "-" } else { &w.pane }),
        format!(
            "runtime: {}",
            if w.runtime.is_empty() {
                "-"
            } else {
                &w.runtime
            }
        ),
        format!("ready  : {}", w.ready),
        format!("狀態   : {}", liveness_word(pane_liveness(live, &w.pane))),
    ]
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
            " Tab/j/k（↓↑）導航 · Enter focus · r read · i 摘要 · c 複製證據 · x cancel · e evict · ? 合法鍵 · q 離開",
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
            lines.push(Line::from(text).style(Style::default().add_modifier(Modifier::BOLD)));
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
    let mut lines = vec![Line::from("Tab 換欄 · j/k（↓↑）移動 · ? 本頁 · q 離開")];
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

fn styled(text: String, selected: bool) -> Line<'static> {
    if selected {
        Line::from(text).style(Style::default().add_modifier(Modifier::REVERSED))
    } else {
        Line::from(text)
    }
}

fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let b = Block::default().borders(Borders::ALL).title(title);
    if focused {
        b.border_style(Style::default().add_modifier(Modifier::BOLD))
    } else {
        b
    }
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
    let rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner).max(1))
        .sum();
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

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
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::action::cancel_cmdline;
use crate::app::{App, Panel};
use crate::model::{LiveIndex, Liveness, Model, Row, owner_liveness, pane_liveness};

pub fn render(f: &mut Frame, model: &Model, live: &LiveIndex, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(f.area());
    let [owners_area, workers_area] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(24)]).areas(main);

    render_owners(f, owners_area, model, live, app);
    render_workers(f, workers_area, model, live, app);
    render_footer(f, footer, app);

    if let Some(id) = &app.confirm {
        render_confirm(f, id);
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
                // status 是權威字（queued/delivered/running），不縮寫不造詞
                format!("{}  └ {}  {}", sel_prefix(selected), t.id, t.status)
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

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let lines = vec![
        Line::from(" Tab/j/k（↓↑）導航 · Enter focus · x cancel · ? 合法鍵 · q 離開"),
        Line::from(format!(" [poll 500ms · tmux 2s] {}", app.message)),
    ];
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

fn render_help(f: &mut Frame, model: &Model, app: &App) {
    // `?`＝當前選中項的合法鍵（§3）
    let mut lines = vec![Line::from("Tab 換欄 · j/k（↓↑）移動 · ? 本頁 · q 離開")];
    match app.panel {
        Panel::Owners => lines.push(Line::from("owner 列：無列上動作（Enter/x 無效）")),
        Panel::Workers => match app.selected_row(model) {
            Some(Row::Worker(_)) => {
                lines.push(Line::from(
                    "worker 列：Enter focus 其 pane；x 無效（僅 task 列）",
                ));
            }
            Some(Row::Task { .. }) => {
                lines.push(Line::from(
                    "task 列：Enter focus 所屬 worker；x cancel（單確認）",
                ));
            }
            None => lines.push(Line::from("（無選中列）")),
        },
    }
    lines.push(Line::from("按任意鍵關閉"));
    popup(f, 58, 6, "合法鍵（當前選中項）", lines);
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
    let h = h.min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string()),
        ),
        rect,
    );
}

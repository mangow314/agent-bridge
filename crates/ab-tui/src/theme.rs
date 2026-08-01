//! 樣式集中層（P4.5 視覺樣式）。**view.rs 只消費，不自己寫 `Style`**。
//!
//! 為什麼集中：樣式散在 render 各處時，「哪個語意是什麼色」沒有單一正本，
//! 改一處漏一處不會有任何訊號——同 `blocker_mark`／`liveness_word` 把字面
//! 收在一處的理由（§2 顯示紀律）。
//!
//! 兩條硬紅線寫在型別層面之外，實作時必須自己守住：
//!
//! - **只加樣式，不動字元**：既有 shell 測試分組全部是 `capture-pane` 的字元
//!   比對，`Style` 吃不進 capture 的字元層。任何 cell 的字元 MUST 與上色前
//!   逐字相同（含空白對齊）。
//! - **顏色只編碼五種語意**：status／liveness／blocker／focus／warning。
//!   **MUST NOT** 用顏色暗示「可刪度」——idle worker 不上任何暗示色
//!   （tui-design.md §5：「沒有任何訊號」≠「可安全刪除」）。
//!   （warning 是核准計畫裡的第五軸；早期註解只列四軸，與 `warning_style`
//!   自相矛盾——審查 minor #5。）
//!
//! 色盤限 **ANSI 16**（使用者已否決 truecolor 自訂 palette）：終端機主題各異，
//! 寫死 RGB 會跟使用者的配色打架，而語意色的目的是「一眼分辨」，不是好看。

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::BorderType;

use crate::model::{Blocker, Liveness};

/// task status 的語意色。**輸入是權威字**（`spec/state.md`／`task.rs`），
/// 不是縮寫也不是自造詞——顏色只加碼，不改字（tui-design.md §2）。
///
/// 未知字面回 default 而不是隨便給個色：權威字集合若擴充，沒有對映時
/// 「不上色」比「上錯色」誠實。
pub fn status_style(status: &str) -> Style {
    let c = match status {
        "running" => Color::Cyan,
        "completed" => Color::Green,
        "failed" => Color::Red,
        "queued" | "delivered" => Color::Yellow,
        "cancelled" => Color::DarkGray,
        _ => return Style::default(),
    };
    Style::default().fg(c)
}

/// 三態死活的語意色。`Unknown` 用 DarkGray 而不是不上色：它與 `Dead` 是
/// **不同**的事實（§5 三態不得壓成兩態），畫面上要分得出來。
pub fn liveness_style(l: Liveness) -> Style {
    let c = match l {
        Liveness::Live => Color::Green,
        Liveness::Dead => Color::Red,
        Liveness::Unknown => Color::DarkGray,
    };
    Style::default().fg(c)
}

/// BLOCKER 軸的語意色。**只有真的被擋住才上色**（`Prompt` → Red＋BOLD）。
///
/// 回 `Option` 而不是「其餘給 default」，是要讓呼叫端明確處理「不上色」這件事：
/// `None`（沒有 blocker）上紅色會謊報，`Occluded`（人正在看）也不是異常——
/// 那是人在介入，不該畫成警報。`Unknown` 更不能上色（沒有訊號 ≠ 有問題）。
pub fn blocker_style(b: Blocker) -> Option<Style> {
    match b {
        Blocker::Prompt => Some(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Blocker::None | Blocker::Occluded | Blocker::Unknown => None,
    }
}

/// 選取列：**背景**色，不用 `Modifier::REVERSED`。
///
/// REVERSED 會把整列的前景／背景對調，語意色（status／liveness）跟著被翻成
/// 背景色而失去意義——選取與語意是兩個正交的軸，疊加時不該互相吃掉。
/// 改用 bg 之後各 Span 的 fg 原樣保留（`Line::style` 是底、Span 樣式 patch 在上）。
///
/// **背景色 MUST NOT 與任何語意前景色相同**（審查 major #1）：原本用
/// DarkGray，而 `cancelled` 的 status 色與 `Liveness::Unknown` 也是 DarkGray
/// ——選中那一列時該 cell 變成 `fg=DarkGray, bg=DarkGray`，字**實際消失**。
/// 選取是最常發生的互動，把選中的那一行變成空白是最糟的失效方向。
/// Blue 是 ANSI 16 裡沒有被任何語意前景佔用的一個。
pub fn selected_row_style() -> Style {
    Style::default().bg(Color::Blue)
}

/// focus 面板的邊框樣式：粗框（`BorderType::Thick`）＋既有 BOLD。
///
/// 非 focus 走 DarkGray——**降低非焦點的存在感**，而不是提高焦點的飽和度：
/// 四個面板同時亮起來的畫面沒有焦點可言。
pub fn panel_border_type(focused: bool) -> BorderType {
    if focused {
        BorderType::Thick
    } else {
        BorderType::Plain
    }
}

pub fn panel_border_style(focused: bool) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// 面板標題：focus 時 BOLD，兩種情況都**不繼承邊框色**。
///
/// 用 `Style::reset()` 起手而不是 `Style::default()`（審查 minor #2）：
/// `default()` 每個欄位都是 `None`，patch 上去是 no-op——標題於是照單全收
/// 邊框的 DarkGray，非 focus 面板連「叫什麼名字」都被壓暗。那與這個函式
/// 存在的理由正好相反。`reset()` 明確把 fg／bg 打回終端機預設，標題才真的
/// 與邊框脫鉤。
pub fn panel_title_style(focused: bool) -> Style {
    let base = Style::reset();
    if focused {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

/// footer 的 sticky 警告：既有 BOLD ＋ Yellow。
///
/// 不用 Red：警告是「請你看一眼」，不是「出錯了」——Red 在本 dashboard 已被
/// `failed` 與 blocker 佔用，再拿來當警告色會讓三件不同的事看起來一樣嚴重。
pub fn warning_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

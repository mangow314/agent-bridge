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
//! - **顏色只編碼六種語意**：status／liveness／blocker／focus／warning，加上
//!   P4.6 切片 D 的 **content-syntax**（pager 的 markdown-lite，見本檔末）。
//!   第六軸只活在 `r` 的全螢幕 overlay 裡，與前五軸永不同框。
//!   **MUST NOT** 用顏色暗示「可刪度」——idle worker 不上任何暗示色
//!   （tui-design.md §5：「沒有任何訊號」≠「可安全刪除」）。
//!   （warning 是核准計畫裡的第五軸；早期註解只列四軸，與 `warning_style`
//!   自相矛盾——審查 minor #5。）
//!
//! ## 色盤：ANSI 16 ＋ truecolor 自動升級（P5.5，**翻案**）
//!
//! 早期版本在此記錄「使用者已否決 truecolor 自訂 palette」，理由是終端機主題
//! 各異、寫死 RGB 會與使用者配色打架。**2026-08-04 使用者重新裁定**：採
//! truecolor **並保留自動降級**——`COLORTERM` 說得出 `truecolor`／`24bit` 才
//! 升級，說不出就逐字沿用原本那份 ANSI 16。原否決要防的事因此仍然成立：
//! 沒有 24-bit 訊號的終端機看到的畫面與翻案前**完全相同**。
//!
//! 兩份色盤都只是同一組語意名的取值（`Palette`），語意軸沒有增加。

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::BorderType;

use crate::model::{Blocker, Liveness};

/// 語意 → 顏色的**取值表**。欄位名是語意，不是顏色名——「哪個語意用什麼色」
/// 可以換，「有哪些語意」不行（換一份色盤不得偷渡第七個軸）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    // status 軸（權威字 → 色）
    running: Color,
    completed: Color,
    failed: Color,
    /// queued／delivered：還沒開始跑
    pending: Color,
    // liveness 軸
    live: Color,
    dead: Color,
    // blocker 軸（只有 `Prompt` 有色）
    blocked: Color,
    /// 三處共用的「低存在感」：`cancelled`／`Unknown`／捲軸與非焦點邊框。
    /// 共用同一格是刻意的——它們講的是同一件事「這裡沒有需要你現在看的東西」。
    dim: Color,
    /// 選取列的**背景**。MUST NOT 與任何語意前景相同（見 `selected_row_style`）。
    selected_bg: Color,
    warning: Color,
    // content-syntax 軸（pager overlay 專用）
    md_heading: Color,
    md_code: Color,
    md_list: Color,
    md_meta: Color,
    diff_add: Color,
    diff_del: Color,
}

/// 翻案前的那一份，**逐字不動**：沒有 24-bit 訊號的終端機畫出來的東西與
/// P5.5 之前完全相同（這是翻案能成立的前提）。
const ANSI16: Palette = Palette {
    running: Color::Cyan,
    completed: Color::Green,
    failed: Color::Red,
    pending: Color::Yellow,
    live: Color::Green,
    dead: Color::Red,
    blocked: Color::Red,
    dim: Color::DarkGray,
    selected_bg: Color::Blue,
    warning: Color::Yellow,
    md_heading: Color::Magenta,
    md_code: Color::Cyan,
    md_list: Color::Yellow,
    md_meta: Color::Blue,
    diff_add: Color::Green,
    diff_del: Color::Red,
};

/// 24-bit 版。取值原則：**同一個語意在兩份色盤裡認得出是同一件事**（紅還是
/// 紅、綠還是綠），只是降飽和到長時間盯著不刺眼的區間。
///
/// `selected_bg` 刻意取深藍而非亮藍：它是**背景**，前景的六個語意色要能疊在
/// 它上面還讀得出來（審查 major #1 的教訓——選取列不得把字吃掉）。
const TRUECOLOR: Palette = Palette {
    running: Color::Rgb(0x56, 0xB6, 0xC2),
    completed: Color::Rgb(0x98, 0xC3, 0x79),
    failed: Color::Rgb(0xE0, 0x6C, 0x75),
    pending: Color::Rgb(0xE5, 0xC0, 0x7B),
    live: Color::Rgb(0x98, 0xC3, 0x79),
    dead: Color::Rgb(0xE0, 0x6C, 0x75),
    blocked: Color::Rgb(0xE0, 0x6C, 0x75),
    dim: Color::Rgb(0x5C, 0x63, 0x70),
    selected_bg: Color::Rgb(0x2C, 0x4F, 0x7C),
    warning: Color::Rgb(0xE5, 0xC0, 0x7B),
    md_heading: Color::Rgb(0xC6, 0x78, 0xDD),
    md_code: Color::Rgb(0x56, 0xB6, 0xC2),
    md_list: Color::Rgb(0xE5, 0xC0, 0x7B),
    md_meta: Color::Rgb(0x61, 0xAF, 0xEF),
    diff_add: Color::Rgb(0x98, 0xC3, 0x79),
    diff_del: Color::Rgb(0xE0, 0x6C, 0x75),
};

/// `COLORTERM` → 色盤（純函式，好單測）。
///
/// **只認 `truecolor`／`24bit` 這兩個既成慣例字面**，其餘（含未設、空字串、
/// `256color`）一律降級。fail-closed 的方向在這裡是「降級」：把 RGB 送給
/// 不支援的終端機，最壞是整片色碼吐在畫面上。
pub fn detect(colorterm: Option<&str>) -> &'static Palette {
    match colorterm.map(str::trim) {
        Some("truecolor") | Some("24bit") => &TRUECOLOR,
        _ => &ANSI16,
    }
}

static PALETTE: OnceLock<&'static Palette> = OnceLock::new();

/// 進 alternate screen 之前呼叫一次。**沒呼叫就是 ANSI 16**——既有 render
/// 測試因此零漂移（它們不 init，畫出來的顏色與翻案前逐字相同）。
pub fn init_from_env() {
    let ct = std::env::var("COLORTERM").ok();
    let _ = PALETTE.set(detect(ct.as_deref()));
}

fn p() -> &'static Palette {
    PALETTE.get().copied().unwrap_or(&ANSI16)
}

/// task status 的語意色。**輸入是權威字**（`spec/state.md`／`task.rs`），
/// 不是縮寫也不是自造詞——顏色只加碼，不改字（tui-design.md §2）。
///
/// 未知字面回 default 而不是隨便給個色：權威字集合若擴充，沒有對映時
/// 「不上色」比「上錯色」誠實。
pub fn status_style(status: &str) -> Style {
    let p = p();
    let c = match status {
        "running" => p.running,
        "completed" => p.completed,
        "failed" => p.failed,
        "queued" | "delivered" => p.pending,
        "cancelled" => p.dim,
        _ => return Style::default(),
    };
    Style::default().fg(c)
}

/// 三態死活的語意色。`Unknown` 用 dim 而不是不上色：它與 `Dead` 是
/// **不同**的事實（§5 三態不得壓成兩態），畫面上要分得出來。
pub fn liveness_style(l: Liveness) -> Style {
    let p = p();
    let c = match l {
        Liveness::Live => p.live,
        Liveness::Dead => p.dead,
        Liveness::Unknown => p.dim,
    };
    Style::default().fg(c)
}

/// BLOCKER 軸的語意色。**只有真的被擋住才上色**（`Prompt` → 紅＋BOLD）。
///
/// 回 `Option` 而不是「其餘給 default」，是要讓呼叫端明確處理「不上色」這件事：
/// `None`（沒有 blocker）上紅色會謊報，`Occluded`（人正在看）也不是異常——
/// 那是人在介入，不該畫成警報。`Unknown` 更不能上色（沒有訊號 ≠ 有問題）。
pub fn blocker_style(b: Blocker) -> Option<Style> {
    match b {
        Blocker::Prompt => Some(Style::default().fg(p().blocked).add_modifier(Modifier::BOLD)),
        Blocker::None | Blocker::Occluded | Blocker::Unknown => None,
    }
}

/// triage 浮頂那幾列的名稱（P5.4）：**只加 BOLD，不上任何顏色**。
///
/// 為什麼不上色：這一列身上已經有 blocker／liveness／status 三個語意色在講
/// 「哪裡不對」，名稱再上一個顏色只會與它們搶注意力，還會冒出第七個沒有定義
/// 的語意軸。BOLD 說的是「這一列值得先看」——與排序是同一件事的兩種表達。
///
/// **這不是可刪度**（§5）：粗體只代表現在有需要人介入的訊號，不代表可以回收。
pub fn attention_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
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
/// 兩份色盤各自都守這條（見 `palette_invariants` 測試）。
pub fn selected_row_style() -> Style {
    Style::default().bg(p().selected_bg)
}

/// focus 面板的邊框樣式：粗框（`BorderType::Thick`）＋既有 BOLD。
///
/// 非 focus 走 dim——**降低非焦點的存在感**，而不是提高焦點的飽和度：
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
        Style::default().fg(p().dim)
    }
}

/// 面板標題：focus 時 BOLD，兩種情況都**不繼承邊框色**。
///
/// 用 `Style::reset()` 起手而不是 `Style::default()`（審查 minor #2）：
/// `default()` 每個欄位都是 `None`，patch 上去是 no-op——標題於是照單全收
/// 邊框的 dim，非 focus 面板連「叫什麼名字」都被壓暗。那與這個函式
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

/// TASKS 欄的捲軸（P4.6 切片 C）。與非 focus 邊框同一個 dim：捲軸講的是
/// 「清單有多長、你在哪」，是**方位**不是語意——上任何語意色都會讓它跟
/// status／liveness 搶注意力，而它一格資訊都沒有多給。
pub fn scrollbar_style() -> Style {
    Style::default().fg(p().dim)
}

/// 兩軸資料 stale 時的 footer 標記（P4.6 切片 C）：沿用警告色。
///
/// 「畫面上這份資料已經舊了」與 sticky 警告是同一類訊息——請你看一眼，不是
/// 出錯了。用紅色會讓它看起來像 `failed`／blocker 那種等級的事。
pub fn stale_style() -> Style {
    warning_style()
}

/// footer 的 sticky 警告：既有 BOLD ＋ 黃。
///
/// 不用紅：警告是「請你看一眼」，不是「出錯了」——紅在本 dashboard 已被
/// `failed` 與 blocker 佔用，再拿來當警告色會讓三件不同的事看起來一樣嚴重。
pub fn warning_style() -> Style {
    Style::default().fg(p().warning).add_modifier(Modifier::BOLD)
}

// ---- 第六軸：content-syntax（P4.6 切片 D，pager 的 markdown-lite）----
//
// 這一軸與前五軸（status／liveness／blocker／focus／warning）**在畫面上永不
// 共存**：它只作用在 `r` 的全螢幕 pager overlay 裡，而那張畫面上沒有任何
// status／liveness／blocker span，也不套 `selected_row_style`。因此顏色與前五
// 軸重用不會產生「同一格兩種語意」或「fg 撞 bg 而字消失」的問題——這是刻意
// 的取捨：色盤裝不下六個互斥的軸，而互斥只在同一張畫面上才有意義。
//
// 這一軸講的是**內容的語法結構**，不是狀態、不是嚴重度，更不是可刪度。

/// ATX 標題（`#`…`######`）。整列上色：標題是分段訊號，人靠它掃過長回覆。
pub fn md_heading_style() -> Style {
    Style::default()
        .fg(p().md_heading)
        .add_modifier(Modifier::BOLD)
}

/// fenced code block（含圍籬那兩列）。整段上色，人才看得出「這裡是原文，
/// 不是敘述」。
pub fn md_code_style() -> Style {
    Style::default().fg(p().md_code)
}

/// 清單項目的 marker（`-` / `*` / `+` / `1.`）。**只染 marker，不染內文**：
/// 整列上色會讓清單本身變成一片色塊，反而讀不出層次。
pub fn md_list_marker_style() -> Style {
    Style::default().fg(p().md_list)
}

/// `agent-bridge read` 標頭列的 key（`task-id:` 那組）。它是中繼資料，不是
/// 內文——染 key 不染值，值本身是證據（id／agent 名），維持原色。
pub fn md_meta_key_style() -> Style {
    Style::default().fg(p().md_meta).add_modifier(Modifier::BOLD)
}

/// diff 的新增／刪除行。**只在明確的 diff 情境下使用**（見
/// `view::highlight_pager`）：散文的行首 `-` 是清單常態，染成「刪除行」是
/// 直接的誤導（gate (e) 明文）。
pub fn diff_add_style() -> Style {
    Style::default().fg(p().diff_add)
}

pub fn diff_del_style() -> Style {
    Style::default().fg(p().diff_del)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `COLORTERM` 三案：認得的兩個字面升級，其餘一律降回 ANSI 16。
    #[test]
    fn colorterm_upgrades_only_on_a_declared_24bit_signal() {
        assert_eq!(detect(Some("truecolor")), &TRUECOLOR);
        assert_eq!(detect(Some("24bit")), &TRUECOLOR);
        // 未設／空／只宣稱 256 色／認不得的字面 → 降級
        for s in [None, Some(""), Some("256color"), Some("yes"), Some("TRUECOLOR")] {
            assert_eq!(
                detect(s),
                &ANSI16,
                "MUST 降級（把 RGB 送給不支援的終端機是把色碼吐在畫面上）：{s:?}"
            );
        }
        // 前後空白是環境變數的常見雜訊，不該害人降級
        assert_eq!(detect(Some(" truecolor ")), &TRUECOLOR);
    }

    /// **兩份色盤的不變量**（換色盤不得偷偷破壞已經論證過的東西）。
    #[test]
    fn palette_invariants_hold_for_both_palettes() {
        for (name, p) in [("ANSI16", &ANSI16), ("TRUECOLOR", &TRUECOLOR)] {
            // (1) 選取背景 MUST NOT 撞**會與它同框的**語意前景——撞了就是
            // 「選中那一列的字消失」（審查 major #1 的原始失效）。
            //
            // content-syntax 那一軸（`md_*`／`diff_*`）刻意不列入：它只活在
            // pager overlay 裡，而那張畫面不套 `selected_row_style`（見本檔
            // 開頭的說明）。ANSI 16 下 `md_meta` 與選取背景同為 Blue 正是這個
            // 取捨的具體結果，不是漏網。
            let fgs = [
                p.running,
                p.completed,
                p.failed,
                p.pending,
                p.live,
                p.dead,
                p.blocked,
                p.dim,
                p.warning,
            ];
            for c in fgs {
                assert_ne!(c, p.selected_bg, "{name}：選取背景撞到語意前景 {c:?}");
            }
            // (2) 同一張畫面上要分得開的幾組 MUST 兩兩不同：status 四態、
            // 死活三態（dim＝unknown）、blocker
            let must_differ = [
                ("running", p.running),
                ("completed", p.completed),
                ("failed", p.failed),
                ("pending", p.pending),
                ("dim", p.dim),
            ];
            for (i, (an, a)) in must_differ.iter().enumerate() {
                for (bn, b) in must_differ.iter().skip(i + 1) {
                    assert_ne!(a, b, "{name}：{an} 與 {bn} 同色，畫面上分不開");
                }
            }
            // (3) 語意對應不得漂移：失敗與死亡同一個紅、成功與存活同一個綠
            // （§5：它們在人眼裡本來就是同一件事的兩種軸）
            assert_eq!(p.failed, p.dead, "{name}：failed 與 dead 該同色");
            assert_eq!(p.completed, p.live, "{name}：completed 與 live 該同色");
        }
    }

    /// **未 init＝ANSI 16**：既有 render 測試不呼叫 `init_from_env`，畫出來的
    /// 顏色 MUST 與翻案前逐字相同（翻案能成立的前提）。
    #[test]
    fn without_init_every_style_comes_from_the_ansi16_palette() {
        assert_eq!(status_style("running").fg, Some(ANSI16.running));
        assert_eq!(status_style("failed").fg, Some(ANSI16.failed));
        assert_eq!(status_style("cancelled").fg, Some(ANSI16.dim));
        assert_eq!(liveness_style(Liveness::Unknown).fg, Some(ANSI16.dim));
        assert_eq!(
            blocker_style(Blocker::Prompt).unwrap().fg,
            Some(ANSI16.blocked)
        );
        assert_eq!(selected_row_style().bg, Some(ANSI16.selected_bg));
        assert_eq!(scrollbar_style().fg, Some(ANSI16.dim));
        assert_eq!(warning_style().fg, Some(ANSI16.warning));
        // 未知權威字仍然不上色（擴充狀態時「不上色」比「上錯色」誠實）
        assert_eq!(status_style("blocked").fg, None);
    }

    /// 顏色以外的東西**不隨色盤變**：BOLD／邊框型別是結構訊號，不是配色。
    #[test]
    fn modifiers_and_border_types_are_not_part_of_the_palette() {
        assert!(
            blocker_style(Blocker::Prompt)
                .unwrap()
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(attention_style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(attention_style().fg, None, "強調 MUST NOT 自帶顏色");
        assert_eq!(panel_border_type(true), BorderType::Thick);
        assert_eq!(panel_border_type(false), BorderType::Plain);
    }
}

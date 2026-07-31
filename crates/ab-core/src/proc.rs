//! 行程身分（M5 窗 1）。對映 spec HOOK-OWNER-5、STATE-AGENT-4。
//!
//! 全部走 `/proc`，不引入新依賴。這一層只回報事實、不做判斷——「取不到就
//! 落回時間窗」的決策屬於 `hook::owner_gate`。

use std::path::Path;

/// `/proc/<pid>/stat` 在 comm 欄之後的欄位切片。
///
/// comm（第 2 欄）是可執行檔名，**可以含空白與右括號**（`(my prog)`），所以
/// 一律從**最後一個** `)` 之後開始切，不能整行 split_whitespace。切出來的
/// index 0 ＝第 3 欄（state），故第 N 欄的 index 是 `N - 3`。
fn stat_fields_after_comm(pid: &str) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &raw[raw.rfind(')')? + 1..];
    Some(tail.split_whitespace().map(str::to_string).collect())
}

/// 本行程的直接父行程 pid（`/proc/self/stat` 第 4 欄）。
///
/// 要的是**直接**父行程而不是祖先鏈：巢狀 runtime 的祖先鏈必然包含 worker
/// 本尊，鏈式比對區分不出來（spec HOOK-OWNER-4 Note 否決的正是那個方案）；
/// 直接父行程則是「誰 fork 了這個 hook」，本尊與巢狀各不相同。
pub fn self_ppid() -> Option<String> {
    let fields = stat_fields_after_comm("self")?;
    let ppid = fields.get(1)?;
    (!ppid.is_empty()).then(|| ppid.clone())
}

/// 指定 pid 的行程啟動刻度（`/proc/<pid>/stat` 第 22 欄，開機以來的 tick）。
///
/// 單獨的 pid 不足以識別行程：pid 會被回收重用，一個死掉的 worker 留下的
/// pid 可能已經是別人的。starttime 把它釘在一次具體的行程生命上。
pub fn starttime(pid: &str) -> Option<String> {
    if !is_plain_pid(pid) {
        return None;
    }
    let fields = stat_fields_after_comm(pid)?;
    let st = fields.get(19)?;
    (!st.is_empty()).then(|| st.clone())
}

/// 指定 pid 的 argv 裡，是否有任一項的 basename 等於 `name`。
///
/// 用途是確認「tmux 給的 `pane_pid` 真的是 runtime 本尊」（STATE-AGENT-4）。
///
/// **逐 argv 比 basename，而不是在整串裡找子字串**，兩種形狀都要分得出來：
///
/// - runtime 是腳本時 argv 是 `["bash", "/path/to/claude", …]`——argv[0] 是
///   直譯器，光看 argv[0] 會漏掉真正的 runtime。
/// - 中間夾了 shell 時 argv 是 `["sh", "-c", "…exec claude …"]`——整串裡
///   找得到 "claude"，但沒有任何一個 **argv 項**的 basename 是 `claude`。
///
/// 漏判（該命中而沒中）只是兩欄留空、落回時間窗；誤判（把 shell 當成
/// runtime）會記下錯的 pid，之後本尊的 hook 一律比對不符而被自己的閘門擋掉，
/// 比不做還糟。故形狀往「寧可漏判」那側設計。
///
/// **不讀 `/proc/<pid>/environ`**：那裡是行程的完整環境（含各種憑證），本
/// 專案的安全邊界明令不得讀取。spawn tag 存在環境而不在 argv，所以身分確認
/// 只能走 argv 這條較弱但無害的路。
pub fn argv_has_basename(pid: &str, name: &str) -> bool {
    if !is_plain_pid(pid) || name.is_empty() {
        return false;
    }
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    raw.split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        // argv 不保證合法 UTF-8；取 basename 後才轉字串，轉不了就不算命中
        .filter_map(|a| a.rsplit(|b| *b == b'/').next())
        .any(|base| base == name.as_bytes())
}

/// pid 字串只收純數字：它會被拼進 `/proc/<pid>/…` 路徑，`..` 之類的值
/// 會讓讀取指向別的地方。空字串（＝欄位留空的落回情形）同樣拒絕。
fn is_plain_pid(pid: &str) -> bool {
    !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
}

/// `/proc` 在本環境是否可用。不可用時 HOOK-OWNER-5 全面落回時間窗判別。
pub fn available() -> bool {
    Path::new("/proc/self/stat").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_ppid_matches_proc_and_is_numeric() {
        let ppid = self_ppid().expect("/proc/self/stat 應可讀");
        assert!(ppid.bytes().all(|b| b.is_ascii_digit()), "ppid={ppid}");
        // 自己的 starttime 也讀得到，且兩次讀取一致（同一個行程生命）
        let me = std::process::id().to_string();
        let a = starttime(&me).expect("自身 starttime 應可讀");
        assert_eq!(Some(a), starttime(&me));
    }

    #[test]
    fn comm_with_spaces_and_parens_does_not_shift_fields() {
        // 模擬 comm 含空白與右括號：欄位必須從最後一個 ')' 之後算起
        let raw = "42 (weird ) name) R 7 42 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 999888";
        let tail = &raw[raw.rfind(')').unwrap() + 1..];
        let f: Vec<&str> = tail.split_whitespace().collect();
        assert_eq!(f[1], "7", "第 4 欄 ppid");
        assert_eq!(f[19], "999888", "第 22 欄 starttime");
    }

    #[test]
    fn traversal_and_empty_pids_are_refused() {
        for bad in ["", "..", "1/../2", "self", "-1", "1 2"] {
            assert!(!is_plain_pid(bad), "應拒絕 {bad:?}");
            assert_eq!(starttime(bad), None, "應拒絕 {bad:?}");
            assert!(!argv_has_basename(bad, "x"), "應拒絕 {bad:?}");
        }
    }

    #[test]
    fn argv_basename_matches_own_binary_and_refuses_empty_name() {
        let me = std::process::id().to_string();
        // 自身 argv[0] 是帶路徑的測試執行檔，取 basename 後應命中
        let raw = std::fs::read(format!("/proc/{me}/cmdline")).unwrap();
        let argv0 = raw.split(|b| *b == 0).next().unwrap();
        let base =
            String::from_utf8(argv0.rsplit(|b| *b == b'/').next().unwrap().to_vec()).unwrap();
        assert!(argv_has_basename(&me, &base), "應命中自身 basename {base}");
        // 空名字永遠 false：否則「沒有 runtime 名可比」會退化成無條件相符
        assert!(!argv_has_basename(&me, ""));
        assert!(!argv_has_basename(&me, "__surely_not_an_argv__"));
    }
}

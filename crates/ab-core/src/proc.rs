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
    parse_stat_fields_after_comm(&raw)
}

/// `stat_fields_after_comm` 的純解析核心，吃一整行 stat 內容。分出來是為了讓
/// 測試直接餵極端形狀（comm 含空白／右括號）驗本體，而不是在測試裡另抄一份
/// 同樣的切法——抄一份的話這裡改壞了測試照樣綠。
fn parse_stat_fields_after_comm(raw: &str) -> Option<Vec<String>> {
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

/// 一份 `/proc/<pid>/cmdline` 位元組是否指向名為 `name` 的 runtime 本尊。
///
/// 純函式（不碰 `/proc`），形狀判定的全部規則都在這裡，測試直接餵位元組。
///
/// **只看前兩項 argv**，不是「任一項」：
///
/// - argv[0] 的 basename 等於 runtime 名——直接執行檔形。
/// - argv[0] 是直譯器（`bash /path/to/claude`、`env codex`）時，argv[1] 的
///   basename 等於 runtime 名。argv[1] 以 `-` 起首的是選項不是腳本，不看：
///   `sh -c "…exec claude…"` 的 argv[1] 是 `-c`，正確不命中。
///
/// 放寬成「任一 argv 項」會把完全不是 runtime 的行程認成 runtime——實測
/// `python -c 'sleep' codex` 的 argv 裡就有一項 basename 是 `codex`
/// （codex 複核 2026-07-31 §2 blocker 3）。
///
/// 漏判（該命中而沒中）只是兩欄留空、落回時間窗；誤判（把中介行程當成
/// runtime）會記下錯的 pid，本尊的 hook 之後一律比對不符，等於 M5 的自癒
/// 整條失效。故形狀一律往「寧可漏判」那側設計。
///
/// **不讀 `/proc/<pid>/environ`**：那裡是行程的完整環境（含各種憑證），本
/// 專案的安全邊界明令不得讀取。spawn tag 存在環境而不在 argv，所以身分確認
/// 只能走 argv 這條較弱但無害的路。
fn cmdline_is_runtime(raw: &[u8], name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // argv 不保證合法 UTF-8，全程比位元組，不做 lossy 轉換
    let mut argv = raw.split(|b| *b == 0).filter(|a| !a.is_empty());
    let Some(argv0) = argv.next() else {
        return false;
    };
    if basename(argv0) == name.as_bytes() {
        return true;
    }
    let Some(argv1) = argv.next() else {
        return false;
    };
    !argv1.starts_with(b"-") && basename(argv1) == name.as_bytes()
}

fn basename(item: &[u8]) -> &[u8] {
    item.rsplit(|b| *b == b'/').next().unwrap_or(item)
}

/// STATE-AGENT-4 的身分取樣：確認 `pid` 現在是名為 `name` 的 runtime，回傳它
/// 的 starttime；任何一步對不上都回 `None`（＝兩欄留空、hook 落回時間窗）。
///
/// **cmdline 與 starttime 必須是同一份行程快照**：分兩次讀的話，行程可能在
/// 中間退出而 pid 被重用，於是拿舊 runtime 的 argv 驗證成功、卻記下新行程的
/// starttime（codex 複核 §2 blocker 1）。所以 `starttime → cmdline →
/// starttime` 夾住，兩次相同才採用。
pub fn attest_runtime(pid: &str, name: &str) -> Option<String> {
    if !is_plain_pid(pid) || name.is_empty() {
        return None;
    }
    let before = starttime(pid)?;
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if !cmdline_is_runtime(&raw, name) {
        return None;
    }
    let after = starttime(pid)?;
    (after == before).then_some(before)
}

/// 指定 pid 的（直接父 pid, starttime），取自**同一次** stat 讀取，兩值必屬
/// 同一次行程生命。launcher 形（HOOK-OWNER-5）走訪中介時用：分兩次讀的話，
/// 中介可能在兩讀之間退出且 pid 被重用，父 pid 與 starttime 會各屬一個行程。
pub fn ppid_and_starttime(pid: &str) -> Option<(String, String)> {
    if !is_plain_pid(pid) {
        return None;
    }
    let fields = stat_fields_after_comm(pid)?;
    let ppid = fields.get(1)?;
    let st = fields.get(19)?;
    (!ppid.is_empty() && !st.is_empty()).then(|| (ppid.clone(), st.clone()))
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
        // 模擬 comm 含空白與右括號：欄位必須從最後一個 ')' 之後算起。
        // 測解析本體，不在測試裡另抄一份切法
        let raw = "42 (weird ) name) R 7 42 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 999888";
        let f = parse_stat_fields_after_comm(raw).expect("應解析得出欄位");
        assert_eq!(f[1], "7", "第 4 欄 ppid");
        assert_eq!(f[19], "999888", "第 22 欄 starttime");
        // 沒有右括號＝不是 stat 的形狀，不得硬解
        assert_eq!(parse_stat_fields_after_comm("42 weird R 7"), None);
    }

    #[test]
    fn traversal_and_empty_pids_are_refused() {
        for bad in ["", "..", "1/../2", "self", "-1", "1 2"] {
            assert!(!is_plain_pid(bad), "應拒絕 {bad:?}");
            assert_eq!(starttime(bad), None, "應拒絕 {bad:?}");
            assert_eq!(attest_runtime(bad, "x"), None, "應拒絕 {bad:?}");
            assert_eq!(ppid_and_starttime(bad), None, "應拒絕 {bad:?}");
        }
    }

    #[test]
    fn ppid_and_starttime_agree_with_single_field_reads() {
        let me = std::process::id().to_string();
        let (ppid, st) = ppid_and_starttime(&me).expect("自身 stat 應可讀");
        assert_eq!(Some(ppid), self_ppid());
        assert_eq!(Some(st), starttime(&me));
    }

    /// cmdline 判定的五種形狀。這條是 STATE-AGENT-4 的行為錨：規則一旦放寬回
    /// 「任一 argv 項」，第四項（同名參數）就會變綠。
    #[test]
    fn cmdline_shapes() {
        let argv = |items: &[&str]| items.join("\0").into_bytes();

        // ① 直接執行檔
        assert!(cmdline_is_runtime(&argv(&["/usr/bin/claude"]), "claude"));
        assert!(cmdline_is_runtime(&argv(&["claude", "--resume"]), "claude"));
        // ② 直譯器＋真 script 路徑
        assert!(cmdline_is_runtime(
            &argv(&["/bin/bash", "/opt/bin/claude", "--x"]),
            "claude"
        ));
        assert!(cmdline_is_runtime(&argv(&["env", "codex"]), "codex"));
        // ③ sh -c "…exec claude…"：argv[1] 是選項，不得命中
        assert!(!cmdline_is_runtime(
            &argv(&["sh", "-c", "exec claude --resume"]),
            "claude"
        ));
        // ④ 任意位置的同名參數：實測 `python -c 'sleep' codex` 會誤命中舊規則
        assert!(!cmdline_is_runtime(
            &argv(&["python", "-c", "sleep", "codex"]),
            "codex"
        ));
        assert!(!cmdline_is_runtime(
            &argv(&["tail", "-f", "/var/log/claude"]),
            "claude"
        ));
        // ⑤ 非 UTF-8：不 panic、不 lossy，位元組比對照樣分得出命中與否
        let mut bad = b"/opt/\xff/claude".to_vec();
        bad.push(0);
        assert!(cmdline_is_runtime(&bad, "claude"));
        assert!(!cmdline_is_runtime(&[0xff, 0xfe], "claude"));
        // 空 cmdline（kernel thread）與空名字永遠 false：否則「沒有 runtime
        // 名可比」會退化成無條件相符
        assert!(!cmdline_is_runtime(b"", "claude"));
        assert!(!cmdline_is_runtime(&argv(&["claude"]), ""));
    }

    #[test]
    fn attest_runtime_matches_own_binary() {
        let me = std::process::id().to_string();
        let raw = std::fs::read(format!("/proc/{me}/cmdline")).unwrap();
        let argv0 = raw.split(|b| *b == 0).next().unwrap();
        let base = String::from_utf8(basename(argv0).to_vec()).unwrap();
        // 自身 argv[0] 是帶路徑的測試執行檔，取 basename 後應命中，且回的
        // starttime 就是自己的
        assert_eq!(attest_runtime(&me, &base), starttime(&me));
        assert!(attest_runtime(&me, &base).is_some());
        assert_eq!(attest_runtime(&me, ""), None);
        assert_eq!(attest_runtime(&me, "__surely_not_an_argv__"), None);
    }
}

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// atomic_write:172 — 同目錄暫存檔 + rename，保證讀者任何時點都看不到半寫檔案
/// （state.md STATE-GEN-2）。payload 以 `&[u8]` 原樣搬運，不經 `String`／lossy
/// 轉換（架構 §3：byte 流只在 fsio 邊界，request/response 保真靠這個）。
pub fn atomic_write(dest: &Path, content: &[u8]) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| Error::new(format!("無效的目的路徑：{}", dest.display())))?;
    let (mut f, tmp) = create_unique_tmp(dir)?;
    // 暫存檔一旦建立成功，之後**每一條**失敗路徑都要清掉它，否則資料目錄會累積
    // 孤兒 .tmp.* 檔（ENOSPC／quota 下寫入反覆失敗時尤其明顯）。
    if let Err(e) = f.write_all(content).and_then(|()| f.flush()) {
        let _ = fs::remove_file(&tmp);
        return Err(Error::new(format!("寫入暫存檔失敗 {}：{e}", tmp.display())));
    }
    drop(f);
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::new(format!("無法將暫存檔改名為 {}：{e}", dest.display()))
    })?;
    Ok(())
}

/// 對應 bash `mktemp "$(dirname dest)/.tmp.XXXXXX"`：同目錄、不可預測檔名、
/// **原子建檔**。`create_new` 底層是 `O_CREAT|O_EXCL`，既不 follow symlink 也不
/// truncate 既有檔——`mktemp` 的保證在這個形狀下才成立。先 `exists()` 再
/// `File::create` 是 check-then-create：兩步之間的窗口讓 `File::create` 可能跟著
/// 符號連結走出資料目錄、或截斷一個剛出現的檔案。
/// 不引入 tempfile crate（架構 §1 std-only）；以 pid+奈秒+重試序號組合，
/// 對單一行程內的連續呼叫足夠唯一。
fn create_unique_tmp(dir: &Path) -> Result<(File, PathBuf)> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id();
    for attempt in 0..100u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = dir.join(format!(".tmp.{pid}.{nanos}.{attempt}"));
        match create_exclusive(&candidate) {
            Ok(f) => return Ok((f, candidate)),
            // 撞名才換下一個候選名；其餘錯誤（權限、目錄不存在、唯讀 fs）立即
            // 返回並保留 cause——靠重試把真正的失敗磨成「產不出唯一檔名」，等於
            // 把使用者帶去錯的方向查。
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(Error::new(format!(
                    "無法建立暫存檔 {}：{e}",
                    candidate.display()
                )));
            }
        }
    }
    Err(Error::new(format!(
        "無法產生唯一暫存檔名於 {}",
        dir.display()
    )))
}

/// `O_CREAT|O_EXCL` 建檔。抽成獨立函式是為了讓「不 truncate、不 follow
/// symlink」這兩條**可被測試直接指名一個路徑來驗**——候選名含奈秒，測試無法
/// 從外面預測 `create_unique_tmp` 會挑哪一個。
fn create_exclusive(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_back_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ab-core-fsio-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.txt");
        atomic_write(&dest, b"hello\n").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello\n");
        // 同檔案第二次寫入應覆蓋，且不留暫存檔
        atomic_write(&dest, b"world\n").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"world\n");
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "暫存檔未清乾淨：{leftover:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ab-core-fsio-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 候選路徑已被佔用時 MUST 建檔失敗（換下一個候選名），**且既有檔內容
    /// 不得被 truncate**。這是本次修正的主錨：舊寫法是 `exists()` 檢查後才
    /// `File::create`，那條路徑會把撞上的檔案截成 0 byte。
    #[test]
    fn exclusive_create_refuses_existing_file_without_truncating() {
        let dir = scratch("excl");
        let occupied = dir.join(".tmp.occupied");
        fs::write(&occupied, b"pre-existing payload").unwrap();

        let err = create_exclusive(&occupied).expect_err("撞名時 MUST NOT 建檔成功");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&occupied).unwrap(),
            b"pre-existing payload",
            "既有檔被 truncate 了"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 候選路徑是指向別處的 symlink 時 MUST NOT 跟著連結走：目標檔案既不被
    /// 寫入也不被 truncate（`O_EXCL` 對 symlink 一律失敗，不看目標存不存在）。
    #[cfg(unix)]
    #[test]
    fn exclusive_create_refuses_symlink_and_leaves_target_untouched() {
        let dir = scratch("symlink");
        let victim = dir.join("victim.txt");
        fs::write(&victim, b"victim payload").unwrap();
        let link = dir.join(".tmp.link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        create_exclusive(&link).expect_err("symlink 候選 MUST NOT 建檔成功");
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"victim payload",
            "symlink 目標被寫入或 truncate 了"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// rename 失敗（目的地是一個目錄）時 MUST 清掉暫存檔——失敗路徑不留孤兒
    /// `.tmp.*`，否則 ENOSPC／quota 反覆失敗會把資料目錄塞滿。
    #[test]
    fn failed_rename_leaves_no_tmp_file() {
        let dir = scratch("rename-fail");
        let dest = dir.join("dest-is-a-dir");
        fs::create_dir(&dest).unwrap();

        atomic_write(&dest, b"payload").expect_err("rename 到目錄 MUST 失敗");

        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "失敗路徑殘留暫存檔：{leftover:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}

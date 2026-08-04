use std::fs::{self, File};
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
    let tmp = unique_tmp_path(dir)?;
    {
        let mut f = File::create(&tmp)
            .map_err(|e| Error::new(format!("無法建立暫存檔 {}：{e}", tmp.display())))?;
        f.write_all(content)
            .map_err(|e| Error::new(format!("寫入暫存檔失敗 {}：{e}", tmp.display())))?;
    }
    fs::rename(&tmp, dest).map_err(|e| {
        // rename 失敗要清掉殘留暫存檔，否則資料目錄會累積孤兒 .tmp.* 檔
        let _ = fs::remove_file(&tmp);
        Error::new(format!("無法將暫存檔改名為 {}：{e}", dest.display()))
    })?;
    Ok(())
}

/// 對應 bash `mktemp "$(dirname dest)/.tmp.XXXXXX"`：同目錄、不可預測檔名。
/// 不引入 tempfile crate（架構 §1 std-only）；以 pid+奈秒+重試序號組合，
/// 對單一行程內的連續呼叫足夠唯一。
fn unique_tmp_path(dir: &Path) -> Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id();
    for attempt in 0..100u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = dir.join(format!(".tmp.{pid}.{nanos}.{attempt}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::new(format!(
        "無法產生唯一暫存檔名於 {}",
        dir.display()
    )))
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
}

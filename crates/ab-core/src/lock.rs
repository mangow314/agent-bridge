use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::paths::Paths;

/// mkdir 鎖的 RAII guard。**正確性論證只允許引用顯式 `release()` 路徑**——
/// SIGKILL／abort 時 Drop 不執行，殘鎖＝bash 現況等價（架構 §6 鎖語意紅線）；
/// Drop 只是「忘了呼叫 release() 時的便利網」，用於一般控制流（含 `?` 提早
/// return）的正常 unwind。
#[derive(Debug)]
pub struct LockGuard {
    dir: Option<PathBuf>,
}

impl LockGuard {
    /// 顯式釋放；比照 bash `release_lock`:217——rmdir 失敗（非空／權限）只
    /// 警告到 stderr，不視為呼叫端錯誤（STATE-LOCK-2：殘留鎖會擋住後續操作，
    /// 這個警告訊息本身就是唯一的救援線索，不可吞掉）。
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(dir) = self.dir.take()
            && dir.is_dir()
            && let Err(e) = std::fs::remove_dir(&dir)
        {
            eprintln!(
                "agent-bridge: 警告：無法釋放鎖目錄 {}（非空或權限問題），它會擋住後續操作，請手動移除：{e}",
                dir.display()
            );
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// acquire_lock:227 — mkdir 鎖：佔用短暫重試（25 次、每次 0.2s）；建鎖失敗但
/// 鎖目錄不存在（權限／sandbox 問題）MUST 立即以真實原因終止，MUST NOT
/// 誤報為「鎖佔用中」（state.md STATE-LOCK-1）。
///
/// 非佔用分支比照 bash:234-238 補第二次 mkdir 嘗試：初次失敗且鎖目錄不存在
/// 時，瞬時性錯誤（例如短暫的檔案系統壓力）可能已在兩次呼叫之間消失——
/// 只試一次就 die 對「瞬時錯誤已解除」的情形不是終態等價（會把 bash 版能
/// 成功取鎖的呼叫誤判為失敗）。第二次仍失敗才以其錯誤原因終止。
pub fn acquire_lock(paths: &Paths, id: &str) -> Result<LockGuard> {
    let lock = paths.locks_dir.join(format!("{id}.lock"));
    let mut tries = 0u32;
    loop {
        match std::fs::create_dir(&lock) {
            Ok(()) => break,
            Err(_) if lock.is_dir() => {
                // 佔用中（第一次 mkdir 失敗且鎖目錄確實存在）：有限重試
                tries += 1;
                if tries >= 25 {
                    return Err(Error::new(format!(
                        "無法取得鎖（locks/{id}.lock 佔用中或殘留），請稍後重試"
                    )));
                }
                sleep(Duration::from_millis(200));
                continue;
            }
            Err(_) => {
                // 非佔用：鎖目錄不存在卻建立失敗，比照 bash 再嘗試一次
                match std::fs::create_dir(&lock) {
                    Ok(()) => break,
                    Err(e2) => {
                        return Err(Error::new(format!(
                            "無法建立鎖目錄（非鎖佔用，疑似權限或 sandbox 寫入限制）：{e2}"
                        )));
                    }
                }
            }
        }
    }
    Ok(LockGuard { dir: Some(lock) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> (tempdir_guard::Dir, Paths) {
        let dir = tempdir_guard::Dir::new("ab-core-lock-test");
        let paths = Paths {
            data_dir: dir.path.clone(),
            agents_dir: dir.path.join("agents"),
            tasks_dir: dir.path.join("tasks"),
            locks_dir: dir.path.join("locks"),
            state_dir: dir.path.join("state"),
        };
        std::fs::create_dir_all(&paths.locks_dir).unwrap();
        (dir, paths)
    }

    #[test]
    fn acquire_then_release_allows_reacquire() {
        let (_dir, paths) = test_paths();
        let guard = acquire_lock(&paths, "agents-registry").unwrap();
        assert!(paths.locks_dir.join("agents-registry.lock").is_dir());
        guard.release();
        assert!(!paths.locks_dir.join("agents-registry.lock").is_dir());
        // 釋放後應能重新取得
        let guard2 = acquire_lock(&paths, "agents-registry").unwrap();
        guard2.release();
    }

    #[test]
    fn occupied_lock_times_out() {
        let (_dir, paths) = test_paths();
        let _held = acquire_lock(&paths, "busy").unwrap();
        let err = acquire_lock(&paths, "busy").unwrap_err();
        assert!(err.message.contains("無法取得鎖"));
    }

    /// 非佔用分支：鎖路徑被一個普通檔案（非目錄）擋住時，兩次 mkdir 嘗試
    /// 都會失敗（阻擋物持續存在），錯誤訊息 MUST 落在「非鎖佔用」分支而非
    /// 「鎖佔用中」分支——快速失敗、不進 25 次重試迴圈。這間接驗證了
    /// bash:234-238 的雙嘗試結構有被複製（若只驗證單次嘗試也會通過本測試，
    /// 但訊息分流正確性是本次修復要保的不變量）。
    #[test]
    fn non_directory_blocker_reports_real_error_not_occupied() {
        let (_dir, paths) = test_paths();
        let lock_path = paths.locks_dir.join("blocked.lock");
        std::fs::write(&lock_path, b"not a directory").unwrap();
        let err = acquire_lock(&paths, "blocked").unwrap_err();
        assert!(
            err.message.contains("無法建立鎖目錄"),
            "應落在非佔用分支，實際訊息：{}",
            err.message
        );
        assert!(!err.message.contains("佔用中"));
    }

    /// 極簡暫存目錄 helper：不額外引入 tempfile crate（架構 §1 std-only）。
    mod tempdir_guard {
        use std::path::PathBuf;

        pub struct Dir {
            pub path: PathBuf,
        }

        impl Dir {
            pub fn new(prefix: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "{prefix}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir { path }
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}

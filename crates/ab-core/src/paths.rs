use std::env;
use std::path::PathBuf;

use crate::error::{Error, Result};

/// 資料目錄佈局（`agents/` `tasks/` `state/` `locks/`）與 `AGENT_BRIDGE_DATA`
/// 解析（ENV-DATA-1）。對映 bash：全域初始化（DATA_DIR）與 `ensure_dirs`:148。
pub struct Paths {
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub locks_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    /// ENV-DATA-1：`AGENT_BRIDGE_DATA` 未設定或空字串時取預設
    /// `~/.local/share/agent-bridge`（比照 bash `${AGENT_BRIDGE_DATA:-...}`，
    /// `:-` 對空字串同樣觸發預設值）。
    pub fn resolve() -> Self {
        let data_dir = match env::var(crate::config::ENV_DATA) {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => {
                let home = env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".local/share/agent-bridge")
            }
        };
        Paths {
            agents_dir: data_dir.join("agents"),
            tasks_dir: data_dir.join("tasks"),
            locks_dir: data_dir.join("locks"),
            state_dir: data_dir.join("state"),
            data_dir,
        }
    }

    /// ensure_dirs:148 — 非唯讀指令進場時補建四個子目錄；唯讀指令
    /// （status/await/idle/list/hook）MUST NOT 呼叫本函式（CLI-RO-1，
    /// 由 `ab` dispatch 層的豁免表保證，不在此處判斷）。
    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.agents_dir,
            &self.tasks_dir,
            &self.locks_dir,
            &self.state_dir,
        ] {
            std::fs::create_dir_all(d)
                .map_err(|e| Error::new(format!("無法建立資料目錄 {}：{e}", d.display())))?;
        }
        Ok(())
    }
}

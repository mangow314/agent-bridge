//! 呼叫者綁定（CALLER-BIND）：從呼叫者自己的 `AGENT_BRIDGE_SPAWN_TAG` 析出
//! `caller_name`，與子指令的目標 agent 比對。
//!
//! 三態：tag 合法且同名 → 放行；tag 合法且異名 → 拒絕並寫 `agents.log`；
//! tag 未設／空字串／不符文法 → 放行（人工 pane、orchestrator 本尊）。
//! tag 解析重用 `hook::agent_name`，不另寫一份。

use crate::error::{Error, Result};
use crate::hook;
use crate::paths::Paths;
use crate::registry;
use crate::time::now_iso;

/// 以環境中的 tag 解出的 caller 為準檢查 `target`。`task` 為 `None` 時審計寫 `-`。
pub fn check_caller_bound(
    paths: &Paths,
    cmd: &str,
    target: &str,
    task: Option<&str>,
) -> Result<()> {
    let Some(caller) = hook::agent_name() else {
        return Ok(());
    };
    check_named(paths, &caller, cmd, target, task)
}

fn check_named(
    paths: &Paths,
    caller: &str,
    cmd: &str,
    target: &str,
    task: Option<&str>,
) -> Result<()> {
    if caller == target {
        return Ok(());
    }
    let task_s = task.unwrap_or("-");
    let line = format!(
        "{} refused-identity cmd={} caller={} target={} task={}\n",
        now_iso(),
        registry::squash(cmd),
        registry::squash(caller),
        registry::squash(target),
        registry::squash(task_s),
    );
    // 審計寫入失敗不翻盤：拒絕本身就是結果，呼叫端仍收到非零退出。
    let _ = registry::append_line(&paths.data_dir.join("agents.log"), &line);
    Err(Error::new(format!(
        "拒絕：呼叫者 '{caller}' 不得代 '{target}' 執行 {cmd}（task {task_s}）"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(tag: &str) -> (Paths, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("ab-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut p = Paths::resolve();
        p.data_dir = d.clone();
        (p, d)
    }

    #[test]
    fn same_name_passes_and_other_name_is_logged() {
        let (p, d) = paths_in("a");
        assert!(check_named(&p, "w1", "reply", "w1", Some("t1")).is_ok());
        assert!(!d.join("agents.log").exists());
        let e = check_named(&p, "w1", "reply", "w2", Some("t1")).unwrap_err();
        assert!(e.to_string().contains("w1") && e.to_string().contains("w2"));
        let log = std::fs::read_to_string(d.join("agents.log")).unwrap();
        assert!(log.contains("refused-identity cmd=reply caller=w1 target=w2 task=t1"));
        let _ = std::fs::remove_dir_all(&d);
    }
}

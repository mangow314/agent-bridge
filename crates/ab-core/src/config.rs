//! 環境變數的集中讀取點（spec/env.md）。
//!
//! 兩件事在這裡收斂：
//!
//! 1. **名稱字面字串**：每個 `AGENT_BRIDGE_*` 的名字以 `const` 出現一次，
//!    其餘模組引用常數而非自行拼字串。`tests/check-contract.sh` check 1 是
//!    對 `bin/agent-bridge` 與 `spec/env.md` 做集合比對（不掃 Rust），但同一
//!    份名單在 Rust 側也必須以**字面字串**存在才 grep 得到——M4 cutover 後
//!    正本換人時那道檢查要能原樣沿用。
//! 2. **同一個變數的兩套失效方向**：`AGENT_BRIDGE_STATE_TTL` 在通知端壞值
//!    MUST 致命（ENV-TTL-1/2），在 hook 端壞值 MUST NOT 致命（hook 鐵律：
//!    任何內部錯誤一律 exit 0）。兩者不是同一個函式加旗標能講清楚的事，
//!    拆成 `state_ttl_strict` 與 `state_ttl_lenient` 兩個名字，呼叫端選錯
//!    的機會就小。

use crate::error::{Error, Result};

/// 資料目錄根（`paths::Paths::resolve`）。
pub const ENV_DATA: &str = "AGENT_BRIDGE_DATA";
/// `await` 輪詢間隔秒數（CLI 層自行解析，錯誤文案屬 CLI）。
pub const ENV_POLL_INTERVAL: &str = "AGENT_BRIDGE_POLL_INTERVAL";
/// worker 活動狀態新鮮度秒數。
pub const ENV_STATE_TTL: &str = "AGENT_BRIDGE_STATE_TTL";
/// 兩次送鍵之間的間隔秒數。
pub const ENV_NOTIFY_DELAY: &str = "AGENT_BRIDGE_NOTIFY_DELAY";
/// hook 端據以析出「我是誰」的 spawn tag。
pub const ENV_SPAWN_TAG: &str = "AGENT_BRIDGE_SPAWN_TAG";

/// 同時存活的 spawned worker 上限。
pub const ENV_MAX_SPAWN: &str = "AGENT_BRIDGE_MAX_SPAWN";
/// spawn 後等待 worker 自報就緒的秒數（0＝不等待）。
pub const ENV_READY_TIMEOUT: &str = "AGENT_BRIDGE_READY_TIMEOUT";
/// 就緒探針的重送間隔秒數。
pub const ENV_READY_PROBE_INTERVAL: &str = "AGENT_BRIDGE_READY_PROBE_INTERVAL";
/// worker 守則檔路徑。
pub const ENV_WORKER_BRIEF: &str = "AGENT_BRIDGE_WORKER_BRIEF";
/// 接手者守則檔路徑（relay 用）。
pub const ENV_SUCCESSOR_BRIEF: &str = "AGENT_BRIDGE_SUCCESSOR_BRIEF";
/// claude worker 的 hooks settings 檔路徑。
pub const ENV_CLAUDE_HOOKS: &str = "AGENT_BRIDGE_CLAUDE_HOOKS";
/// 額外要穿透進 worker pane 的環境變數名（逗號分隔）。
pub const ENV_PASS_ENV: &str = "AGENT_BRIDGE_PASS_ENV";
/// 接力鏈深度（relay 在鏈上逐棒下傳）。
pub const ENV_RELAY_DEPTH: &str = "AGENT_BRIDGE_RELAY_DEPTH";
/// 接力鏈深度上限（0＝解除限制）。
pub const ENV_MAX_RELAY_DEPTH: &str = "AGENT_BRIDGE_MAX_RELAY_DEPTH";
/// 單次 tmux 送鍵子行程的逾時秒數（0＝不設限）。
pub const ENV_TMUX_TIMEOUT: &str = "AGENT_BRIDGE_TMUX_TIMEOUT";
/// Page 層推播的自訂命令（argv[0]，後接 title、body 兩個參數）。設了就取代
/// 桌面通知那一層——SSH／無桌面環境的逃生口（ntfy、telegram…）。
pub const ENV_NOTIFY_CMD: &str = "AGENT_BRIDGE_NOTIFY_CMD";
/// here 落點 split 後套用的 tmux layout（`none`＝只 split 不重排）。
pub const ENV_HERE_LAYOUT: &str = "AGENT_BRIDGE_HERE_LAYOUT";

/// state TTL 的預設值（bash `${AGENT_BRIDGE_STATE_TTL:-1800}`）。
pub const STATE_TTL_DEFAULT: i64 = 1800;

/// 環境變數的三態讀取：未設定／設定為合法 UTF-8／**設定但非 UTF-8**。
///
/// `env::var().unwrap_or_default()` 會把第三態壓成第二態的空字串，於是
/// 「設定成一串亂碼」被當成「沒設定」而退預設——bash 沒有這個概念，它拿到
/// 的是原始位元組、regex 判不過就 die（codex 複核 2026-07-31 的 finding）。
/// 這裡把三態留著，由呼叫端各自決定方向。
enum EnvValue {
    Unset,
    Text(String),
    NonUnicode,
}

fn read_env(name: &str) -> EnvValue {
    classify(std::env::var_os(name))
}

/// 分類邏輯與讀取分開，是為了讓它測得到：直接測 `read_env` 得動行程層級的
/// 環境變數，會與並行的其他測試互踩。
fn classify(v: Option<std::ffi::OsString>) -> EnvValue {
    match v {
        None => EnvValue::Unset,
        Some(os) => match os.into_string() {
            // bash `:-` 對空字串同樣觸發預設
            Ok(s) if s.is_empty() => EnvValue::Unset,
            Ok(s) => EnvValue::Text(s),
            Err(_) => EnvValue::NonUnicode,
        },
    }
}

/// bash `[[ "$ttl" =~ ^[0-9]{1,9}$ ]]`：非負整數、至多 9 位。
fn parse_ttl(raw: &str) -> Option<i64> {
    if raw.is_empty() || raw.len() > 9 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // 前導零比照 bash `10#$ttl` 強制十進位
    raw.parse::<i64>().ok()
}

/// ENV-TTL-1/2（通知端）：只認非負整數，其餘視為設定錯誤直接 die——
/// **壞值 MUST 致命**，不得靜默退回預設。0 是合法值（＝關掉整條 state 通道）。
pub fn state_ttl_strict() -> Result<i64> {
    let raw = match read_env(ENV_STATE_TTL) {
        EnvValue::Unset => return Ok(STATE_TTL_DEFAULT),
        EnvValue::Text(v) => v,
        // 非 UTF-8 也是「設定了一個判不過 regex 的值」，同樣致命。訊息裡的值
        // 以 lossy 呈現——die 文案的目的是讓人看見自己設了什麼
        EnvValue::NonUnicode => {
            let lossy = std::env::var_os(ENV_STATE_TTL)
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Err(Error::new(format!("{ENV_STATE_TTL} 需為非負整數：{lossy}")));
        }
    };
    parse_ttl(&raw).ok_or_else(|| Error::new(format!("{ENV_STATE_TTL} 需為非負整數：{raw}")))
}

/// hook_owner_gate 用（bash:2074）：壞值**或 0** 一律退預設、不得 die。
///
/// 0 在這裡不能照收：TTL=0 時通知端通道雖關，stop 的 block 輸出仍活著（冒名
/// 攻擊面仍在），gate 必須繼續用預設窗運作——「永遠可接管」等於沒有 gate。
pub fn state_ttl_lenient() -> i64 {
    let EnvValue::Text(raw) = read_env(ENV_STATE_TTL) else {
        // 未設定與非 UTF-8 都退預設（hook 端不得 die）
        return STATE_TTL_DEFAULT;
    };
    match parse_ttl(&raw) {
        Some(v) if v > 0 => v,
        _ => STATE_TTL_DEFAULT,
    }
}

/// `sleep "${AGENT_BRIDGE_NOTIFY_DELAY:-0.3}"` 的秒數。
///
/// bash 未驗證這個值的格式，壞值讓 `sleep` 立刻失敗；而 `notify_pane` 是被
/// `if` 包住呼叫的（errexit 在該語境抑制），失敗後**繼續往下執行**。此處
/// 對齊該終態：解析不出正數就回 `None`（呼叫端不睡，直接往下走）。
pub fn notify_delay_secs() -> Option<f64> {
    let raw = match read_env(ENV_NOTIFY_DELAY) {
        EnvValue::Unset => return Some(0.3),
        EnvValue::Text(v) => v,
        // 非 UTF-8 之於 `sleep` 就是一個壞參數，同壞值路徑：不睡、往下走
        EnvValue::NonUnicode => return None,
    };
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Some(v),
        _ => None,
    }
}

/// `AGENT_BRIDGE_TMUX_TIMEOUT`：單次 tmux 送鍵子行程的逾時秒數，預設 `5`。
/// `Some(d)`＝設此上限，`None`＝不設限。
///
/// **壞值退預設、不 die**：這是防止整個指令被鎖死的安全網（AB-COPYMODE-1），
/// 拼錯一個環境變數不該把安全網整個拆掉——與 `state_ttl_lenient` 同方向。
/// `0` 則是顯式的逃生口（回到加逾時之前的無限等待），照收。
///
/// 上限只需涵蓋「tmux 正常回應」的量級（毫秒級），預設 5 秒已極寬鬆；真正
/// 的 copy-mode 卡死是永不返回，多寬的窗都會撞到。
pub fn tmux_timeout() -> Option<std::time::Duration> {
    match read_env(ENV_TMUX_TIMEOUT) {
        EnvValue::Text(raw) => parse_tmux_timeout(Some(&raw)),
        // 未設定與非 UTF-8 都退預設（安全網不因壞值消失）
        _ => parse_tmux_timeout(None),
    }
}

/// `tmux_timeout` 的純判定核心（環境變數已讀成 `Option<&str>`）。
///
/// 抽出來是為了可測：直接測 `tmux_timeout()` 得動 process 全域的環境變數，
/// 而測試是平行跑的——那種測試會做出隨機紅。
///
/// 上限 `MAX_SECS`（一天）不是「合理的等待時間」而是溢位護欄：
/// `Instant::now() + Duration::from_secs(u64::MAX)` 會 panic，而 panic 的位置
/// 在任務已建立之後的通知階段（跨廠複核 2026-07-31 finding 2）。夾住而非退
/// 預設，是因為使用者寫一個超大值的意圖顯然是「幾乎別逾時」，夾到一天完整
/// 保留那個意圖；真的想要不設限有 `0` 這個顯式逃生口。
fn parse_tmux_timeout(raw: Option<&str>) -> Option<std::time::Duration> {
    const DEFAULT_SECS: u64 = 5;
    const MAX_SECS: u64 = 86_400;
    let secs = match raw {
        Some(s) => s.trim().parse::<u64>().unwrap_or(DEFAULT_SECS),
        None => DEFAULT_SECS,
    };
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs.min(MAX_SECS)))
    }
}

/// `L` 尾行預覽的**三重界**（tui-design §9 P4.7：one-shot、行／byte／時間
/// 三重有界，且 bounded MUST 成立於**資料取得路徑**——先全讀進記憶體再截
/// 不算）。三個值只有這一份定義，取得路徑（`tmux::capture_pane_tail`）與
/// UI 都讀它，不在別處各截一次。
///
/// 為什麼是這三個數字：
/// - `TAIL_HISTORY_LINES = 200`：這是**預覽**不是 scrollback dump。200 行約等於
///   兩三個畫面高，足以看出「這個 worker 最後在做什麼」。
///
///   ⚠️ 名字說的就是它的實情：這是傳給 `capture-pane -S -<n>` 的**向後要求的
///   history 行數**，不是「總共只取 n 行」。tmux 回的是「n 行 history ＋整個
///   可見區」——實測 80x24 的 pane 印 500 行後 `-S -200` 回 224 行（＝200＋24）。
///   它的作用是讓 tmux 那一側一開始就不撈整份歷史；**總量的硬上限是下面的
///   byte 界**，那一條才是資源保證。
/// - `TAIL_MAX_BYTES = 64 KiB`：約 200 行 × 320 欄的寬鬆估計，並涵蓋上面那個
///   「多回一個畫面高」的落差。單行 10MB 的 pane（例如 `cat` 了一個 minified
///   bundle）會在讀取迴圈裡當場被截斷，不會整份進記憶體。
/// - `TAIL_TIMEOUT = 2s`：與 liveness 輪詢同一個量級（`LIVE_POLL`）。這條路徑
///   是一次性 thread，逾時只影響那一次預覽；**刻意不吃**
///   `AGENT_BRIDGE_TMUX_TIMEOUT=0` 的無限逃生口——契約要求時間界成立於取得
///   路徑本身，一個可以被環境變數關掉的界不是界。
pub const TAIL_HISTORY_LINES: usize = 200;
/// 見 `TAIL_HISTORY_LINES`。
pub const TAIL_MAX_BYTES: usize = 64 * 1024;
/// 見 `TAIL_HISTORY_LINES`。
pub const TAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// bash `REPO_ROOT="$(dirname "$(dirname "$SCRIPT_PATH")")"`（bin/agent-bridge:45-46），
/// 其中 `SCRIPT_PATH` 是 `readlink -f` 後的實體路徑。Rust 這邊
/// `current_exe()` 在 Linux 讀 `/proc/self/exe`，同樣是解析完符號連結的實體
/// 路徑——測試把 `$SHIM/agent-bridge` symlink 到執行檔，兩邊都會解析到本尊。
///
/// 因此 Rust 執行檔的擺放位置決定預設 brief 找不找得到：必須是
/// `<root>/<某層>/<執行檔>`，讓 `<root>/share/` 是它的祖父層兄弟目錄
/// （測試 22f／23a 也是用 `dirname(dirname($BRIDGE))/share` 反推正本位置）。
fn repo_root() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
}

/// `${VAR:-<repo_root>/<rel>}`：未設定或空字串取預設（ENV-BRIEF-1/2）。
fn path_env_or(name: &str, rel: &str) -> std::path::PathBuf {
    match read_env(name) {
        EnvValue::Text(v) => std::path::PathBuf::from(v),
        // 非 UTF-8 路徑照原樣用：bash 拿到的也是原始位元組
        EnvValue::NonUnicode => std::env::var_os(name)
            .map(std::path::PathBuf::from)
            .unwrap_or_default(),
        EnvValue::Unset => repo_root().join(rel),
    }
}

/// worker 守則檔（ENV-BRIEF-1）。
pub fn worker_brief() -> std::path::PathBuf {
    path_env_or(ENV_WORKER_BRIEF, "share/worker-brief.md")
}

/// 接手者守則檔（ENV-BRIEF-2）。
pub fn successor_brief() -> std::path::PathBuf {
    path_env_or(ENV_SUCCESSOR_BRIEF, "share/successor-brief.md")
}

/// claude worker 的 hooks settings。
pub fn claude_hooks_settings() -> std::path::PathBuf {
    path_env_or(ENV_CLAUDE_HOOKS, "share/claude-worker-hooks.json")
}

/// bash `[[ "$max" =~ ^[0-9]{1,9}$ ]]`（cmd_spawn:1180）：壞值致命。
pub fn max_spawn() -> Result<i64> {
    match read_env(ENV_MAX_SPAWN) {
        EnvValue::Unset => Ok(4),
        EnvValue::Text(raw) => parse_ttl(&raw)
            .ok_or_else(|| Error::new(format!("{ENV_MAX_SPAWN} 需為非負整數：{raw}"))),
        EnvValue::NonUnicode => Err(Error::new(format!(
            "{ENV_MAX_SPAWN} 需為非負整數：{}",
            lossy(ENV_MAX_SPAWN)
        ))),
    }
}

/// `validate_ready_opts`:988 — 兩個 readiness 參數**必須在建 pane 之前**驗完；
/// 回傳 `(timeout_secs, probe_interval_secs)`。
pub fn ready_opts() -> Result<(u64, f64)> {
    let t_raw = match read_env(ENV_READY_TIMEOUT) {
        EnvValue::Unset => String::from("30"),
        EnvValue::Text(v) => v,
        EnvValue::NonUnicode => lossy(ENV_READY_TIMEOUT),
    };
    let timeout = parse_ttl(&t_raw).ok_or_else(|| {
        Error::new(format!(
            "{ENV_READY_TIMEOUT} 需為非負整數（秒，至多 9 位；0＝不等待）：{t_raw}"
        ))
    })? as u64;

    let i_raw = match read_env(ENV_READY_PROBE_INTERVAL) {
        EnvValue::Unset => String::from("2"),
        EnvValue::Text(v) => v,
        EnvValue::NonUnicode => lossy(ENV_READY_PROBE_INTERVAL),
    };
    if !decimal_shape_ok(&i_raw) {
        return Err(Error::new(format!(
            "{ENV_READY_PROBE_INTERVAL} 需為正數（秒）：{i_raw}"
        )));
    }
    // bash `[[ ! "$i" =~ ^0*(\.0+)?$ ]]`：小數點前後都只有 0（或省略整數位）
    // 即零值。`.0`／`00` 這些形狀也要擋，否則探針成忙迴圈
    let interval: f64 = i_raw.parse().unwrap_or(0.0);
    if interval == 0.0 {
        return Err(Error::new(format!(
            "{ENV_READY_PROBE_INTERVAL} 需大於 0（否則探針會忙迴圈）：{i_raw}"
        )));
    }
    Ok((timeout, interval))
}

/// bash `^([0-9]+|[0-9]*\.[0-9]+)$`（小數點後至少一位；`1.` 不合法）。
fn decimal_shape_ok(raw: &str) -> bool {
    match raw.split_once('.') {
        None => !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()),
        Some((int_part, frac)) => {
            int_part.bytes().all(|b| b.is_ascii_digit())
                && !frac.is_empty()
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

/// `AGENT_BRIDGE_PASS_ENV`（cmd_spawn:1238-1246）：逗號分隔的變數名清單，
/// 逐個驗 `^[A-Za-z_][A-Za-z0-9_]*$`，空段落跳過；不合法即 die。
pub fn pass_env_names() -> Result<Vec<String>> {
    let raw = match read_env(ENV_PASS_ENV) {
        EnvValue::Unset => return Ok(Vec::new()),
        EnvValue::Text(v) => v,
        EnvValue::NonUnicode => lossy(ENV_PASS_ENV),
    };
    let mut out = Vec::new();
    for part in raw.split(',') {
        if part.is_empty() {
            continue;
        }
        let mut bytes = part.bytes();
        let head_ok = matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_');
        if !head_ok || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(Error::new(format!(
                "{ENV_PASS_ENV} 含不合法的變數名：{part}"
            )));
        }
        out.push(part.to_string());
    }
    Ok(out)
}

/// `AGENT_BRIDGE_PASS_ENV` **不得穿透**的保留變數（P4.7 切片 A／B4）。
///
/// 這兩個變數是 spawn 自己拼進啟動指令的：`SPAWN_TAG` 是子代的世代身分
/// （despawn／回滾／lineage 全靠它），`RELAY_DEPTH` 是接力鏈的棒次。白名單
/// 若放它們過去，assignment 會排在 spawn 自己那一個**之後**——同一條命令列
/// 上後項覆蓋前項，子代於是頂著**呼叫者的** tag 開起來：despawn 會殺錯世代、
/// lineage 會把自己認成自己的 parent、relay 深度會被重置。
///
/// 處置是**靜默剔除＋stderr 警告**，不使 spawn 失敗：呼叫端多半是把整份環境
/// 名單抄過來，讓 spawn 硬失敗只會逼人拆名單，而剔除之後的行為正是他要的。
pub const RESERVED_PASS_ENV: [&str; 2] = [ENV_SPAWN_TAG, ENV_RELAY_DEPTH];

pub fn is_reserved_pass_env(name: &str) -> bool {
    RESERVED_PASS_ENV.contains(&name)
}

/// 接力鏈深度（cmd_relay:1438-1445）。bash 用 `${VAR-0}` 而非 `${VAR:-0}`：
/// **「已設但為空」不吃預設**，會落進格式檢查被拒——否則
/// `AGENT_BRIDGE_RELAY_DEPTH=''` 會靜默把鏈深度重置成 0，cap 形同虛設。
/// 故此處不能用 `read_env`（它把空字串歸進 `Unset`）。
pub fn relay_depth() -> Result<i64> {
    depth_var(
        ENV_RELAY_DEPTH,
        0,
        "需為非負整數（空值也不接受，避免靜默重置鏈深度）",
    )
}

/// 接力鏈深度上限（預設 10；0＝解除限制）。
pub fn max_relay_depth() -> Result<i64> {
    depth_var(ENV_MAX_RELAY_DEPTH, 10, "需為非負整數（空值也不接受）")
}

/// here 落點的 layout（預設 `main-vertical`）。白名單驗證、壞值致命：
/// spawn 端在建 pane 前呼叫（CLI-SPAWN-2 的 precheck 紀律），錯字不該
/// 等 pane 落地後才被 tmux 拒絕。**空字串也是壞值**——不走 `read_env` 的
/// `:-` 慣例，設了就必須是白名單值（codex plan 審查 2026-08-04 R4）。
pub fn here_layout() -> Result<String> {
    parse_here_layout(std::env::var_os(ENV_HERE_LAYOUT))
}

const HERE_LAYOUTS: [&str; 6] = [
    "main-vertical",
    "main-horizontal",
    "tiled",
    "even-vertical",
    "even-horizontal",
    "none",
];

/// 與讀取分開以便測試（same pattern as `classify`）。
fn parse_here_layout(v: Option<std::ffi::OsString>) -> Result<String> {
    let Some(os) = v else {
        return Ok("main-vertical".to_string());
    };
    let Ok(s) = os.into_string() else {
        return Err(Error::new(format!(
            "{ENV_HERE_LAYOUT} 含非 UTF-8 內容，拒絕使用"
        )));
    };
    if HERE_LAYOUTS.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(Error::new(format!(
            "{ENV_HERE_LAYOUT} 不是合法 layout（{}；空值也不接受）：{s}",
            HERE_LAYOUTS.join("|")
        )))
    }
}

/// 「人工 session」判準的**唯一正本**：spawn 落點的 auto 規則與
/// CLI-RELAY-4 盯守提醒都走這裡，兩路不得各自解讀（P4 審查 CONFIRMED 1）。
/// 三態語意：unset 與空字串＝人工（`:-` 慣例——空值視同沒設）；非 UTF-8
/// ＝視為 tag 在場（保守：寧可少印提醒、worker 不落人類 window，也不把
/// 亂碼當人工）。tag 是 placement provenance hint，不是身分授權。
pub fn caller_is_manual() -> bool {
    matches!(read_env(ENV_SPAWN_TAG), EnvValue::Unset)
}

fn depth_var(name: &str, default: i64, complaint: &str) -> Result<i64> {
    let Some(os) = std::env::var_os(name) else {
        return Ok(default);
    };
    let raw = os.to_string_lossy().into_owned();
    parse_ttl(&raw).ok_or_else(|| Error::new(format!("{name} {complaint}：{raw}")))
}

fn lossy(name: &str) -> String {
    std::env::var_os(name)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// here layout 的值域：unset＝預設 main-vertical；白名單值原樣通過；
    /// 空字串、白名單外、非 UTF-8 一律致命（fail-closed，不套 `:-` 慣例）。
    #[test]
    fn here_layout_value_domain() {
        use std::ffi::OsString;
        assert_eq!(parse_here_layout(None).unwrap(), "main-vertical");
        for good in HERE_LAYOUTS {
            assert_eq!(parse_here_layout(Some(OsString::from(good))).unwrap(), good);
        }
        for bad in ["", "sideways", "main-vertical ", "MAIN-VERTICAL"] {
            assert!(
                parse_here_layout(Some(OsString::from(bad))).is_err(),
                "壞值應致命：{bad:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert!(parse_here_layout(Some(OsString::from_vec(vec![0xff, 0xfe]))).is_err());
        }
    }

    /// ENV-TMUX-1 的值域：預設 5、`0`＝不設限、壞值退預設。
    ///
    /// 壞值必須退預設而非關掉上限：這是防止整個 `send` 被 tmux 鎖死的安全網
    /// （AB-COPYMODE-1），拼錯一個變數名不該把安全網拆掉。
    #[test]
    fn tmux_timeout_value_domain() {
        use std::time::Duration;
        assert_eq!(parse_tmux_timeout(None), Some(Duration::from_secs(5)));
        assert_eq!(parse_tmux_timeout(Some("2")), Some(Duration::from_secs(2)));
        assert_eq!(
            parse_tmux_timeout(Some(" 7 ")),
            Some(Duration::from_secs(7))
        );
        // 顯式逃生口：不設限
        assert_eq!(parse_tmux_timeout(Some("0")), None);
        // 壞值一律退預設，**不得**變成 None（那等於安全網被壞值拆掉）
        for bad in ["abc", "", "-1", "1.5", "5s"] {
            assert_eq!(
                parse_tmux_timeout(Some(bad)),
                Some(Duration::from_secs(5)),
                "壞值應退預設：{bad}"
            );
        }
    }

    /// 溢位護欄：合法但極大的 `u64` 曾讓 `Instant + Duration` 在通知階段
    /// panic（跨廠複核 2026-07-31 finding 2）。夾到上限，不是退預設也不是 panic。
    #[test]
    fn absurdly_large_timeout_is_clamped_not_fatal() {
        use std::time::Duration;
        let huge = parse_tmux_timeout(Some(&u64::MAX.to_string())).expect("不得變成不設限");
        assert_eq!(huge, Duration::from_secs(86_400));
        // 夾過的值必須加得出期限（原 panic 的算式）
        assert!(std::time::Instant::now().checked_add(huge).is_some());
    }

    /// `validate_ready_opts` 的零值判斷：`0`／`0.0`／`.0`／`00` 全是零，
    /// 必須被擋（bash `^0*(\.0+)?$`）。`1.` 則是形狀就不合法。
    #[test]
    fn probe_interval_shapes() {
        for good in ["1", "0.5", ".5", "2", "10.25"] {
            assert!(decimal_shape_ok(good), "形狀應合法：{good}");
        }
        for bad in ["1.", "", ".", "abc", "-1", "1e3"] {
            assert!(!decimal_shape_ok(bad), "形狀應不合法：{bad}");
        }
        for zero in ["0", "0.0", ".0", "00", "0.00"] {
            assert!(decimal_shape_ok(zero), "形狀合法但值為零：{zero}");
            assert_eq!(zero.parse::<f64>().unwrap(), 0.0);
        }
    }

    #[test]
    fn ttl_grammar_matches_bash_regex() {
        assert_eq!(parse_ttl("0"), Some(0));
        assert_eq!(parse_ttl("1800"), Some(1800));
        // 前導零走十進位而非八進位
        assert_eq!(parse_ttl("0900"), Some(900));
        assert_eq!(parse_ttl("999999999"), Some(999_999_999));
        // 10 位超出 {1,9}
        assert_eq!(parse_ttl("1000000000"), None);
        assert_eq!(parse_ttl("-1"), None);
        assert_eq!(parse_ttl("1.5"), None);
        assert_eq!(parse_ttl("abc"), None);
        assert_eq!(parse_ttl(""), None);
    }

    /// hook 端的 0 與壞值都退預設；strict 端的 0 是合法值。兩者方向相反，
    /// 這個測試就是那條分界線。
    #[test]
    fn lenient_and_strict_diverge_on_zero() {
        assert_eq!(parse_ttl("0"), Some(0));
        // lenient 的 0→預設是在 state_ttl_lenient 的 match 裡，不在 parse_ttl
        let lenient_of = |raw: &str| match parse_ttl(raw) {
            Some(v) if v > 0 => v,
            _ => STATE_TTL_DEFAULT,
        };
        assert_eq!(lenient_of("0"), STATE_TTL_DEFAULT);
        assert_eq!(lenient_of("bad"), STATE_TTL_DEFAULT);
        assert_eq!(lenient_of("60"), 60);
    }

    /// 「設定成非 UTF-8」不得被壓成「沒設定」：bash 拿到的是原始位元組、
    /// regex 判不過就 die，通知端必須跟著死（codex 複核 2026-07-31）。
    #[cfg(unix)]
    #[test]
    fn non_unicode_env_is_not_unset() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        assert!(matches!(classify(None), EnvValue::Unset));
        assert!(matches!(
            classify(Some(OsString::from(""))),
            EnvValue::Unset
        ));
        assert!(matches!(
            classify(Some(OsString::from("1800"))),
            EnvValue::Text(_)
        ));
        assert!(matches!(
            classify(Some(OsString::from_vec(vec![0xff, 0xfe]))),
            EnvValue::NonUnicode
        ));
    }
}

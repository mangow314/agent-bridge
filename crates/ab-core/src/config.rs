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

#[cfg(test)]
mod tests {
    use super::*;

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

/// now_iso:144 — 顯式 UTC ISO 8601（`YYYY-MM-DDTHH:MM:SSZ`），不受 locale/TZ
/// 影響（架構 §5「繼承環境」與 §1 std-only 的交集：不引入 chrono，自行以
/// Howard Hinnant 的 civil_from_days 演算法換算曆法）。
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_epoch(now.as_secs())
}

/// 供未來 TTL／epoch 比對重用（state.md STATE-CHAN-2 的新鮮度判定）。
pub fn format_epoch(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let secs_of_day = epoch_secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// http://howardhinnant.github.io/date_algorithms.html#civil_from_days
/// 輸入：1970-01-01 起算的天數（可為負）；輸出：(年, 月, 日) proleptic
/// Gregorian civil calendar，UTC 定義下與 `date -u` 一致。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch_matches_utc() {
        // 2026-07-31T00:00:00Z 的 epoch 秒數（以 `date -ud` 驗證）
        assert_eq!(format_epoch(1_785_456_000), "2026-07-31T00:00:00Z");
        // Unix epoch 起點
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        // 世紀交界＋閏年邊界
        assert_eq!(format_epoch(951_782_400), "2000-02-29T00:00:00Z");
    }
}

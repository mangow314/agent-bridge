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

/// 現在的 UNIX epoch 秒（TTL 新鮮度比對用，對映 bash `date -u +%s`）。
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// task-id 前綴用的緊湊時戳（對映 bash `date -u +%Y%m%dT%H%M%SZ`）。
pub fn now_compact() -> String {
    format_epoch_compact(now_epoch().max(0) as u64)
}

pub fn format_epoch_compact(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let secs_of_day = epoch_secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// 解析自家寫出的定長 ISO 8601 UTC 時戳（`YYYY-MM-DDTHH:MM:SSZ`）成 epoch 秒。
/// 對映 bash `date -ud "$ts" +%s`；bash 的 `date` 接受的格式更寬鬆，但 state／
/// metadata 的時戳都由本工具自己以 `now_iso` 寫出，形狀固定。解析失敗回
/// `None`，呼叫端一律當「無狀態」處理（notify_or_defer:389-391 的失效方向：
/// 寧可多送一次鍵，不可因壞時戳把通知永久停掉）。
pub fn parse_iso_to_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s[from..to].parse::<i64>().ok() };
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let hh = num(11, 13)?;
    let mi = num(14, 16)?;
    let ss = num(17, 19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || hh > 23 || mi > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    // 曆日必須真實存在：`1..=31` 的粗檢會放行 2/31、4/31 這類日期，而
    // `days_from_civil` 對它們照樣算得出天數（溢到次月）。GNU `date -ud`
    // 對這些輸入是 rc=1，本函式必須同樣回 None——`gc` 靠「判不出年紀就保留」
    // 護住損壞 metadata，靜默換算會讓那些目錄反而進入刪除候選。
    // 用逆運算 round-trip 檢查，不另寫一份月長／閏年表。
    if civil_from_days(days) != (y, mo as u32, d as u32) {
        return None;
    }
    Some(days * 86400 + hh * 3600 + mi * 60 + ss)
}

/// http://howardhinnant.github.io/date_algorithms.html#days_from_civil
/// civil_from_days 的逆運算：(年, 月, 日) → 1970-01-01 起算天數。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe as i64 - 719468
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

    /// ISO 解析與格式化互為逆運算；壞形狀一律 `None`（不得矇混成某個時間）。
    #[test]
    fn iso_parse_round_trips_and_rejects_junk() {
        for e in [0u64, 951_782_400, 1_785_456_000, 1_785_512_727] {
            let iso = format_epoch(e);
            assert_eq!(
                parse_iso_to_epoch(&iso),
                Some(e as i64),
                "來回不一致：{iso}"
            );
        }
        assert_eq!(parse_iso_to_epoch(""), None);
        assert_eq!(parse_iso_to_epoch("not-a-timestamp"), None);
        assert_eq!(parse_iso_to_epoch("2026-07-31T00:00:00"), None); // 缺 Z
        assert_eq!(parse_iso_to_epoch("2026-13-31T00:00:00Z"), None); // 月份越界
    }

    /// 不存在的曆日 MUST 回 `None`（GNU `date -ud` 對這些是 rc=1）。
    /// 靜默換算會讓 gc 把「判不出年紀、該保留」的損壞 metadata 送進刪除候選。
    #[test]
    fn impossible_calendar_dates_are_rejected() {
        for bad in [
            "2026-02-31T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-02-29T00:00:00Z", // 2026 非閏年
            "2026-04-31T00:00:00Z",
            "2026-06-31T00:00:00Z",
            "2026-09-31T00:00:00Z",
            "2026-11-31T00:00:00Z",
            "2026-01-00T00:00:00Z",
        ] {
            assert_eq!(parse_iso_to_epoch(bad), None, "不該被接受：{bad}");
        }
        // 真實存在的閏日必須通過
        assert!(parse_iso_to_epoch("2024-02-29T00:00:00Z").is_some());
        assert!(parse_iso_to_epoch("2000-02-29T00:00:00Z").is_some()); // 400 年閏
        assert_eq!(parse_iso_to_epoch("1900-02-29T00:00:00Z"), None); // 100 年不閏
        assert!(parse_iso_to_epoch("2026-01-31T00:00:00Z").is_some());
    }

    #[test]
    fn compact_form_matches_task_id_prefix_shape() {
        assert_eq!(format_epoch_compact(1_785_456_000), "20260731T000000Z");
    }

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

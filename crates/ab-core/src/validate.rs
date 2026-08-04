/// NAME_RE：agent／sender 名稱文法 `^[A-Za-z0-9_-]+$`（STATE-GEN-3、CLI-GEN-3）。
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// TASK_ID_RE：`^[A-Za-z0-9][A-Za-z0-9._-]*$`（CLI-GEN-3）。首字元強制英數，
/// 擋掉 dotfile 形（`.`/`..`）與被當旗標的 `-` 開頭形。
pub fn is_valid_task_id(s: &str) -> bool {
    let mut bytes = s.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rejects_space_and_empty() {
        assert!(is_valid_name("alice"));
        assert!(is_valid_name("alice_2-b"));
        assert!(!is_valid_name("bad name"));
        assert!(!is_valid_name(""));
    }

    #[test]
    fn task_id_rejects_dot_leading() {
        assert!(is_valid_task_id("20260731T000000Z-ab12"));
        assert!(!is_valid_task_id(".hidden"));
        assert!(!is_valid_task_id("-flag"));
        assert!(!is_valid_task_id(""));
    }
}

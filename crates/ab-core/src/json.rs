//! 手寫最小 JSON 讀寫（架構 §7：不遷就 serde 預設，序列化形狀以 jq fixture
//! 對拍為準）。讀取端服務 `registry::read_provenance` 等三態判定
//! （state.md STATE-AGENT-2：非 object／解析失敗都要能分辨出來，不能只認
//! 「成功」與「失敗」兩態）；寫入端只服務目前已知的扁平物件形狀
//! （`agents/<name>.json`），欄位序＝插入序，對齊 jq 的 insertion order。

use std::iter::Peekable;
use std::str::CharIndices;

/// 通用 JSON 值：足夠涵蓋 registry／未來 metadata 讀取需要的形狀，不是一個
/// 完整規格認證的 parser（例如不做 UTF-16 surrogate pair 配對），但對本專案
/// 自己寫出的 JSON 與常見手工 JSON 輸入已足夠健壯。
#[derive(Debug, Clone, PartialEq)]
pub enum JVal {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<JVal>),
    Object(Vec<(String, JVal)>),
}

pub fn parse(input: &str) -> Result<JVal, String> {
    let mut p = Parser {
        chars: input.char_indices().peekable(),
        src: input,
    };
    let v = p.parse_value()?;
    p.skip_ws();
    if p.chars.peek().is_some() {
        return Err("多餘的尾隨內容".into());
    }
    Ok(v)
}

/// 讀取 object 內某個字串欄位；型別不符或找不到都回 `None`（呼叫端自行決定
/// fail-open 或 fail-closed，本函式不預設立場）。**重複鍵取最後一個值**
/// （`.iter().rev().find()`）：jq／標準 JSON 物件語意是後出現的鍵覆蓋前面
/// 的同名鍵（RFC 8259 §4 對重複鍵行為不強制，但 jq 的實作是後者為準），
/// 用 `find`（取第一個）會與 jq 的判斷結果不一致。
pub fn str_field<'a>(fields: &'a [(String, JVal)], key: &str) -> Option<&'a str> {
    fields.iter().rev().find(|(k, _)| k == key).and_then(|(_, v)| match v {
        JVal::Str(s) => Some(s.as_str()),
        _ => None,
    })
}

/// 讀取 object 內某個布林欄位是否「明確為 true」；缺欄位／型別不符都回
/// `false`（對映 `jq -e '.spawned == true'` 的比較語意——`null`、字串、數字
/// 一律不算 true）。**重複鍵取最後一個值**，理由同 `str_field`——
/// `{"spawned": false, "spawned": true}` 這類輸入 jq 判定為 `true`，若取第一
/// 個會誤判為 `false`，等於把「出身不明／已 spawn」誤判成「人工註冊」，
/// 破壞 STATE-AGENT-2 的 fail-closed 前提。
pub fn bool_field_is_true(fields: &[(String, JVal)], key: &str) -> bool {
    matches!(
        fields.iter().rev().find(|(k, _)| k == key),
        Some((_, JVal::Bool(true)))
    )
}

struct Parser<'a> {
    chars: Peekable<CharIndices<'a>>,
    src: &'a str,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&(_, c)) = self.chars.peek() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    fn parse_value(&mut self) -> Result<JVal, String> {
        self.skip_ws();
        match self.peek_char() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => self.parse_string().map(JVal::Str),
            Some('t') => self.parse_lit("true", JVal::Bool(true)),
            Some('f') => self.parse_lit("false", JVal::Bool(false)),
            Some('n') => self.parse_lit("null", JVal::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err("預期一個 JSON 值".into()),
        }
    }

    fn parse_lit(&mut self, lit: &str, val: JVal) -> Result<JVal, String> {
        for expect in lit.chars() {
            match self.chars.next() {
                Some((_, c)) if c == expect => {}
                _ => return Err(format!("預期字面值 {lit}")),
            }
        }
        Ok(val)
    }

    fn parse_object(&mut self) -> Result<JVal, String> {
        self.chars.next(); // consume '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek_char() == Some('}') {
            self.chars.next();
            return Ok(JVal::Object(fields));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            match self.chars.next() {
                Some((_, ':')) => {}
                _ => return Err("預期 ':'".into()),
            }
            let val = self.parse_value()?;
            fields.push((key, val));
            self.skip_ws();
            match self.chars.next() {
                Some((_, ',')) => continue,
                Some((_, '}')) => break,
                _ => return Err("預期 ',' 或 '}'".into()),
            }
        }
        Ok(JVal::Object(fields))
    }

    fn parse_array(&mut self) -> Result<JVal, String> {
        self.chars.next(); // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek_char() == Some(']') {
            self.chars.next();
            return Ok(JVal::Array(items));
        }
        loop {
            let val = self.parse_value()?;
            items.push(val);
            self.skip_ws();
            match self.chars.next() {
                Some((_, ',')) => continue,
                Some((_, ']')) => break,
                _ => return Err("預期 ',' 或 ']'".into()),
            }
        }
        Ok(JVal::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        match self.chars.next() {
            Some((_, '"')) => {}
            _ => return Err("預期字串".into()),
        }
        let mut s = String::new();
        loop {
            match self.chars.next() {
                Some((_, '"')) => break,
                Some((_, '\\')) => match self.chars.next() {
                    Some((_, '"')) => s.push('"'),
                    Some((_, '\\')) => s.push('\\'),
                    Some((_, '/')) => s.push('/'),
                    Some((_, 'n')) => s.push('\n'),
                    Some((_, 't')) => s.push('\t'),
                    Some((_, 'r')) => s.push('\r'),
                    Some((_, 'b')) => s.push('\u{8}'),
                    Some((_, 'f')) => s.push('\u{c}'),
                    Some((_, 'u')) => {
                        let cp = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            // 高代理：必須緊接一個 \uDC00-\uDFFF 低代理才能
                            // 組成合法的 UTF-16 代理對（jq／RFC 8259 都拒收
                            // 落單的高代理，不得靜默替換成 U+FFFD）。
                            match (self.chars.next(), self.chars.next()) {
                                (Some((_, '\\')), Some((_, 'u'))) => {
                                    let low = self.parse_hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&low) {
                                        return Err("高代理未搭配合法的低代理".into());
                                    }
                                    let combined =
                                        0x10000 + (cp - 0xD800) * 0x400 + (low - 0xDC00);
                                    s.push(
                                        char::from_u32(combined)
                                            .ok_or("代理對組成不合法的 code point")?,
                                    );
                                }
                                _ => return Err("高代理未搭配低代理".into()),
                            }
                        } else if (0xDC00..=0xDFFF).contains(&cp) {
                            // 落單的低代理：沒有前導高代理，同樣不合法。
                            return Err("未配對的低代理".into());
                        } else {
                            s.push(char::from_u32(cp).ok_or("不合法的 code point")?);
                        }
                    }
                    _ => return Err("不合法的跳脫序列".into()),
                },
                // 字串內容中「未跳脫」的控制字元（< 0x20）MUST 拒收
                // （RFC 8259 §7／jq 行為一致）：合法輸入只能以 `\n`／`\t`／
                // `` 等跳脫序列表達控制字元，裸控制位元組視為損壞輸入。
                Some((_, c)) if (c as u32) < 0x20 => {
                    return Err(format!("字串內含未跳脫的控制字元：\\u{:04x}", c as u32));
                }
                Some((_, c)) => s.push(c),
                None => return Err("未結束的字串".into()),
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.chars.next().map(|(_, c)| c).ok_or("不完整的 \\u 跳脫")?;
            let d = c.to_digit(16).ok_or("不合法的 16 進位數字")?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<JVal, String> {
        let start = self.chars.peek().map(|&(i, _)| i).unwrap_or(self.src.len());
        if self.peek_char() == Some('-') {
            self.chars.next();
        }
        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
            self.chars.next();
        }
        if self.peek_char() == Some('.') {
            self.chars.next();
            while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                self.chars.next();
            }
        }
        if matches!(self.peek_char(), Some('e') | Some('E')) {
            self.chars.next();
            if matches!(self.peek_char(), Some('+') | Some('-')) {
                self.chars.next();
            }
            while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                self.chars.next();
            }
        }
        let end = self.chars.peek().map(|&(i, _)| i).unwrap_or(self.src.len());
        self.src[start..end]
            .parse::<f64>()
            .map(JVal::Number)
            .map_err(|e| e.to_string())
    }
}

/// 手寫 JSON 物件 builder：對齊 jq 預設 pretty-print（2-space 縮排、
/// `"key": value`、逐欄位換行、結尾 `}` 無縮排、無結尾換行——換行由呼叫端
/// atomic_write 補上一次，比照 bash `printf '%s\n' "$doc" | atomic_write`）。
pub struct JsonObject {
    fields: Vec<(String, JsonScalar)>,
}

enum JsonScalar {
    Str(String),
    Bool(bool),
}

impl JsonObject {
    pub fn new() -> Self {
        JsonObject { fields: Vec::new() }
    }

    pub fn push_str(mut self, key: &str, val: &str) -> Self {
        self.fields.push((key.to_string(), JsonScalar::Str(val.to_string())));
        self
    }

    pub fn push_bool(mut self, key: &str, val: bool) -> Self {
        self.fields.push((key.to_string(), JsonScalar::Bool(val)));
        self
    }

    pub fn render(&self) -> String {
        if self.fields.is_empty() {
            return "{}".to_string();
        }
        let mut out = String::from("{\n");
        for (i, (k, v)) in self.fields.iter().enumerate() {
            out.push_str("  ");
            out.push_str(&render_str(k));
            out.push_str(": ");
            match v {
                JsonScalar::Str(s) => out.push_str(&render_str(s)),
                JsonScalar::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            }
            if i + 1 < self.fields.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push('}');
        out
    }
}

impl Default for JsonObject {
    fn default() -> Self {
        Self::new()
    }
}

fn render_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_flat_object() {
        let src = r#"{"name": "alice", "pane_id": "%0", "registered_at": "2026-07-31T00:00:00Z"}"#;
        let v = parse(src).unwrap();
        match v {
            JVal::Object(fields) => {
                assert_eq!(str_field(&fields, "name"), Some("alice"));
                assert_eq!(str_field(&fields, "pane_id"), Some("%0"));
            }
            _ => panic!("預期 Object"),
        }
    }

    #[test]
    fn non_object_root_is_distinguishable() {
        assert!(matches!(parse("null").unwrap(), JVal::Null));
        assert!(matches!(parse("[1,2]").unwrap(), JVal::Array(_)));
        assert!(parse("{not json").is_err());
    }

    #[test]
    fn writer_matches_jq_pretty_print_shape() {
        let doc = JsonObject::new()
            .push_str("name", "alice")
            .push_str("pane_id", "%0")
            .push_str("registered_at", "2026-07-31T00:00:00Z");
        assert_eq!(
            doc.render(),
            "{\n  \"name\": \"alice\",\n  \"pane_id\": \"%0\",\n  \"registered_at\": \"2026-07-31T00:00:00Z\"\n}"
        );
    }

    #[test]
    fn spawned_bool_field_detection() {
        let v = parse(r#"{"spawned": true}"#).unwrap();
        if let JVal::Object(fields) = v {
            assert!(bool_field_is_true(&fields, "spawned"));
        } else {
            panic!("預期 Object");
        }
        let v = parse(r#"{"spawned": false}"#).unwrap();
        if let JVal::Object(fields) = v {
            assert!(!bool_field_is_true(&fields, "spawned"));
        } else {
            panic!("預期 Object");
        }
    }

    /// 2a：重複鍵取最後一個值（jq 語意）。`{"spawned": false, "spawned": true}`
    /// bash／jq 判定為 spawned；取第一個會誤判成 manual，破壞 fail-closed。
    #[test]
    fn duplicate_key_takes_last_value() {
        let v = parse(r#"{"spawned": false, "spawned": true}"#).unwrap();
        if let JVal::Object(fields) = v {
            assert!(bool_field_is_true(&fields, "spawned"));
        } else {
            panic!("預期 Object");
        }

        let v = parse(r#"{"name": "first", "name": "second"}"#).unwrap();
        if let JVal::Object(fields) = v {
            assert_eq!(str_field(&fields, "name"), Some("second"));
        } else {
            panic!("預期 Object");
        }
    }

    /// 2b：字串內未跳脫的控制字元（< 0x20）MUST 被拒收，不能靜默通過。
    #[test]
    fn raw_control_char_in_string_is_rejected() {
        let src = "{\"name\": \"a\u{0}b\"}"; // runtime 為原始 NUL：JSON 層未跳脫的控制字元
        assert!(parse(src).is_err());
    }

    /// 2c：`\uD800`-`\uDBFF` 高代理必須與緊接的低代理配對才能組成合法
    /// code point；落單的高代理／低代理都 MUST 回傳解析錯誤。
    #[test]
    fn lone_surrogate_is_rejected_but_pair_combines() {
        // 落單高代理（沒有後續 \u 低代理）
        assert!(parse(r#"{"x": "\uD800"}"#).is_err());
        // 落單低代理
        assert!(parse(r#"{"x": "\uDC00"}"#).is_err());
        // 合法代理對：U+1F600 拆成高代理 0xD83D ＋ 低代理 0xDE00。用 format!
        // 動態組出跳脫序列文字，避免原始碼裡直接嵌入逐字 emoji 字元。
        let high: u32 = 0xD83D;
        let low: u32 = 0xDE00;
        let src = format!("{{\"x\": \"\\u{high:04X}\\u{low:04X}\"}}");
        let v = parse(&src).unwrap();
        if let JVal::Object(fields) = v {
            assert_eq!(str_field(&fields, "x"), Some("\u{1F600}"));
        } else {
            panic!("預期 Object");
        }
    }
}

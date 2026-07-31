//! JSON 讀寫。**解析端走 `serde_json`**（`preserve_order`：欄位序＝插入序，
//! 對齊 jq 的 insertion order）；**輸出形狀由本模組的 `render_pretty` 掌控**，
//! 不遷就任何 pretty printer 的預設——序列化形狀以 jq fixture 對拍為準
//! （架構 §7）。
//!
//! 讀取端服務 `registry::read_provenance` 等三態判定（state.md STATE-AGENT-2：
//! 非 object／解析失敗都要能分辨出來，不能只認「成功」與「失敗」兩態）。
//!
//! 沿革：M0.5 spike 期為手寫 parser，M1 依使用者裁決（2026-07-31）改
//! serde_json；當時的邊界測試（重複鍵、裸控制字元、落單 surrogate、非 object
//! 根）原樣留在本檔尾，改為斷言 **serde_json 的**行為——它們守的是「本專案
//! 依賴的 JSON 語意」，不是某一個實作。

use serde_json::{Map, Value};

/// 解析成 `serde_json::Value`；錯誤訊息只給人看，呼叫端不對其文字做判斷。
pub fn parse(input: &str) -> Result<Value, String> {
    serde_json::from_str(input).map_err(|e| e.to_string())
}

/// 讀取 object 內某個字串欄位；型別不符或找不到都回 `None`（呼叫端自行決定
/// fail-open 或 fail-closed，本函式不預設立場）。
///
/// 重複鍵無須在此特別處理：`serde_json` 反序列化時後出現的鍵會覆蓋前者
/// （與 jq 一致；RFC 8259 §4 對重複鍵不強制，但 jq 的實作是後者為準），
/// 到這裡已經只剩一個值——`duplicate_key_takes_last_value` 測試守著這點。
pub fn str_field<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    fields.get(key).and_then(|v| v.as_str())
}

/// `jq -r '.<key> // empty'` 的逐字語意，供 hook 那組「bash 拿 jq 讀什麼、
/// Rust 就得拿到什麼」的欄位使用（codex 複核 2026-07-31 的 parity finding）。
///
/// 與 `str_field` 的差別是**非字串值不回 `None`**：`jq -r` 會把它們印成文字，
/// bash 端於是拿到一個非空字串並據以判斷。兩者混用正是那輪抓到的洞——例如
/// state 檔的 `owner: 1`，bash 視為「有主」而擋下異主寫入，`str_field` 卻當
/// 成「無主」直接放行。
///
/// 對映規則（`//` 的 alternative operator 對 `null` **與 `false`** 都觸發）：
/// - 缺欄位／`null`／`false`／空字串 → `None`
/// - 字串 → 原樣
/// - `true`／數字 → 其 JSON 文字
/// - 陣列／物件 → `render_pretty`（jq 預設輸出即 pretty，非 compact）
pub fn jq_raw_field(fields: &Map<String, Value>, key: &str) -> Option<String> {
    let out = match fields.get(key)? {
        Value::Null | Value::Bool(false) => return None,
        Value::String(s) => s.clone(),
        Value::Bool(true) => "true".to_string(),
        Value::Number(n) => n.to_string(),
        v => render_pretty(v),
    };
    if out.is_empty() { None } else { Some(out) }
}

/// jq alternative operator `.<key> // <default>` 的**逐字**語意：只有缺欄位、
/// `null`、`false` 會落到 default，**空字串是 truthy、原樣回傳**。
///
/// 與 `jq_raw_field` 的分工：後者模擬的是 bash 慣用的 `.x // empty` 再配
/// `[[ -n ]]` 判斷（M2 的 hook 欄位全走那個形狀），把空字串併進「沒有值」是
/// 對的；但 `.spawned_at // .registered_at`、`.disposable_at // "?"` 這類
/// **鏈式 fallback** 不同——欄位是空字串時 bash 拿到空字串就停在那裡，
/// 併進 None 會讓它繼續往後掉，於是 idle 對一份 `spawned_at: ""` 的 registry
/// 印出 registered_at 算出來的秒數，bash 印 `-`（codex 複核 2026-07-31）。
pub fn jq_alt(fields: &Map<String, Value>, key: &str) -> Option<String> {
    match fields.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Bool(true)) => Some("true".to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(v) => Some(render_pretty(v)),
    }
}

/// 讀取 object 內某個布林欄位是否「明確為 true」；缺欄位／型別不符都回
/// `false`（對映 `jq -e '.spawned == true'` 的比較語意——`null`、字串、數字
/// 一律不算 true）。
pub fn bool_field_is_true(fields: &Map<String, Value>, key: &str) -> bool {
    matches!(fields.get(key), Some(Value::Bool(true)))
}

/// 就地設定 object 的字串欄位：**已存在的鍵保持原位改值**（jq `.status = $s`
/// 的語意——賦值不會把欄位搬到尾端），不存在才 append。`update_meta_status`
/// 的 metadata.json 欄位序 parity 靠這個。`preserve_order` 下的 `Map` 即
/// `IndexMap`，`insert` 對既有鍵保留原位置，正是所需語意。
pub fn set_str_field(fields: &mut Map<String, Value>, key: &str, val: &str) {
    fields.insert(key.to_string(), Value::String(val.to_string()));
}

/// jq 預設 pretty-print 形狀：2-space 縮排、`"key": value`、逐欄位換行、
/// 空物件／空陣列印成 `{}`／`[]`、結尾無換行（換行由呼叫端補一次，比照
/// bash `printf '%s\n' "$doc" | atomic_write`）。
///
/// 不直接用 `serde_json::to_string_pretty`：那是別人的預設，隨版本可變，而
/// 本專案要對齊的是 jq 的輸出形狀。自家 render 讓「形狀」成為本 repo 測得到、
/// 改得動的東西（jq fixture 對拍是 gate）。
pub fn render_pretty(v: &Value) -> String {
    let mut out = String::new();
    render_at(v, 0, &mut out);
    out
}

fn render_at(v: &Value, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let inner_pad = "  ".repeat(depth + 1);
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&render_str(s)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&inner_pad);
                render_at(item, depth + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push(']');
        }
        Value::Object(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let n = fields.len();
            for (i, (k, val)) in fields.iter().enumerate() {
                out.push_str(&inner_pad);
                out.push_str(&render_str(k));
                out.push_str(": ");
                render_at(val, depth + 1, out);
                if i + 1 < n {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push('}');
        }
    }
}

/// 字串跳脫比照 jq：只跳脫 JSON 必需的字元（`"`、`\`、控制字元），
/// **非 ASCII 原樣輸出**（jq 預設不做 `\u` 跳脫，除非 `-a`）。
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
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 建構扁平字串／布林物件（agent registry 檔的形狀）。欄位序＝插入序。
pub struct JsonObject {
    fields: Map<String, Value>,
}

impl JsonObject {
    pub fn new() -> Self {
        JsonObject { fields: Map::new() }
    }

    pub fn push_str(mut self, key: &str, val: &str) -> Self {
        self.fields
            .insert(key.to_string(), Value::String(val.to_string()));
        self
    }

    pub fn push_bool(mut self, key: &str, val: bool) -> Self {
        self.fields.insert(key.to_string(), Value::Bool(val));
        self
    }

    pub fn render(&self) -> String {
        render_pretty(&Value::Object(self.fields.clone()))
    }
}

impl Default for JsonObject {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(src: &str) -> Map<String, Value> {
        match parse(src).unwrap() {
            Value::Object(m) => m,
            _ => panic!("預期 Object"),
        }
    }

    /// `//` 的 alternative operator **只對 `null`／`false` 生效**：空字串是
    /// truthy，`"" // "x"` 仍是 `""`。`jq_raw_field`（模擬 `// empty` ＋
    /// `[[ -n ]]`）與 `jq_alt`（模擬鏈式 fallback）在這一點上刻意分岔——
    /// idle 的 `.spawned_at // .registered_at` 走錯一邊就會多印一個秒數，
    /// bash 印 `-`（codex 複核 2026-07-31）。
    #[test]
    fn alt_operator_treats_empty_string_as_present() {
        let f = obj(r#"{"a":"","b":null,"c":false,"d":"x","e":3}"#);
        assert_eq!(jq_alt(&f, "a").as_deref(), Some(""));
        assert_eq!(jq_alt(&f, "b"), None);
        assert_eq!(jq_alt(&f, "c"), None);
        assert_eq!(jq_alt(&f, "missing"), None);
        assert_eq!(jq_alt(&f, "d").as_deref(), Some("x"));
        assert_eq!(jq_alt(&f, "e").as_deref(), Some("3"));
        // 對照組：`// empty` ＋ 非空判斷的形狀把空字串併進「沒有值」
        assert_eq!(jq_raw_field(&f, "a"), None);
    }

    #[test]
    fn round_trip_flat_object() {
        let fields =
            obj(r#"{"name": "alice", "pane_id": "%0", "registered_at": "2026-07-31T00:00:00Z"}"#);
        assert_eq!(str_field(&fields, "name"), Some("alice"));
        assert_eq!(str_field(&fields, "pane_id"), Some("%0"));
    }

    #[test]
    fn non_object_root_is_distinguishable() {
        assert!(matches!(parse("null").unwrap(), Value::Null));
        assert!(matches!(parse("[1,2]").unwrap(), Value::Array(_)));
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

    /// 巢狀與空容器形狀（jq：空物件／空陣列印成單行 `{}`／`[]`）。
    #[test]
    fn nested_and_empty_containers_match_jq() {
        let v = parse(r#"{"a": {"b": [1, 2]}, "c": {}, "d": []}"#).unwrap();
        assert_eq!(
            render_pretty(&v),
            "{\n  \"a\": {\n    \"b\": [\n      1,\n      2\n    ]\n  },\n  \"c\": {},\n  \"d\": []\n}"
        );
    }

    /// 整數不得印成 `1.0`（metadata 的 `version: 1`）；非 ASCII 原樣輸出
    /// （jq 預設不做 \u 跳脫）。
    #[test]
    fn number_and_unicode_shape() {
        let v = parse(r#"{"version": 1, "msg": "中文"}"#).unwrap();
        assert_eq!(
            render_pretty(&v),
            "{\n  \"version\": 1,\n  \"msg\": \"中文\"\n}"
        );
    }

    #[test]
    fn spawned_bool_field_detection() {
        assert!(bool_field_is_true(&obj(r#"{"spawned": true}"#), "spawned"));
        assert!(!bool_field_is_true(
            &obj(r#"{"spawned": false}"#),
            "spawned"
        ));
        assert!(!bool_field_is_true(
            &obj(r#"{"spawned": "true"}"#),
            "spawned"
        ));
        assert!(!bool_field_is_true(&obj(r#"{}"#), "spawned"));
    }

    /// 2a：重複鍵取最後一個值（jq 語意）。`{"spawned": false, "spawned": true}`
    /// bash／jq 判定為 spawned；取第一個會誤判成 manual，破壞 fail-closed。
    /// 換 serde_json 後這是**依賴方的行為**，故留作回歸測試。
    /// `jq -r '.x // empty'` 的對照表。`str_field` 對非字串一律 `None`，
    /// 這裡不是——差別本身就是那輪 parity finding 的內容。
    #[test]
    fn jq_raw_field_matches_jq_r_alternative_semantics() {
        let Ok(Value::Object(m)) = parse(
            r#"{"s":"x","empty":"","n":42,"f":1.5,"t":true,"no":false,
                "nil":null,"arr":[1],"obj":{"k":"v"}}"#,
        ) else {
            panic!("fixture 應為 object");
        };
        assert_eq!(jq_raw_field(&m, "s").as_deref(), Some("x"));
        assert_eq!(jq_raw_field(&m, "n").as_deref(), Some("42"));
        assert_eq!(jq_raw_field(&m, "f").as_deref(), Some("1.5"));
        assert_eq!(jq_raw_field(&m, "t").as_deref(), Some("true"));
        // `//` 對 null 與 false 都觸發 alternative → empty
        assert_eq!(jq_raw_field(&m, "no"), None);
        assert_eq!(jq_raw_field(&m, "nil"), None);
        // 空字串在 `// empty` 之後仍是空字串，bash 端 `[[ -n ]]` 判為無值
        assert_eq!(jq_raw_field(&m, "empty"), None);
        assert_eq!(jq_raw_field(&m, "missing"), None);
        // 複合值：jq 預設輸出即 pretty
        assert_eq!(jq_raw_field(&m, "arr").as_deref(), Some("[\n  1\n]"));
        assert_eq!(
            jq_raw_field(&m, "obj").as_deref(),
            Some("{\n  \"k\": \"v\"\n}")
        );
    }

    #[test]
    fn duplicate_key_takes_last_value() {
        assert!(bool_field_is_true(
            &obj(r#"{"spawned": false, "spawned": true}"#),
            "spawned"
        ));
        assert_eq!(
            str_field(&obj(r#"{"name": "first", "name": "second"}"#), "name"),
            Some("second")
        );
    }

    /// 2b：字串內未跳脫的控制字元（< 0x20）MUST 被拒收，不能靜默通過。
    #[test]
    fn raw_control_char_in_string_is_rejected() {
        let src = "{\"name\": \"a\u{0}b\"}"; // runtime 為原始 NUL：JSON 層未跳脫的控制字元
        assert!(parse(src).is_err());
    }

    /// 2c：`\uD800`-`\uDBFF` 高代理必須與緊接的低代理配對才能組成合法
    /// code point；落單的高代理／低代理都 MUST 回傳解析錯誤（不得靜默替換
    /// 成 U+FFFD）。
    #[test]
    fn lone_surrogate_is_rejected_but_pair_combines() {
        assert!(parse(r#"{"x": "\uD800"}"#).is_err());
        assert!(parse(r#"{"x": "\uDC00"}"#).is_err());
        // 合法代理對：U+1F600 拆成高代理 0xD83D ＋ 低代理 0xDE00。用 format!
        // 動態組出跳脫序列文字，避免原始碼裡直接嵌入逐字 emoji 字元。
        let high: u32 = 0xD83D;
        let low: u32 = 0xDE00;
        let src = format!("{{\"x\": \"\\u{high:04X}\\u{low:04X}\"}}");
        assert_eq!(str_field(&obj(&src), "x"), Some("\u{1F600}"));
    }

    /// 既有鍵改值 MUST 保持原位置（jq 賦值語意），新鍵才 append。
    #[test]
    fn set_str_field_keeps_position() {
        let mut fields = obj(r#"{"a": "1", "b": "2", "c": "3"}"#);
        set_str_field(&mut fields, "b", "changed");
        set_str_field(&mut fields, "d", "new");
        let keys: Vec<&str> = fields.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c", "d"]);
        assert_eq!(str_field(&fields, "b"), Some("changed"));
    }
}

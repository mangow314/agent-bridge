use std::fmt;

/// ab-core 全域錯誤：攜帶「使用者可見訊息」欄位，訊息文字逐字對齊 bash die
/// 的中文 stderr（架構 §4：parity gate 驗這個）。`ab` dispatch 層統一把
/// `Err` 印到 stderr 並轉 exit code；ab-core 內部只回傳 `Result`，不自行印字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

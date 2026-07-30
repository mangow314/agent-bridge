//! ab-core：領域邏輯庫（狀態、儲存、tmux client）。除 `serde_json`
//! （M1 起，架構 §1 已預留）外不引入外部依賴；
//! 地位是結構錨的實作面——行為疑義一律回 `spec/` 與 bash 正本
//! `bin/agent-bridge`。模組對映表見 `docs/rust/architecture.md` §2。
//!
//! M1 範圍：狀態機核心——registry CRUD、task 目錄與狀態機、gc、最小 tmux
//! 通知（notify_or_defer 的 TTL gate 含在內）。M2 補上 `hook`（Claude Code
//! hook 協定）與 `config`（`AGENT_BRIDGE_*` 集中讀取）；`spawn` 生命週期群
//! 留給 M3，見 architecture.md 的模組對映表。

pub mod config;
pub mod error;
pub mod fsio;
pub mod hook;
pub mod json;
pub mod lock;
pub mod notify;
pub mod paths;
pub mod registry;
pub mod task;
pub mod time;
pub mod tmux;
pub mod validate;

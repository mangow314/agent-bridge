//! ab-core：領域邏輯庫（狀態、儲存、tmux client）。std only（架構 §1）；
//! 地位是結構錨的實作面——行為疑義一律回 `spec/` 與 bash 正本
//! `bin/agent-bridge`。模組對映表見 `docs/rust/architecture.md` §2。
//!
//! M0.5 spike 範圍：只涵蓋 `register`／`list`／`send`（錯誤路徑）所需的必要
//! 子集（paths/fsio/lock/registry/tmux/json/time/validate/error）；
//! task/notify/hook/spawn 等模組留給後續階段，見 architecture.md 的模組對映表。

pub mod error;
pub mod fsio;
pub mod json;
pub mod lock;
pub mod paths;
pub mod registry;
pub mod time;
pub mod tmux;
pub mod validate;

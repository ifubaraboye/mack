//! Token totals for the settings Usage page.
//!
//! The page counts only turns executed inside Mack, captured live from each
//! provider stream. The CLI-transcript scanner that used to reconstruct
//! Claude Code / Codex usage from `~/.claude` and `~/.codex` is gone; this
//! module remains as the crate-internal re-export point for the totals
//! types so call sites keep a single import path.

pub use waku_protocol::usage_history::{MackUsageTotals, TokenTotals};

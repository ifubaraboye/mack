//! Token totals for the settings Usage page.
//!
//! The page counts only turns executed inside Mack, captured live from each
//! provider stream as `DriverEvent::TurnUsage` and summed over every stored
//! session. There is deliberately no transcript scanning here: turns driven
//! outside Mack (Claude Code, Codex CLI, …) are outside the page's scope.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub uncached_input: u64,
    pub cached_input: u64,
    pub cache_creation: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl TokenTotals {
    pub fn total(&self) -> u64 {
        self.uncached_input + self.cached_input + self.cache_creation + self.output
    }

    pub fn add(&mut self, other: &TokenTotals) {
        self.uncached_input += other.uncached_input;
        self.cached_input += other.cached_input;
        self.cache_creation += other.cache_creation;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
}

/// Lifetime token totals for turns executed inside Mack itself, summed over
/// every stored session.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MackUsageTotals {
    pub totals: TokenTotals,
    pub turns: u64,
    pub sessions: u64,
}

impl MackUsageTotals {
    pub fn total_tokens(&self) -> u64 {
        self.totals.total()
    }
}

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::computer_use::ComputerAppGrant;
use crate::model::ProviderKind;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(default)]
pub struct DaemonSettings {
    pub computer_use_enabled: bool,
    pub computer_use_allowed_apps: Vec<ComputerAppGrant>,
    pub disabled_providers: Vec<ProviderKind>,
    /// Cross-chat memory extraction and retrieval. Defaults on so existing
    /// behavior is unchanged; the struct-level `#[serde(default)]` keeps old
    /// settings files (which lack the key) parsing to enabled.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub provider_binary_overrides: HashMap<ProviderKind, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn default_memory_enabled() -> bool {
    true
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            computer_use_enabled: false,
            computer_use_allowed_apps: Vec::new(),
            disabled_providers: Vec::new(),
            memory_enabled: true,
            provider_binary_overrides: HashMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl DaemonSettings {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
            .join("settings.json")
    }

    pub fn discard_legacy_app_keys(&mut self) {
        for key in ["analytics_enabled", "favorite_models", "theme", "language"] {
            self.extra.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_enabled_by_default() {
        assert!(DaemonSettings::default().memory_enabled);
        // Settings files written before the key existed must parse to
        // enabled — otherwise the toggle would silently flip existing users
        // off and quarantine their file as corrupt.
        let old: DaemonSettings =
            serde_json::from_str(r#"{"computer_use_enabled":false,"disabled_providers":[]}"#)
                .unwrap();
        assert!(old.memory_enabled);
        let explicit: DaemonSettings = serde_json::from_str(r#"{"memory_enabled":false}"#).unwrap();
        assert!(!explicit.memory_enabled);
    }
}

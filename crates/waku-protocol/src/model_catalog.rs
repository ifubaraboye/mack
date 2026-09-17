//! Provider fallback choices used before daemon-side discovery completes.

use crate::model::{ProviderAgentPreset, ProviderKind, ProviderModel};

pub fn fallback_models(provider: ProviderKind) -> Vec<ProviderModel> {
    match provider {
        // ChatGPT models are account- and plan-specific and come only from
        // the authenticated `/models` endpoint. A fabricated fallback would
        // offer models the account cannot use, so discovery is authoritative.
        ProviderKind::ChatGpt => Vec::new(),
        // Same for Claude: the subscription catalog is authoritative. The
        // daemon substitutes its curated fallback only after an
        // authenticated discovery attempt, never here.
        ProviderKind::Claude => Vec::new(),
    }
}

pub fn fallback_agent_presets(_provider: ProviderKind) -> Vec<ProviderAgentPreset> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_fallback_catalog_is_empty() {
        // Models are account- and plan-specific and come only from the
        // authenticated `/models` endpoint, so discovery is authoritative
        // and the pre-discovery picker stays empty — never a hardcoded list.
        assert!(fallback_models(ProviderKind::ChatGpt).is_empty());
    }
}

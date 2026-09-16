//! Provider model and agent-preset discovery (ChatGPT-only).

use std::path::{Path, PathBuf};

use crate::model::{ProviderAgentPreset, ProviderKind, ProviderModel};

/// ChatGPT models are account- and plan-specific and come only from the
/// authenticated `/models` endpoint through the daemon-owned session. An
/// invented fallback would offer models the account cannot use, so discovery
/// is authoritative and pre-discovery is empty.
pub fn fallback_models(_provider: ProviderKind) -> Vec<ProviderModel> {
    Vec::new()
}

pub fn fallback_agent_presets(_provider: ProviderKind) -> Vec<ProviderAgentPreset> {
    Vec::new()
}

/// Discovers both ordinary models and provider-owned agent compositions.
///
/// ChatGPT discovery is session-authenticated, not CLI-based: the daemon
/// serves it through the ChatGPT session manager instead of this CLI probe
/// path, so direct calls stay empty (with cache fallback preserved).
pub fn discover_catalog(
    provider: ProviderKind,
    _binary: &Path,
) -> (Vec<ProviderModel>, Vec<ProviderAgentPreset>) {
    let discovered: Vec<ProviderModel> = Vec::new();
    let models = if discovered.is_empty() {
        cached_models(provider).unwrap_or_else(|| fallback_models(provider))
    } else {
        let models = deduplicate(discovered);
        write_cached_models(provider, &models);
        models
    };
    let presets = fallback_agent_presets(provider);
    (models, presets)
}

/// Where a provider's last discovered catalog is cached. Debug builds keep it
/// in the checkout's gitignored `temp/` beside the debug database, so
/// development never touches the installed app's cache.
fn model_cache_path(provider: ProviderKind) -> PathBuf {
    let directory = if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("temp")
            .join("model-cache")
    } else {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(crate::identity::DATA_DIRECTORY_NAME)
            .join("models")
    };
    directory.join(format!("{}.json", provider.id()))
}

/// The catalog cached by the last successful discovery, or `None` when no run
/// has cached one or the file no longer parses. Reads the filesystem, so call
/// it from the discovery thread, never from render.
pub fn cached_models(provider: ProviderKind) -> Option<Vec<ProviderModel>> {
    read_models_file(&model_cache_path(provider))
}

fn read_models_file(path: &Path) -> Option<Vec<ProviderModel>> {
    let contents = std::fs::read(path).ok()?;
    let models = serde_json::from_slice::<Vec<ProviderModel>>(&contents).ok()?;
    (!models.is_empty()).then_some(models)
}

/// Best-effort: a cache that fails to write only costs the next launch its
/// head start.
pub(crate) fn write_cached_models(provider: ProviderKind, models: &[ProviderModel]) {
    let _ = write_models_file(&model_cache_path(provider), models);
}

fn write_models_file(path: &Path, models: &[ProviderModel]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename so a crash mid-write can't leave a torn file for the
    // next launch to trip over.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(models)?)?;
    std::fs::rename(temporary, path)
}

pub(crate) fn display_name_from_slug(slug: &str) -> String {
    let words = slug
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| match part.to_ascii_lowercase().as_str() {
            "gpt" => "GPT".to_owned(),
            "ai" => "AI".to_owned(),
            "xai" => "xAI".to_owned(),
            _ if part
                .chars()
                .all(|char| char.is_ascii_digit() || char == '.') =>
            {
                part.to_owned()
            }
            _ => {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
            }
        })
        .collect::<Vec<_>>();
    if words.first().is_some_and(|word| word == "GPT") {
        words.join("-")
    } else {
        words.join(" ")
    }
}

fn deduplicate(models: Vec<ProviderModel>) -> Vec<ProviderModel> {
    let mut seen = std::collections::HashSet::new();
    models
        .into_iter()
        .filter(|model| seen.insert(model.id.clone()))
        .collect()
}

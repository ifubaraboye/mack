//! Provider-owned slash-command discovery (ChatGPT-only).

use std::path::Path;

use waku_protocol::composer::SlashCommand;
use waku_protocol::model::ProviderKind;

/// Discover the command surface the installed CLI can expose without creating
/// a provider session. ChatGPT reports no sessionless CLI catalog, so this
/// always returns `None`; filesystem-defined commands remain the fallback.
pub(crate) fn discover(
    _provider: ProviderKind,
    _binary: &Path,
    _project_root: &Path,
) -> Option<Vec<SlashCommand>> {
    None
}

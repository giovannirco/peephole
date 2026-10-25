//! Peephole plugins. [`build`] turns a config entry into a running plugin.
//! Implementation pending; the API below is the contract the hub builds against.

use std::sync::Arc;

use peephole_core::Plugin;
use peephole_core::config::PluginEntry;

/// Names of the plugin kinds this build knows.
pub fn kinds() -> &'static [&'static str] {
    &["clock", "demo", "prometheus", "http_json", "mempool", "unifi_protect"]
}

/// Instantiate the plugin for `entry.kind` with `entry.settings()`.
/// Unknown kinds and invalid settings are errors (the hub refuses to start).
pub fn build(entry: &PluginEntry) -> anyhow::Result<Arc<dyn Plugin>> {
    anyhow::bail!("plugin kind {:?} is not implemented yet", entry.kind)
}

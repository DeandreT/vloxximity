//! On-disk persistence for voice settings and per-account mutes.
//!
//! Files live in the Nexus-provided per-addon directory
//! (`nexus::paths::get_addon_dir("Vloxximity")`). Each call is a best-effort
//! I/O — errors are logged but never propagated, since persistence failures
//! should never block the voice pipeline.
//!
//! **Secrets** (the GW2 API key) live in the OS keyring, not in
//! `settings.json`. The `gw2_api_key` field on `VoiceSettings` is marked
//! `#[serde(skip)]` and the settings module shuttles it through the
//! keyring. Legacy plaintext keys discovered in old `settings.json` files
//! are migrated into the keyring on first load.

mod mutes;
mod settings;

use std::path::PathBuf;

const ADDON_NAME: &str = "Vloxximity";

pub use mutes::{load_muted_accounts, save_muted_accounts};
pub use settings::{load_settings, save_settings};

/// Returns the addon's per-addon data directory, creating it if needed.
pub fn addon_dir() -> Option<PathBuf> {
    let dir = nexus::paths::get_addon_dir(ADDON_NAME)?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Failed to create addon dir {}: {}", dir.display(), e);
        return None;
    }
    Some(dir)
}

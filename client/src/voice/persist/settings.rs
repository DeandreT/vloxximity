//! `settings.json` plus OS-keyring storage for the GW2 API key.
//!
//! On native Windows the API key lives in the OS Credential Manager. Under
//! Wine/Proton the in-prefix Credential Manager doesn't reliably persist
//! credentials across sessions, so we detect Wine at runtime and fall back
//! to a plaintext file in the addon dir.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::voice::manager::VoiceSettings;

const SETTINGS_FILE: &str = "settings.json";
const API_KEY_FILE: &str = "api_key.txt";
const KEYRING_SERVICE: &str = "Vloxximity";
const KEYRING_API_KEY: &str = "gw2-api-key";

/// Legacy schema for migrating plaintext keys out of `settings.json`. The
/// production `VoiceSettings` skips `gw2_api_key` from serde, so we can't
/// see a leftover field through it.
#[derive(Debug, Default, Deserialize)]
struct LegacySettings {
    #[serde(default)]
    gw2_api_key: String,
}

fn settings_path() -> Option<PathBuf> {
    Some(super::addon_dir()?.join(SETTINGS_FILE))
}

#[cfg(windows)]
fn api_key_file_path() -> Option<PathBuf> {
    Some(super::addon_dir()?.join(API_KEY_FILE))
}

/// True when this DLL is running inside a Wine / Proton prefix. Detected
/// via the Wine-specific `wine_get_version` export from ntdll, which the
/// real Microsoft ntdll never exports. Cached for the process lifetime.
#[cfg(windows)]
fn is_wine() -> bool {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};
    use std::sync::OnceLock;

    type HMODULE = *mut c_void;
    type FARPROC = *const c_void;

    extern "system" {
        fn GetModuleHandleA(name: *const c_char) -> HMODULE;
        fn GetProcAddress(module: HMODULE, name: *const c_char) -> FARPROC;
    }

    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| unsafe {
        let ntdll = match CString::new("ntdll.dll") {
            Ok(s) => s,
            Err(_) => return false,
        };
        let module = GetModuleHandleA(ntdll.as_ptr());
        if module.is_null() {
            return false;
        }
        let probe = match CString::new("wine_get_version") {
            Ok(s) => s,
            Err(_) => return false,
        };
        let detected = !GetProcAddress(module, probe.as_ptr()).is_null();
        if detected {
            log::info!(
                "Detected Wine/Proton — API key will persist to api_key.txt \
                 in the addon dir instead of the OS keyring"
            );
        }
        detected
    })
}

/// Load `VoiceSettings` from `settings.json`, falling back to `Default` on
/// any error (missing file, invalid JSON, I/O failure). The `gw2_api_key`
/// field is sourced from the OS keyring; if a legacy plaintext key is
/// present in the on-disk file, it is migrated into the keyring and wiped
/// from disk before we return.
pub fn load_settings() -> VoiceSettings {
    let Some(path) = settings_path() else {
        return VoiceSettings::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return VoiceSettings {
                gw2_api_key: read_api_key_from_keyring(),
                ..VoiceSettings::default()
            };
        }
        Err(e) => {
            log::warn!("Failed to read {}: {} — using defaults", path.display(), e);
            return VoiceSettings {
                gw2_api_key: read_api_key_from_keyring(),
                ..VoiceSettings::default()
            };
        }
    };

    let mut settings = match serde_json::from_str::<VoiceSettings>(&text) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Failed to parse {}: {} — using defaults", path.display(), e);
            VoiceSettings::default()
        }
    };
    log::info!("Loaded settings from {}", path.display());

    // Pull any legacy plaintext key out of the JSON.
    let legacy_key = serde_json::from_str::<LegacySettings>(&text)
        .map(|l| l.gw2_api_key)
        .unwrap_or_default();

    if !legacy_key.is_empty() {
        log::info!("Migrating GW2 API key from settings.json to OS keyring");
        write_api_key_to_keyring(&legacy_key);
        save_settings_inner(&settings, &path);
        settings.gw2_api_key = legacy_key;
    } else {
        settings.gw2_api_key = read_api_key_from_keyring();
    }

    settings
}

pub fn save_settings(settings: &VoiceSettings) {
    let Some(path) = settings_path() else {
        return;
    };
    save_settings_inner(settings, &path);
    if settings.gw2_api_key.is_empty() {
        delete_api_key_from_keyring();
    } else {
        write_api_key_to_keyring(&settings.gw2_api_key);
    }
}

fn save_settings_inner(settings: &VoiceSettings, path: &Path) {
    match serde_json::to_string_pretty(settings) {
        Ok(text) => {
            if let Err(e) = std::fs::write(path, text) {
                log::warn!("Failed to write {}: {}", path.display(), e);
            }
        }
        Err(e) => log::warn!("Failed to serialize settings: {}", e),
    }
}

#[cfg(windows)]
fn read_api_key_from_keyring() -> String {
    if is_wine() {
        return read_api_key_from_file();
    }
    match keyring::Entry::new(KEYRING_SERVICE, KEYRING_API_KEY) {
        Ok(entry) => match entry.get_password() {
            Ok(secret) => secret,
            Err(keyring::Error::NoEntry) => String::new(),
            Err(e) => {
                log::warn!("Failed to read API key from keyring: {}", e);
                String::new()
            }
        },
        Err(e) => {
            log::warn!("Failed to open keyring entry: {}", e);
            String::new()
        }
    }
}

#[cfg(windows)]
fn write_api_key_to_keyring(key: &str) {
    if is_wine() {
        write_api_key_to_file(key);
        return;
    }
    match keyring::Entry::new(KEYRING_SERVICE, KEYRING_API_KEY) {
        Ok(entry) => {
            if let Err(e) = entry.set_password(key) {
                log::warn!("Failed to write API key to keyring: {}", e);
            }
        }
        Err(e) => log::warn!("Failed to open keyring entry: {}", e),
    }
}

#[cfg(windows)]
fn delete_api_key_from_keyring() {
    if is_wine() {
        delete_api_key_from_file();
        return;
    }
    match keyring::Entry::new(KEYRING_SERVICE, KEYRING_API_KEY) {
        Ok(entry) => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => log::warn!("Failed to delete API key from keyring: {}", e),
        },
        Err(e) => log::warn!("Failed to open keyring entry: {}", e),
    }
}

/// Read the API key from the Wine fallback file. Used on Wine where the
/// in-prefix Credential Manager doesn't persist credentials across
/// process lifetimes. On real Windows this is never reached.
#[cfg(windows)]
fn read_api_key_from_file() -> String {
    let Some(path) = api_key_file_path() else {
        return String::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            log::warn!("Failed to read API key from {}: {}", path.display(), e);
            String::new()
        }
    }
}

#[cfg(windows)]
fn write_api_key_to_file(key: &str) {
    let Some(path) = api_key_file_path() else {
        return;
    };
    if let Err(e) = std::fs::write(&path, key) {
        log::warn!("Failed to write API key to {}: {}", path.display(), e);
    }
}

#[cfg(windows)]
fn delete_api_key_from_file() {
    let Some(path) = api_key_file_path() else {
        return;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("Failed to delete {}: {}", path.display(), e),
    }
}

// Non-Windows builds (host-side tests on Linux/macOS) have no keyring
// dependency. The functions become no-ops; the `gw2_api_key` field still
// exists in `VoiceSettings`, but it's not persisted anywhere.
#[cfg(not(windows))]
fn read_api_key_from_keyring() -> String {
    String::new()
}
#[cfg(not(windows))]
fn write_api_key_to_keyring(_key: &str) {}
#[cfg(not(windows))]
fn delete_api_key_from_keyring() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::manager::VoiceMode;
    use crate::voice::room_type::RoomType;

    fn roundtrip_settings(settings: &VoiceSettings) -> VoiceSettings {
        let text = serde_json::to_string(settings).expect("serialize");
        serde_json::from_str(&text).expect("deserialize")
    }

    #[test]
    fn settings_roundtrip_preserves_persistent_fields() {
        let mut original = VoiceSettings::default();
        original.mode = VoiceMode::VoiceActivity;
        original.ptt_key = 0x42;
        original.min_distance = 250.0;
        original.max_distance = 7500.0;
        original.input_volume = 0.8;
        original.output_volume = 0.6;
        original.directional_audio_enabled = false;
        original.spatial_3d_enabled = false;
        original.show_peer_markers = true;
        original.server_url = "wss://example.com/ws".to_string();
        original.gw2_api_key = "secret-key".to_string();
        // Session-only — should NOT survive the round trip.
        original.is_muted = true;
        original.is_deafened = true;

        let restored = roundtrip_settings(&original);
        assert_eq!(restored.mode, VoiceMode::VoiceActivity);
        assert_eq!(restored.ptt_key, 0x42);
        assert_eq!(restored.min_distance, 250.0);
        assert_eq!(restored.max_distance, 7500.0);
        assert_eq!(restored.input_volume, 0.8);
        assert_eq!(restored.output_volume, 0.6);
        assert!(!restored.directional_audio_enabled);
        assert!(!restored.spatial_3d_enabled);
        assert!(restored.show_peer_markers);
        assert_eq!(restored.server_url, "wss://example.com/ws");
        // gw2_api_key lives in the OS keyring, not in JSON. It must not
        // survive a serde round trip — that's how we keep it off disk.
        assert_eq!(restored.gw2_api_key, "");
        // Session-only fields reset to defaults.
        assert!(!restored.is_muted, "is_muted should not persist");
        assert!(!restored.is_deafened, "is_deafened should not persist");
    }

    #[test]
    fn serialized_settings_never_contain_api_key() {
        let mut s = VoiceSettings::default();
        s.gw2_api_key = "DO-NOT-LEAK-THIS-1234".to_string();
        let text = serde_json::to_string_pretty(&s).expect("serialize");
        assert!(
            !text.contains("DO-NOT-LEAK-THIS-1234"),
            "settings JSON must never contain the API key string; got: {}",
            text
        );
        assert!(
            !text.contains("gw2_api_key"),
            "settings JSON must not even mention the api_key field name; got: {}",
            text
        );
    }

    #[test]
    fn settings_accept_partial_json() {
        // Old/partial JSON should deserialize cleanly using defaults for
        // any fields we've added since.
        let json = r#"{ "server_url": "ws://legacy.example/ws" }"#;
        let parsed: VoiceSettings = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.server_url, "ws://legacy.example/ws");
        assert_eq!(parsed.min_distance, VoiceSettings::default().min_distance);
        // Newly added field falls back to its default when omitted.
        assert_eq!(parsed.room_type_volumes.get(RoomType::Map), 1.0);
    }

    #[test]
    fn settings_roundtrip_preserves_room_type_volumes() {
        let mut original = VoiceSettings::default();
        original.room_type_volumes.set(RoomType::Map, 0.4);
        original.room_type_volumes.set(RoomType::Squad, 1.2);
        original.room_type_volumes.set(RoomType::Party, 0.8);

        let restored = roundtrip_settings(&original);
        assert!((restored.room_type_volumes.get(RoomType::Map) - 0.4).abs() < 1e-6);
        assert!((restored.room_type_volumes.get(RoomType::Squad) - 1.2).abs() < 1e-6);
        assert!((restored.room_type_volumes.get(RoomType::Party) - 0.8).abs() < 1e-6);
    }
}

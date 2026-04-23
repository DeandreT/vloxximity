//! Vloxximity - GW2 Proximity Voice Chat Addon
//!
//! A Nexus addon that adds local & directional voice chat to Guild Wars 2.
//! Players using the addon can hear other addon users who are nearby on the
//! same map instance, with 3D spatial audio positioning.

pub mod audio;
pub mod network;
pub mod position;
pub mod ui;
pub mod voice;
mod nexus_logger;

use nexus::gui::{register_render, render, RenderType};
use nexus::imgui::{Condition, Ui, Window};
use nexus::keybind::{keybind_handler, register_keybind_with_string};
use nexus::log::{log, LogLevel};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use std::sync::Arc;

use ui::SettingsWindow;
use voice::VoiceManager;

/// Addon version
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Global addon state
static ADDON_STATE: OnceCell<AddonState> = OnceCell::new();

/// Addon state container
struct AddonState {
    voice_manager: Arc<RwLock<VoiceManager>>,
    settings_window: Arc<RwLock<SettingsWindow>>,
}

impl AddonState {
    fn new() -> anyhow::Result<Self> {
        let voice_manager = Arc::new(RwLock::new(VoiceManager::new(voice::DEFAULT_SERVER_URL)));
        let settings_window = Arc::new(RwLock::new(SettingsWindow::new()));

        Ok(Self {
            voice_manager,
            settings_window,
        })
    }

    fn get() -> Option<&'static AddonState> {
        ADDON_STATE.get()
    }
}

nexus::export! {
    name: "Vloxximity",
    signature: -0x564C4F58, // "VLOX" in hex, negated for unofficial addon
    load: addon_load,
    unload: addon_unload,
}

/// Called when addon is loaded
fn addon_load() {
    // Initialize logging to Nexus
    if let Err(e) = nexus_logger::init() {
        log::warn!("Failed to initialize Nexus logger: {}", e);
    }

    log(LogLevel::Info, "Vloxximity", format!("Vloxximity {} loading...", VERSION));

    // Initialize addon state
    match AddonState::new() {
        Ok(state) => {
            // Initialize voice manager
            {
                let mut vm = state.voice_manager.write();
                if let Err(e) = vm.init() {
                    log(LogLevel::Warning, "Vloxximity", format!("Failed to initialize voice manager: {}", e));
                }
            }

            // Store state
            let _ = ADDON_STATE.set(state);

            // Register render callbacks
            register_render(RenderType::Render, render!(render_main)).revert_on_unload();
            register_render(RenderType::OptionsRender, render!(render_options)).revert_on_unload();

            // Register keybinds
            let ptt_handler = keybind_handler!(handle_ptt);
            register_keybind_with_string("Push To Talk", ptt_handler, "")
                .revert_on_unload();

            let toggle_handler = keybind_handler!(handle_toggle);
            register_keybind_with_string("Settings Window Toggle", toggle_handler, "")
                .revert_on_unload();

            log(LogLevel::Info, "Vloxximity", "Vloxximity loaded successfully");
        }
        Err(e) => {
            log(LogLevel::Critical, "Vloxximity", format!("Failed to initialize Vloxximity: {}", e));
        }
    }
}

/// Called when addon is unloaded
fn addon_unload() {
    log(LogLevel::Info, "Vloxximity", "Vloxximity unloading...");

    if let Some(state) = AddonState::get() {
        state.voice_manager.write().shutdown();
    }

    log(LogLevel::Info, "Vloxximity", "Vloxximity unloaded");
}

/// Main render callback (called each frame)
fn render_main(ui: &Ui) {
    let Some(state) = AddonState::get() else {
        return;
    };

    // Update voice manager
    {
        let mut vm = state.voice_manager.write();
        if let Err(e) = vm.update() {
            // Don't spam logs - use a static to rate limit
            static LAST_ERROR: OnceCell<std::time::Instant> = OnceCell::new();
            let now = std::time::Instant::now();
            if LAST_ERROR
                .get()
                .map(|t| now.duration_since(*t).as_secs() > 5)
                .unwrap_or(true)
            {
                log(LogLevel::Warning, "Vloxximity", format!("Voice update error: {}", e));
                let _ = LAST_ERROR.set(now);
            }
        }
    }

    // Render settings window and apply pending changes
    {
        let mut settings_window = state.settings_window.write();
        {
            let vm = state.voice_manager.read();
            settings_window.render(ui, &vm);
        }
        // Apply any pending changes (needs write lock)
        {
            let mut vm = state.voice_manager.write();
            settings_window.apply_pending(&mut vm);
        }
    }

    // Render speaking indicator overlay
    {
        let vm = state.voice_manager.read();
        render_speaking_indicator(ui, &vm);
        render_peer_markers(ui, &vm);
    }
}

fn render_peer_markers(ui: &Ui, voice_manager: &VoiceManager) {
    if !voice_manager.settings().show_peer_markers {
        return;
    }

    let Some(camera) = voice_manager.last_camera_transform() else {
        return;
    };
    // Vertical FOV in radians, fallback ~70° when the identity JSON hasn't
    // arrived yet (loading screens, zoning).
    let fov_v = voice_manager.last_fov().unwrap_or(1.222);

    let peers = voice_manager.get_peers();
    if peers.is_empty() {
        return;
    }

    let display = ui.io().display_size;
    let draw = ui.get_foreground_draw_list();

    for peer in peers {
        let Some([sx, sy]) = camera.world_to_screen(&peer.position, fov_v, display) else {
            continue;
        };
        let color = if peer.is_speaking {
            [0.2, 1.0, 0.2, 1.0]
        } else {
            [1.0, 0.85, 0.2, 1.0]
        };
        draw.add_circle([sx, sy], 8.0, color).thickness(2.0).build();
        draw.add_circle([sx, sy], 2.0, color).filled(true).build();
        draw.add_text([sx + 12.0, sy - 8.0], color, &peer.player_name);
    }
}

/// Render options in Nexus settings
fn render_options(ui: &Ui) {
    let Some(state) = AddonState::get() else {
        return;
    };

    ui.text("Vloxximity Voice Chat");
    ui.separator();

    let vm = state.voice_manager.read();
    let voice_state = vm.state();
    let status = match voice_state {
        voice::VoiceState::Disconnected => "Disconnected",
        voice::VoiceState::Connecting => "Connecting...",
        voice::VoiceState::Connected => "Connected",
        voice::VoiceState::InRoom => "In Room",
    };

    ui.text(format!("Status: {}", status));
    ui.text(format!("Peers: {}", vm.peer_count()));

    ui.separator();

    if ui.button("Open Settings") {
        drop(vm);
        state.settings_window.write().open();
    }
}

/// Render speaking indicator overlay
fn render_speaking_indicator(ui: &Ui, voice_manager: &VoiceManager) {
    let peers = voice_manager.get_peers();
    let speaking_peers: Vec<_> = peers.into_iter().filter(|p| p.is_speaking).collect();

    if speaking_peers.is_empty() {
        return;
    }

    // Draw speaking indicators in corner
    let display_size = ui.io().display_size;
    let window_pos = [display_size[0] - 210.0, 10.0];

    Window::new("##vloxximity_speaking")
        .position(window_pos, Condition::Always)
        .size([200.0, 100.0], Condition::FirstUseEver)
        .no_decoration()
        .always_auto_resize(true)
        .no_inputs()
        .bg_alpha(0.7)
        .build(ui, || {
            for peer in speaking_peers.iter().take(5) {
                ui.text(format!("[*] {}", peer.player_name));
            }

            if speaking_peers.len() > 5 {
                ui.text(format!("... and {} more", speaking_peers.len() - 5));
            }
        });
}

/// Handle PTT keybind
fn handle_ptt(_identifier: &str, is_release: bool) {
    if let Some(state) = AddonState::get() {
        // Press = talking, release = not talking
        state.voice_manager.read().set_ptt(!is_release);
    }
}

/// Handle toggle settings keybind
fn handle_toggle(_identifier: &str, is_release: bool) {
    if !is_release {
        return; // Only toggle on key press, not release
    }

    if let Some(state) = AddonState::get() {
        state.settings_window.write().toggle();
    }
}
